//! Unix ownership: observe without reaping, finish all group signals, then reap.
//!
//! `Child::try_wait` reaps on Unix, so it is never used by the normal lifecycle.
//! `waitid(WNOWAIT)` keeps the leader PID allocated until the sole owner reaps.
//! Sources: Rust 1.99 `std::process::Child`; Linux wait(2)/proc_pid_stat(5);
//! Apple XNU `bsd/kern/kern_exit.c` and `bsd/kern/proc_info.c`.
//! The embedding process must not reap this child elsewhere or install
//! SIGCHLD=SIG_IGN/SA_NOCLDWAIT while a run is active. Lost association is terminal.
//! Group observations are bounded snapshots, not a container for escaped children.

use crate::result::GroupState;
use std::io;
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::sync::OnceLock;
use std::sync::mpsc::{self, SyncSender};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {}

const MAX_GROUP_PIDS: usize = 65_536;
const REAPER_CAPACITY: usize = 128;
const REAPER_BUDGET: Duration = Duration::from_secs(5);
// Public Apple proc_info.h: the process is working its way through exit().
// https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/proc_info.h
#[cfg(target_os = "macos")]
const PROC_FLAG_INEXIT: u32 = 4;

pub(crate) struct ManagedChild {
    child: Option<Child>,
    pid: libc::pid_t,
    associated: bool,
    exit_observed: bool,
    reaped_status: Option<ExitStatus>,
}

impl ManagedChild {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        check_sigchld_disposition()?;
        command.process_group(0);
        let child = command.spawn()?;
        // Unix pid_t is positive and represented by the std Child's u32 PID.
        let pid = child.id() as libc::pid_t;
        Ok(Self {
            child: Some(child),
            pid,
            associated: true,
            exit_observed: false,
            reaped_status: None,
        })
    }

    pub(crate) fn id(&self) -> u32 {
        self.pid as u32
    }

    pub(crate) fn take_stdout(&mut self) -> Option<OwnedFd> {
        self.child.as_mut()?.stdout.take().map(Into::into)
    }

    pub(crate) fn take_stderr(&mut self) -> Option<OwnedFd> {
        self.child.as_mut()?.stderr.take().map(Into::into)
    }

    pub(crate) fn observe(&mut self) -> io::Result<bool> {
        if self.is_retired() {
            return Err(retired_error());
        }
        match observe_pid(self.pid) {
            Ok(exited) => {
                self.exit_observed |= exited;
                Ok(self.exit_observed)
            }
            Err(error) => {
                if error.raw_os_error() == Some(libc::ECHILD) {
                    // No later signal OR wait may use a PID whose ownership was lost.
                    self.associated = false;
                    self.child.take();
                }
                Err(error)
            }
        }
    }

    /// Send to the original group while the unreaped leader reserves its ID.
    /// Forced termination also targets the sole owned leader by positive PID:
    /// it may have moved into another group, which we must never signal.
    /// Success means at least one delivery was attempted, not group quiescence.
    /// No delivery target also includes Darwin exit-in-progress recipients;
    /// group_state must still prove their eventual cleanup independently.
    pub(crate) fn signal(&mut self, signal: i32) -> io::Result<bool> {
        self.observe()?;
        // SAFETY: pid is positive, belongs to the sole owned unreaped Child, and
        // WNOWAIT just verified association. Negation addresses only that group.
        let result = unsafe { libc::kill(-self.pid, signal) };
        let group = if result == 0 {
            Ok(true)
        } else {
            let error = io::Error::last_os_error();
            group_signal_failure(error, || {
                // Darwin's group kill can report EPERM when only zombies or
                // exiting processes remain. Signal-recipient proof is separate
                // from cleanup liveness: INEXIT is still live for cleanup.
                let terminal = self.observe()?;
                Ok((terminal, self.group_signal_targets()?))
            })
        };
        if self.is_retired() {
            // A failed verification may have detected ECHILD and retired the
            // numerical PID. Even forced termination must not use it again.
            return group;
        }
        // Even a failed/missing group must not prevent forced termination of
        // the direct process we still own. WNOWAIT pins its PID until reap.
        let leader = if signal == libc::SIGKILL && !self.exit_observed {
            // SAFETY: pid is positive and identifies the same sole owned,
            // unreaped child verified by WNOWAIT above. No group derived from
            // the child's current membership is addressed, so a controller
            // group joined by the child cannot receive this signal.
            if unsafe { libc::kill(self.pid, signal) } == 0 {
                Ok(true)
            } else {
                let error = io::Error::last_os_error();
                // Darwin may reject signalling a zombie with EPERM. Only a
                // new terminal WNOWAIT observation can make that harmless;
                // a live unkillable child remains an explicit signal error.
                match self.observe() {
                    Ok(true) => Ok(false),
                    Ok(false) => Err(error),
                    Err(observation) => Err(observation),
                }
            }
        } else {
            Ok(false)
        };
        // Preserve the original group failure even when the leader route
        // succeeded. One successful kill never proves cleanup of the group.
        match (group, leader) {
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(group_sent), Ok(leader_sent)) => Ok(group_sent || leader_sent),
        }
    }

    pub(crate) fn group_state(&self) -> io::Result<GroupState> {
        if self.is_retired() {
            return Ok(GroupState::Unknown);
        }
        let first = group_snapshot(self.pid)?;
        if !first.live.is_empty() {
            return Ok(GroupState::LiveMembers {
                count: first.live.len(),
            });
        }
        // A second census catches ordinary fork/exit and list/read races. This
        // remains observational evidence, never a claim about escaped processes.
        let second = group_snapshot(self.pid)?;
        if !second.live.is_empty() {
            return Ok(GroupState::LiveMembers {
                count: second.live.len(),
            });
        }
        if first.zombies != second.zombies {
            return Ok(GroupState::Unknown);
        }
        Ok(GroupState::NoLiveMembers {
            zombies: second.zombies.len(),
        })
    }

    fn group_signal_targets(&self) -> io::Result<GroupSignalTargets> {
        if self.is_retired() {
            return Ok(GroupSignalTargets::Unknown);
        }
        #[cfg(target_os = "macos")]
        {
            let first = group_snapshot(self.pid)?;
            let second = group_snapshot(self.pid)?;
            Ok(classify_group_signal_targets(&first, &second))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(self.group_state()?.into())
        }
    }

    pub(crate) fn reap(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.reaped_status {
            return Ok(status);
        }
        if self.is_retired() {
            return Err(retired_error());
        }
        if !self.exit_observed {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "leader termination has not been observed",
            ));
        }
        // A WEXITED event is terminal, so this wait cannot await a live leader.
        // All group signals must precede this operation. Retire even on failure.
        let result = self.child.as_mut().ok_or_else(retired_error)?.wait();
        self.associated = false;
        self.child.take();
        if let Ok(status) = result {
            self.reaped_status = Some(status);
        }
        result
    }

    /// Retired also covers lost association; it does NOT prove successful reap.
    pub(crate) fn is_retired(&self) -> bool {
        !self.associated
    }
}

fn group_signal_failure(
    error: io::Error,
    delivery_proof: impl FnOnce() -> io::Result<(bool, GroupSignalTargets)>,
) -> io::Result<bool> {
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    #[cfg(target_os = "macos")]
    if error.raw_os_error() == Some(libc::EPERM)
        && matches!(
            delivery_proof(),
            Ok((
                true,
                GroupSignalTargets::NoLiveMembers | GroupSignalTargets::ExitingMembers
            ))
        )
    {
        // AlreadyGone means no deliverable signal recipient. Exiting members
        // still count as live to group_state and must finish before cleanup.
        return Ok(false);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = delivery_proof;
    // Ordinary live/unknown targets, an unobserved leader, and failed ownership or
    // census probes never turn a permission failure into cleanup evidence.
    Err(error)
}

#[derive(Debug, PartialEq, Eq)]
enum GroupSignalTargets {
    NoLiveMembers,
    #[cfg(target_os = "macos")]
    ExitingMembers,
    LiveMembers,
    Unknown,
}

impl From<GroupState> for GroupSignalTargets {
    fn from(group: GroupState) -> Self {
        match group {
            GroupState::NoLiveMembers { .. } => Self::NoLiveMembers,
            GroupState::LiveMembers { .. } => Self::LiveMembers,
            GroupState::Unknown => Self::Unknown,
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if !self.associated {
            return;
        }
        match observe_pid(self.pid) {
            Ok(_) => {
                // SAFETY: the WNOWAIT check above establishes an owned unreaped
                // leader; no wait/reap occurs before the group signal.
                unsafe { libc::kill(-self.pid, libc::SIGKILL) };
                // Also stop the directly owned leader if it left its group.
                // Child has not been reaped and no other owner may wait for it.
                let _ = child.kill();
                self.associated = false;
                handoff_reaper(child);
            }
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                self.associated = false;
            }
            Err(_) => {
                // We still retain sole ownership; background wait is safe, but
                // no unverified numerical PID/PGID is used for signalling.
                self.associated = false;
                handoff_reaper(child);
            }
        }
    }
}

fn retired_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "process association was retired",
    )
}

fn check_sigchld_disposition() -> io::Result<()> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: a null new-action pointer queries without installing a handler;
    // action points to sufficient writable storage for the platform's ABI.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful sigaction initialized the complete native structure.
    let action = unsafe { action.assume_init() };
    if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SIGCHLD disposition discards child status",
        ));
    }
    Ok(())
}

fn observe_pid(pid: libc::pid_t) -> io::Result<bool> {
    for _ in 0..8 {
        // SAFETY: siginfo_t contains native integers, pointers and a C union;
        // all-zero storage is valid. Darwin may leave it untouched on WNOHANG.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: pid identifies the caller's sole owned child, the pointer is
        // valid initialized storage, and WNOWAIT prevents consuming its status.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: waitid with WEXITED fills the child-event union when a
            // PID is present; initialized zero storage represents no event.
            let observed = unsafe { info.si_pid() };
            if observed == 0 {
                return Ok(false);
            }
            if observed != pid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "waitid returned a different child",
                ));
            }
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Err(io::Error::from(io::ErrorKind::Interrupted))
}

#[derive(Default)]
struct GroupSnapshot {
    live: Vec<libc::pid_t>,
    zombies: Vec<libc::pid_t>,
    #[cfg(target_os = "macos")]
    exiting: Vec<libc::pid_t>,
}

impl GroupSnapshot {
    fn normalize(&mut self) {
        self.live.sort_unstable();
        self.live.dedup();
        self.zombies.sort_unstable();
        self.zombies.dedup();
        #[cfg(target_os = "macos")]
        {
            self.exiting.sort_unstable();
            self.exiting.dedup();
        }
    }
}

#[cfg(target_os = "macos")]
fn classify_group_signal_targets(
    first: &GroupSnapshot,
    second: &GroupSnapshot,
) -> GroupSignalTargets {
    // SRUN+INEXIT is still live for cleanup. Both complete censuses must show
    // no ordinary live member before EPERM can be a harmless delivery outcome.
    let ordinary_live = |snapshot: &GroupSnapshot| {
        snapshot
            .live
            .iter()
            .any(|pid| snapshot.exiting.binary_search(pid).is_err())
    };
    if ordinary_live(first) || ordinary_live(second) {
        return GroupSignalTargets::LiveMembers;
    }
    if !first.live.is_empty() || !second.live.is_empty() {
        return GroupSignalTargets::ExitingMembers;
    }
    if first.zombies != second.zombies {
        return GroupSignalTargets::Unknown;
    }
    GroupSignalTargets::NoLiveMembers
}

#[cfg(target_os = "macos")]
fn group_snapshot(pgid: libc::pid_t) -> io::Result<GroupSnapshot> {
    // Apple sys/proc_info.h: PROC_PGRP_ONLY = 2. libc provides all ABI types.
    const PROC_PGRP_ONLY: u32 = 2;
    let mut capacity = 64usize;
    loop {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let byte_capacity = std::mem::size_of_val(pids.as_slice());
        // SAFETY: __error returns this thread's writable errno pointer.
        unsafe { *libc::__error() = 0 };
        // SAFETY: pids is contiguous writable pid_t storage; byte count fits
        // c_int under MAX_GROUP_PIDS, and pgid is an owned positive group ID.
        let bytes = unsafe {
            libc::proc_listpids(
                PROC_PGRP_ONLY,
                pgid as u32,
                pids.as_mut_ptr().cast(),
                byte_capacity as i32,
            )
        };
        if bytes < 0 || (bytes == 0 && io::Error::last_os_error().raw_os_error() != Some(0)) {
            return Err(io::Error::last_os_error());
        }
        let bytes = bytes as usize;
        if bytes > byte_capacity || bytes % std::mem::size_of::<libc::pid_t>() != 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        if bytes == byte_capacity {
            if capacity == MAX_GROUP_PIDS {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "group census limit exceeded",
                ));
            }
            capacity = (capacity * 2).min(MAX_GROUP_PIDS);
            continue;
        }
        pids.truncate(bytes / std::mem::size_of::<libc::pid_t>());
        let mut snapshot = GroupSnapshot::default();
        for pid in pids.into_iter().filter(|pid| *pid > 0) {
            let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::uninit();
            let size = std::mem::size_of::<libc::proc_bsdshortinfo>();
            // SAFETY: __error is valid writable thread-local errno storage.
            unsafe { *libc::__error() = 0 };
            // SAFETY: info has the exact native ABI size. arg=1 requests zombie
            // lookup too; this read-only flavor does not require same-user UID.
            let bytes = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDT_SHORTBSDINFO,
                    1,
                    info.as_mut_ptr().cast(),
                    size as i32,
                )
            };
            if bytes != size as i32 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(if bytes == 0 && error.raw_os_error() != Some(0) {
                    error
                } else {
                    io::Error::from(io::ErrorKind::InvalidData)
                });
            }
            // SAFETY: an exact-size successful call initializes every field.
            let info = unsafe { info.assume_init() };
            if info.pbsi_pid != pid as u32 || info.pbsi_pgid != pgid as u32 {
                continue;
            }
            if info.pbsi_status == libc::SZOMB {
                snapshot.zombies.push(pid);
            } else {
                snapshot.live.push(pid);
                // Apple's public proc_info.h: PROC_FLAG_INEXIT = 4 means the
                // process is working its way through exit(). Keep it live for
                // cleanup; the flag only refines private signal-recipient proof.
                // https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/sys/proc_info.h
                if info.pbsi_flags & PROC_FLAG_INEXIT != 0 {
                    snapshot.exiting.push(pid);
                }
            }
        }
        snapshot.normalize();
        return Ok(snapshot);
    }
}

#[cfg(target_os = "linux")]
fn group_snapshot(pgid: libc::pid_t) -> io::Result<GroupSnapshot> {
    // Without complete procfs visibility, absence is not cleanup evidence.
    if !procfs_is_unrestricted()? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "restricted procfs visibility",
        ));
    }
    let mut snapshot = GroupSnapshot::default();
    let mut scanned = 0usize;
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<libc::pid_t>() else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        scanned += 1;
        if scanned > MAX_GROUP_PIDS {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "process census limit exceeded",
            ));
        }
        let bytes = match read_bounded(entry.path().join("stat"), 16_384) {
            Ok(bytes) => bytes,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {
                continue;
            }
            Err(error) => return Err(error),
        };
        let (group, state) = parse_proc_stat(&bytes)?;
        if group != pgid {
            continue;
        }
        match state {
            b'Z' => snapshot.zombies.push(pid),
            b'X' | b'x' => {}
            _ => snapshot.live.push(pid),
        }
    }
    snapshot.normalize();
    Ok(snapshot)
}

#[cfg(target_os = "linux")]
fn read_bounded(path: impl AsRef<std::path::Path>, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "procfs record limit exceeded",
        ));
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn procfs_is_unrestricted() -> io::Result<bool> {
    let directory = std::fs::File::open("/proc")?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: directory is an owned open FD; filesystem is sufficient writable
    // storage for Linux statfs. This is a read-only filesystem query.
    if unsafe { libc::fstatfs(directory.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatfs initialized the native structure.
    if unsafe { filesystem.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
        return Ok(false);
    }
    // procfs can be mounted in an ancestor PID namespace (for example after
    // unshare --pid without mounting a new /proc). Its numeric PID/PGID records
    // would then differ from IDs used by waitid and kill in this process.
    // NStgid lists IDs from procfs's namespace into each nested namespace;
    // exactly one column matching getpid verifies that both use our namespace.
    let status = read_bounded("/proc/self/status", 1_048_576)?;
    // SAFETY: getpid has no pointer arguments and returns the caller's own PID
    // in its active PID namespace without changing process state.
    let pid = unsafe { libc::getpid() };
    if !procfs_namespace_matches(&status, pid)? {
        return Ok(false);
    }
    let fdinfo = read_bounded(
        format!("/proc/self/fdinfo/{}", directory.as_raw_fd()),
        16_384,
    )?;
    let mount_id = fdinfo
        .split(|byte| *byte == b'\n')
        .find_map(|line| {
            line.strip_prefix(b"mnt_id:")
                .and_then(|field| std::str::from_utf8(field).ok())
                .and_then(|field| field.trim().parse::<u64>().ok())
        })
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let mountinfo = read_bounded("/proc/self/mountinfo", 1_048_576)?;
    Ok(mountinfo.split(|byte| *byte == b'\n').any(|line| {
        let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
        if fields.len() < 10
            || std::str::from_utf8(fields[0])
                .ok()
                .and_then(|field| field.parse::<u64>().ok())
                != Some(mount_id)
        {
            return false;
        }
        let Some(separator) = fields.iter().position(|field| *field == b"-") else {
            return false;
        };
        if fields.get(separator + 1) != Some(&b"proc".as_slice()) {
            return false;
        }
        !fields
            .iter()
            .flat_map(|field| field.split(|byte| *byte == b','))
            .any(|option| {
                option.starts_with(b"hidepid=")
                    && option != b"hidepid=0"
                    && option != b"hidepid=off"
            })
    }))
}

#[cfg(any(target_os = "linux", test))]
fn procfs_namespace_matches(status: &[u8], pid: libc::pid_t) -> io::Result<bool> {
    let invalid = || io::Error::from(io::ErrorKind::InvalidData);
    let mut records = status
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"NStgid:"));
    let record = records.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "procfs PID namespace is not verifiable",
        )
    })?;
    if records.next().is_some() {
        return Err(invalid());
    }
    let parse = |field: &[u8]| -> io::Result<libc::pid_t> {
        if field.is_empty() || !field.iter().all(u8::is_ascii_digit) {
            return Err(invalid());
        }
        std::str::from_utf8(field)
            .ok()
            .and_then(|field| field.parse::<libc::pid_t>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(invalid)
    };
    let mut fields = record
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let first = parse(fields.next().ok_or_else(invalid)?)?;
    let mut columns = 1usize;
    for field in fields {
        parse(field)?;
        columns += 1;
    }
    // Equal IDs in different namespaces can be a coincidence. Count columns,
    // rather than trusting equality of the first or last numeric PID alone.
    Ok(columns == 1 && first == pid)
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat(bytes: &[u8]) -> io::Result<(libc::pid_t, u8)> {
    // comm can contain spaces, parentheses and non-UTF-8 bytes. Numeric fields
    // start after its final closing parenthesis, never after split_whitespace[2].
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut fields = bytes[end + 1..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let state = fields
        .next()
        .filter(|field| field.len() == 1)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?[0];
    fields
        .next()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    let group = fields
        .next()
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.parse::<libc::pid_t>().ok())
        .filter(|group| *group >= 0)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    Ok((group, state))
}

struct PendingReap {
    child: Child,
    deadline: Instant,
}

fn handoff_reaper(mut child: Child) {
    static REAPER: OnceLock<Option<SyncSender<PendingReap>>> = OnceLock::new();
    let sender = REAPER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<PendingReap>(REAPER_CAPACITY);
        std::thread::Builder::new()
            .name("failforge-reaper".into())
            .spawn(move || {
                let mut pending = Vec::<PendingReap>::with_capacity(REAPER_CAPACITY);
                loop {
                    if pending.is_empty() {
                        let Ok(child) = receiver.recv() else { break };
                        pending.push(child);
                    }
                    while pending.len() < REAPER_CAPACITY {
                        match receiver.try_recv() {
                            Ok(child) => pending.push(child),
                            Err(_) => break,
                        }
                    }
                    let now = Instant::now();
                    for index in (0..pending.len()).rev() {
                        match pending[index].child.try_wait() {
                            Ok(None) if now < pending[index].deadline => {}
                            _ => {
                                pending.swap_remove(index);
                            }
                        }
                    }
                    if !pending.is_empty() {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            })
            .ok()
            .map(|_| sender)
    });
    // Reaping here is safe: all fallback group signals already happened, and
    // no numerical signalling occurs in this background worker after wait.
    match child.try_wait() {
        Ok(None) => {}
        // Once wait reports an error, never retry a numerical PID whose
        // association may have been consumed by another reaper.
        Ok(Some(_)) | Err(_) => return,
    }
    if let Some(sender) = sender {
        let now = Instant::now();
        let deadline = now.checked_add(REAPER_BUDGET).unwrap_or(now);
        let _ = sender.try_send(PendingReap { child, deadline });
    }
    // Full/unavailable queue or expired budget: best effort ends, never block
    // Drop. An aborted runtime cannot claim successful cleanup in this case.
}

#[cfg(test)]
#[path = "platform_tests.rs"]
mod tests;

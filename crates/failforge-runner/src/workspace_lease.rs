//! An inherited campaign lease survives its Python controller until this owner exits.
//!
//! Only the Rust CLI/helper owns this capability. Both inherited and duplicated
//! descriptors are close-on-exec before a managed command can start, so subject
//! code receives neither. No lock operation is performed here: unlocking a
//! duplicated flock would also unlock the Python controller's shared description.

use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, MetadataExt};

use crate::CommandSpec;

/// Internal descriptor capability supplied only to the Rust cleanup owner.
pub const WORKSPACE_LEASE_ENV: &str = "FAILFORGE_WORKSPACE_LEASE_FD";
/// Exact control-file content; incomplete creation is never a native capability.
pub const WORKSPACE_LEASE_MAGIC: &[u8] = b"FAILFORGE_WORKSPACE_LEASE_V1\n";

/// Keeps a verified campaign lease open through native group cleanup.
///
/// The inherited source descriptor remains open until process exit. We own a
/// duplicate instead of taking ownership of an arbitrary descriptor named by
/// the environment: repeated adoption cannot double-close someone else's File.
/// Both descriptors are close-on-exec, including on validation failure after
/// source flags were changed. The owner must retain this value through shutdown.
///
/// This protects controller death while Rust owners remain alive. It is not a
/// guarantee about descendants after the Rust owner itself receives SIGKILL.
#[derive(Debug)]
pub struct WorkspaceLease {
    _descriptor: OwnedFd,
}

impl WorkspaceLease {
    /// Retain an already owned campaign capability without reopening its path.
    ///
    /// The descriptor is borrowed and duplicated, never consumed or unlocked.
    /// Both descriptors become close-on-exec; the same metadata and exact marker
    /// checks used for inherited capabilities apply before returning ownership.
    pub fn from_borrowed(descriptor: BorrowedFd<'_>) -> io::Result<Self> {
        Self::from_descriptor(descriptor.as_raw_fd())
    }

    /// Keep the owner's private control variable out of a managed child's env.
    ///
    /// Explicit overrides are removed before adding the removal instruction so
    /// the resulting EnvSpec has no set/remove conflict. Global env is unchanged.
    pub fn protect_child(&self, spec: &mut CommandSpec) {
        spec.env.set.remove(OsStr::new(WORKSPACE_LEASE_ENV));
        spec.env.remove.insert(WORKSPACE_LEASE_ENV.into());
    }

    /// Verify an optional inherited descriptor without changing global env.
    ///
    /// Invalid capabilities fail before a managed child is spawned. The error
    /// never includes an environment value, pathname or file contents.
    pub fn from_env() -> io::Result<Option<Self>> {
        Self::from_environment_value(std::env::var_os(WORKSPACE_LEASE_ENV).as_deref())
    }

    fn from_environment_value(value: Option<&OsStr>) -> io::Result<Option<Self>> {
        let Some(value) = value else {
            return Ok(None);
        };
        let text = value.to_str().ok_or_else(invalid_capability)?;
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid_capability());
        }
        let descriptor: RawFd = text.parse().map_err(|_| invalid_capability())?;
        if descriptor < 3 {
            return Err(invalid_capability());
        }
        Self::from_descriptor(descriptor).map(Some)
    }

    fn from_descriptor(descriptor: RawFd) -> io::Result<Self> {
        // SAFETY: fcntl takes an integer descriptor and creates a new independent
        // descriptor referring to its description. No Rust-owned fd is consumed.
        let copied = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 3) };
        if copied < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful F_DUPFD_CLOEXEC created this unique live descriptor.
        let file = unsafe { File::from_raw_fd(copied) };
        // SAFETY: geteuid has no pointer arguments or side effects.
        validate_metadata(&file.metadata()?, unsafe { libc::geteuid() })?;
        let mut magic = [0_u8; WORKSPACE_LEASE_MAGIC.len()];
        file.read_exact_at(&mut magic, 0)?;
        if magic != WORKSPACE_LEASE_MAGIC {
            return Err(invalid_capability());
        }
        let mut extra = [0_u8; 1];
        if file.read_at(&mut extra, WORKSPACE_LEASE_MAGIC.len() as u64)? != 0 {
            return Err(invalid_capability());
        }
        // Check again after reads, including races with truncation/replacement.
        // SAFETY: geteuid has no pointer arguments or side effects.
        validate_metadata(&file.metadata()?, unsafe { libc::geteuid() })?;
        // The duplicate starts CLOEXEC; source must also close before child
        // exec. Preserve any other descriptor flags and never modify the flock.
        // SAFETY: fcntl operates only on an existing integer descriptor.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: F_SETFD takes only descriptor flags; no memory is referenced.
        if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _descriptor: file.into(),
        })
    }
}

fn validate_metadata(metadata: &Metadata, owner: libc::uid_t) -> io::Result<()> {
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() != WORKSPACE_LEASE_MAGIC.len() as u64
    {
        return Err(invalid_capability());
    }
    Ok(())
}

fn invalid_capability() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid inherited workspace lease capability",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, DirBuilder, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const PROBE_MODE: &str = "FAILFORGE_LEASE_UNIT_PROBE";
    const PROBE_FDS: &str = "FAILFORGE_LEASE_UNIT_FDS";

    struct Fixture {
        directory: PathBuf,
        path: PathBuf,
        file: Option<File>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = loop {
                let path = std::env::temp_dir().join(format!(
                    "failforge lease unit {} {}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => break path,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create private unit fixture: {error}"),
                }
            };
            let path = directory.join("lease");
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            file.write_all(WORKSPACE_LEASE_MAGIC).unwrap();
            Self {
                directory,
                path,
                file: Some(file),
            }
        }

        fn file(&self) -> &File {
            self.file.as_ref().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn command() -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "workspace_lease::tests::inherited_fd_probe",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn wait_ready(child: &mut Child) {
        let mut stdout = child.stdout.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut received = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "owner did not report lease readiness");
            let mut poll = libc::pollfd {
                fd: stdout.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll references one valid local pollfd for the owned pipe.
            let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
            if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            assert!(ready > 0, "owner did not report lease readiness");
            let mut buffer = [0_u8; 1024];
            let count = stdout.read(&mut buffer).unwrap();
            assert_ne!(count, 0, "owner exited before READY");
            received.extend_from_slice(&buffer[..count]);
            assert!(received.len() < 16 * 1024, "unexpected owner output volume");
            if received
                .windows(b"LEASE_READY".len())
                .any(|part| part == b"LEASE_READY")
            {
                // The child test harness still writes its final status line.
                child.stdout = Some(stdout);
                break;
            }
        }
    }

    #[test]
    fn borrowed_capability_retains_lock_after_source_closes_and_rejects_wrong_file() {
        let mut fixture = Fixture::new();
        // SAFETY: the private fixture file is live and exclusively owned.
        assert_eq!(
            unsafe { libc::flock(fixture.file().as_raw_fd(), libc::LOCK_EX) },
            0
        );
        let lease = WorkspaceLease::from_borrowed(fixture.file().as_fd()).unwrap();
        drop(fixture.file.take());
        let peer = File::open(&fixture.path).unwrap();
        // SAFETY: nonblocking lock probes use only this live fixture descriptor.
        assert_eq!(
            unsafe { libc::flock(peer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
        drop(lease);
        // SAFETY: all references to the original open file description are closed.
        assert_eq!(
            unsafe { libc::flock(peer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let directory = File::open(&fixture.directory).unwrap();
        assert!(WorkspaceLease::from_borrowed(directory.as_fd()).is_err());
        assert!(directory.metadata().unwrap().is_dir());
    }

    #[test]
    fn optional_environment_and_strict_descriptor_grammar() {
        assert!(
            WorkspaceLease::from_environment_value(None)
                .unwrap()
                .is_none()
        );
        for value in [
            "",
            "0",
            "1",
            "2",
            "-1",
            "+3",
            " 3",
            "3 ",
            "3\0",
            "３",
            "99999999999999999999999999999999",
        ] {
            assert!(WorkspaceLease::from_environment_value(Some(OsStr::new(value))).is_err());
        }
        assert!(WorkspaceLease::from_environment_value(Some(OsStr::from_bytes(b"\xff"))).is_err());
        assert!(WorkspaceLease::from_environment_value(Some(OsStr::new("2147483647"))).is_err());
    }

    #[test]
    fn exact_magic_private_owner_and_no_content_or_offset_mutation() {
        let mut fixture = Fixture::new();
        fixture
            .file
            .as_mut()
            .unwrap()
            .seek(SeekFrom::Start(7))
            .unwrap();
        let source = fixture.file().as_raw_fd();
        let first = WorkspaceLease::from_descriptor(source).unwrap();
        let second = WorkspaceLease::from_descriptor(source).unwrap();
        assert_ne!(first._descriptor.as_raw_fd(), source);
        assert_ne!(
            first._descriptor.as_raw_fd(),
            second._descriptor.as_raw_fd()
        );
        assert_eq!(fixture.file.as_mut().unwrap().stream_position().unwrap(), 7);
        assert_eq!(fs::read(&fixture.path).unwrap(), WORKSPACE_LEASE_MAGIC);
        // SAFETY: F_GETFD only observes live fixture descriptors.
        assert_ne!(
            unsafe { libc::fcntl(source, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        drop(first);
        drop(second);
        assert!(fixture.file().metadata().is_ok());
        // SAFETY: geteuid is side-effect free; mismatch is checked without chown.
        let foreign = unsafe { libc::geteuid() }.wrapping_add(1);
        assert!(validate_metadata(&fixture.file().metadata().unwrap(), foreign).is_err());
    }

    #[test]
    fn incomplete_changed_or_extended_magic_is_rejected() {
        for content in [
            b"".as_slice(),
            b"FAILFORGE_WORKSPACE_".as_slice(),
            b"FAILFORGE_WORKSPACE_LEASE_V2\n".as_slice(),
            b"FAILFORGE_WORKSPACE_LEASE_V1\nextra".as_slice(),
        ] {
            let mut fixture = Fixture::new();
            let file = fixture.file.as_mut().unwrap();
            file.set_len(0).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(content).unwrap();
            assert!(WorkspaceLease::from_descriptor(file.as_raw_fd()).is_err());
            assert_eq!(fs::read(&fixture.path).unwrap(), content);
        }
    }

    #[test]
    fn permissive_special_or_readonly_modes_and_hardlinks_are_rejected() {
        for mode in [0o400, 0o640, 0o666, 0o4600] {
            let fixture = Fixture::new();
            fs::set_permissions(&fixture.path, fs::Permissions::from_mode(mode)).unwrap();
            let observed = fixture.file().metadata().unwrap().mode() & 0o7777;
            // Some host filesystems clear set-user-ID on non-executable data
            // files. Validate what the kernel actually retained, not chmod's
            // requested flag; Linux still exercises the retained special bit.
            if mode == 0o4600 && observed == 0o600 {
                assert!(WorkspaceLease::from_descriptor(fixture.file().as_raw_fd()).is_ok());
            } else {
                assert_eq!(observed, mode);
                assert!(WorkspaceLease::from_descriptor(fixture.file().as_raw_fd()).is_err());
            }
            assert_eq!(fs::read(&fixture.path).unwrap(), WORKSPACE_LEASE_MAGIC);
        }
        let fixture = Fixture::new();
        fs::hard_link(&fixture.path, fixture.directory.join("unrelated")).unwrap();
        assert!(WorkspaceLease::from_descriptor(fixture.file().as_raw_fd()).is_err());
        assert_eq!(fs::read(&fixture.path).unwrap(), WORKSPACE_LEASE_MAGIC);
    }

    #[test]
    fn directory_descriptor_is_rejected_without_closing_source() {
        let fixture = Fixture::new();
        let directory = File::open(&fixture.directory).unwrap();
        assert!(WorkspaceLease::from_descriptor(directory.as_raw_fd()).is_err());
        assert!(directory.metadata().unwrap().is_dir());
    }

    #[test]
    fn adopted_descriptors_do_not_reach_an_exec_child() {
        let fixture = Fixture::new();
        let lease = WorkspaceLease::from_descriptor(fixture.file().as_raw_fd()).unwrap();
        let status = command()
            .env(PROBE_MODE, "closed")
            .env(
                PROBE_FDS,
                format!(
                    "{},{}",
                    fixture.file().as_raw_fd(),
                    lease._descriptor.as_raw_fd()
                ),
            )
            .env_remove(WORKSPACE_LEASE_ENV)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn protected_child_drops_internal_override_and_preserves_other_environment() {
        let fixture = Fixture::new();
        let lease = WorkspaceLease::from_descriptor(fixture.file().as_raw_fd()).unwrap();
        let mut spec = CommandSpec {
            program: "/bin/true".into(),
            args: vec![],
            cwd: std::env::current_dir().unwrap(),
            env: Default::default(),
            timeout_policy: Default::default(),
            capture_policy: Default::default(),
        };
        spec.env.clear = true;
        spec.env
            .set
            .insert(WORKSPACE_LEASE_ENV.into(), "override".into());
        spec.env
            .set
            .insert("APPLICATION_VALUE".into(), "kept".into());
        spec.env.remove.insert("OTHER_REMOVAL".into());
        lease.protect_child(&mut spec);
        lease.protect_child(&mut spec); // Idempotent, including set/remove safety.
        assert!(!spec.env.set.contains_key(OsStr::new(WORKSPACE_LEASE_ENV)));
        assert!(spec.env.remove.contains(OsStr::new(WORKSPACE_LEASE_ENV)));
        assert_eq!(
            spec.env.set.get(OsStr::new("APPLICATION_VALUE")),
            Some(&"kept".into())
        );
        assert!(spec.env.remove.contains(OsStr::new("OTHER_REMOVAL")));
        assert!(spec.env.clear);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn inherited_owner_keeps_flock_after_controller_descriptor_closes() {
        let mut fixture = Fixture::new();
        let source = fixture.file().as_raw_fd();
        // SAFETY: flock is applied only to the private live fixture file.
        assert_eq!(
            unsafe { libc::flock(source, libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let mut launch = command();
        launch
            .env(PROBE_MODE, "hold")
            .env(WORKSPACE_LEASE_ENV, source.to_string());
        // SAFETY: the child callback performs only fcntl; parent flags are never
        // made inheritable, even while unrelated test threads spawn commands.
        unsafe {
            launch.pre_exec(move || {
                let flags = libc::fcntl(source, libc::F_GETFD);
                if flags < 0 || libc::fcntl(source, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = ChildGuard(launch.spawn().unwrap());
        wait_ready(&mut child.0);
        drop(fixture.file.take()); // Close, never LOCK_UN, preserves child's lease.
        let peer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fixture.path)
            .unwrap();
        // SAFETY: nonblocking lock probes use only the private peer descriptor.
        assert_eq!(
            unsafe { libc::flock(peer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            -1
        );
        assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
        child.0.stdin.take().unwrap().write_all(b"x").unwrap();
        assert!(child.0.wait().unwrap().success());
        // SAFETY: the child has exited and closed every inherited description.
        assert_eq!(
            unsafe { libc::flock(peer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
    }

    #[test]
    fn inherited_fd_probe() {
        let Some(mode) = std::env::var_os(PROBE_MODE) else {
            return; // Normal cargo invocation; only the explicitly owned child probes.
        };
        match mode.to_str().unwrap() {
            "closed" => {
                for text in std::env::var(PROBE_FDS).unwrap().split(',') {
                    let fd: RawFd = text.parse().unwrap();
                    // SAFETY: F_GETFD observes the descriptor, without dereferencing.
                    assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
                    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
                }
            }
            "hold" => {
                let lease = WorkspaceLease::from_env().unwrap().unwrap();
                let source: RawFd = std::env::var(WORKSPACE_LEASE_ENV).unwrap().parse().unwrap();
                // The test parent explicitly cleared this flag in its fork
                // child, so this proves adoption changes an inherited source.
                // SAFETY: F_GETFD observes the verified source descriptor.
                assert_ne!(
                    unsafe { libc::fcntl(source, libc::F_GETFD) } & libc::FD_CLOEXEC,
                    0
                );
                // Also exercise an actual subsequent exec, not only flags.
                assert!(
                    command()
                        .env(PROBE_MODE, "closed")
                        .env(
                            PROBE_FDS,
                            format!("{source},{}", lease._descriptor.as_raw_fd())
                        )
                        .env_remove(WORKSPACE_LEASE_ENV)
                        .status()
                        .unwrap()
                        .success()
                );
                println!("LEASE_READY");
                io::stdout().flush().unwrap();
                io::stdin().read_exact(&mut [0_u8; 1]).unwrap();
            }
            _ => panic!("unknown unit probe mode"),
        }
    }
}

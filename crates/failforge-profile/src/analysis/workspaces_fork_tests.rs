use super::*;
use std::cell::{Cell, RefCell};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(3);

struct ChildGuard(Child);
impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}
impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}
impl ChildGuard {
    fn finish(&mut self) -> io::Result<ExitStatus> {
        self.stdin.take();
        let mut deadline = Instant::now() + FIXTURE_TIMEOUT;
        let mut killed = false;
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                if killed {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fixture child did not drain",
                    ));
                }
                self.kill()?;
                killed = true;
                deadline = Instant::now() + FIXTURE_TIMEOUT;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

struct Fixture {
    root: PathBuf,
    work: Workspace,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ff-profile-fork-{}", random_token().unwrap()));
        let work = Workspace::at(root.clone(), &[]).unwrap();
        fs::write(work.path.join("valuable"), b"keep").unwrap();
        Self { root, work }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.work.close();
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn pipe() -> (File, File) {
    let mut raw = [-1; 2];
    // SAFETY: raw has space for two newly owned descriptors.
    assert_eq!(unsafe { libc::pipe(raw.as_mut_ptr()) }, 0);
    for descriptor in raw {
        // SAFETY: each descriptor is live and this sets its close-on-exec flag.
        assert_eq!(
            unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    // SAFETY: pipe returned two distinct descriptors with no Rust owners.
    unsafe { (File::from_raw_fd(raw[0]), File::from_raw_fd(raw[1])) }
}

/// Hold an unrelated child between fork and exec using only signal-safe calls.
/// A real owner instead clears CLOEXEC on the lease before exec.
struct ForkWindow {
    release: Option<File>,
    spawning: Option<JoinHandle<io::Result<Child>>>,
    _read: File,
    _write: File,
}
impl ForkWindow {
    fn new(lease: RawFd, real_owner: bool) -> Self {
        let (mut ready_read, ready_write) = pipe();
        let (release_read, release_write) = pipe();
        let notify = ready_write.as_raw_fd();
        let release = release_read.as_raw_fd();
        let spawning = std::thread::spawn(move || {
            let mut command = Command::new("/bin/cat");
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: the child only uses fcntl/read/write before exec. The
            // parent retains all referenced descriptors until spawn returns.
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(lease, libc::F_GETFD);
                    if flags < 0 || flags & libc::FD_CLOEXEC == 0 {
                        return Err(io::Error::from_raw_os_error(libc::EBADF));
                    }
                    if real_owner
                        && libc::fcntl(lease, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    let ready = b'R';
                    if libc::write(notify, (&ready as *const u8).cast(), 1) != 1 {
                        return Err(io::Error::last_os_error());
                    }
                    let mut byte = 0u8;
                    let mut poll = libc::pollfd {
                        fd: release,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if libc::poll(&mut poll, 1, 2000) <= 0 {
                        return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
                    }
                    if libc::read(release, (&mut byte as *mut u8).cast(), 1) != 1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            command.spawn()
        });
        let window = Self {
            release: Some(release_write),
            spawning: Some(spawning),
            _read: release_read,
            _write: ready_write,
        };
        let mut poll = libc::pollfd {
            fd: ready_read.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows one live descriptor with initialized storage.
        let observed = unsafe { libc::poll(&mut poll, 1, 2000) };
        assert!(observed > 0, "pre-exec fixture did not report readiness");
        let mut ready = [0];
        ready_read.read_exact(&mut ready).unwrap();
        assert_eq!(ready, [b'R']);
        window
    }
    fn try_exec(&mut self) -> io::Result<ChildGuard> {
        if let Some(mut release) = self.release.take() {
            release.write_all(b"E")?;
        }
        let deadline = Instant::now() + FIXTURE_TIMEOUT;
        while !self.spawning.as_ref().unwrap().is_finished() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture spawn did not finish",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.spawning
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("fixture spawn thread panicked"))?
            .map(ChildGuard)
    }
    fn exec(&mut self) -> ChildGuard {
        self.try_exec().unwrap()
    }
}
impl Drop for ForkWindow {
    fn drop(&mut self) {
        if self.spawning.is_some() {
            if let Ok(mut child) = self.try_exec() {
                let _ = child.finish();
            }
        }
    }
}

#[test]
fn unrelated_fork_exec_lease_does_not_fail_cleanup() {
    let mut fixture = Fixture::new();
    let target = fixture.root.join("unrelated-target");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("valuable"), b"preserve").unwrap();
    std::os::unix::fs::symlink(&target, fixture.work.path.join("alias")).unwrap();
    let mut window = ForkWindow::new(fixture.work.lease_fd(), false);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        window.exec()
    });
    let started = Instant::now();
    let result = fixture.work.close();
    let elapsed = started.elapsed();
    let mut child = release.join().unwrap();
    let alive_after_exec = child.try_wait().unwrap().is_none();
    drop(child.stdin.take());
    assert!(child.finish().unwrap().success());
    eprintln!(
        "fork/exec cleanup result={result:?}, elapsed={elapsed:?}, unrelated_child_alive={alive_after_exec}"
    );
    assert!(alive_after_exec);
    result.expect("an unrelated pre-exec lease must drain within the cleanup budget");
    assert!(!fixture.work.path.exists());
    assert_eq!(fs::read(target.join("valuable")).unwrap(), b"preserve");
}

#[test]
fn actual_owner_survives_cleanup_deadline_and_is_recoverable_after_exit() {
    let mut fixture = Fixture::new();
    let mut window = ForkWindow::new(fixture.work.lease_fd(), true);
    let mut child = window.exec();
    let global = regular(
        fixture.work.root.as_raw_fd(),
        OsStr::new(".global.lock"),
        false,
    )
    .unwrap();
    assert!(lock_until(&global, Instant::now() + CLEANUP_LOCK_BUDGET).unwrap());
    let release_global = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(700));
        drop(global);
    });
    let started = Instant::now();
    let result = fixture.work.close();
    let elapsed = started.elapsed();
    release_global.join().unwrap();
    let alive = child.try_wait().unwrap().is_none();
    let payload = fs::read(fixture.work.path.join("valuable"));
    drop(child.stdin.take());
    assert!(child.finish().unwrap().success());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("active cleanup owner")
    );
    assert!(alive);
    assert_eq!(payload.unwrap(), b"keep");
    assert!(elapsed >= CLEANUP_LOCK_BUDGET);
    // Coordination consumed most of the budget. The lease must not receive a
    // second full second after coordination becomes available.
    assert!(elapsed < CLEANUP_LOCK_BUDGET + Duration::from_millis(500));
    fixture.work.close().unwrap();
    assert!(!fixture.work.path.exists());
}

#[test]
fn cancelled_preparation_still_drains_transient_inheritance() {
    let mut fixture = Fixture::new();
    let cancellation = super::super::Cancellation::default();
    cancellation.request(libc::SIGINT);
    assert!(cancellation.check().is_err());
    let mut window = ForkWindow::new(fixture.work.lease_fd(), false);
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        window.exec()
    });
    let result = fixture.work.close();
    let mut child = release.join().unwrap();
    drop(child.stdin.take());
    assert!(child.finish().unwrap().success());
    result.unwrap();
    assert!(!fixture.work.path.exists());
    assert!(cancellation.check().is_err());
}

#[test]
fn detached_recovery_waits_for_transient_inheritance() {
    let mut fixture = Fixture::new();
    let recovery = fixture.work.recovery().unwrap();
    let mut window = ForkWindow::new(fixture.work.lease_fd(), false);
    fixture.work.lease.take();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        window.exec()
    });
    let result = recovery.recover();
    let mut child = release.join().unwrap();
    drop(child.stdin.take());
    assert!(child.finish().unwrap().success());
    result.unwrap();
    assert!(!fixture.work.path.exists());
    fixture.work.closed = true;
}

#[test]
fn changed_campaign_work_lease_or_registry_during_wait_is_preserved() {
    for wait_on_global in [false, true] {
        for kind in [
            "campaign", "work", "lease", "metadata", "registry", "alias", "global", "owner",
        ] {
            let mut fixture = Fixture::new();
            let global = if wait_on_global {
                let file = regular(
                    fixture.work.root.as_raw_fd(),
                    OsStr::new(".global.lock"),
                    false,
                )
                .unwrap();
                assert!(lock_until(&file, Instant::now() + CLEANUP_LOCK_BUDGET).unwrap());
                Some(file)
            } else {
                None
            };
            let mut window = ForkWindow::new(fixture.work.lease_fd(), false);
            let campaign = fixture.work.path.parent().unwrap().to_owned();
            let target = match kind {
                "campaign" => campaign.clone(),
                "lease" | "metadata" => campaign.join(kind),
                "global" => fixture.root.join(".global.lock"),
                "owner" => fixture.root.join(".owner"),
                "registry" => fixture.root.clone(),
                _ => fixture.work.path.clone(),
            };
            let moved = target.with_extension("moved");
            let child_target = target.clone();
            let child_moved = moved.clone();
            let mutate = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                fs::rename(&child_target, &child_moved).unwrap();
                match kind {
                    "lease" | "metadata" | "global" | "owner" => {
                        let bytes = fs::read(&child_moved).unwrap();
                        let file = regular(libc::AT_FDCWD, child_target.as_os_str(), true).unwrap();
                        write_marker(&file, &bytes).unwrap();
                    }
                    "alias" => std::os::unix::fs::symlink(&child_moved, &child_target).unwrap(),
                    _ => {
                        fs::DirBuilder::new()
                            .mode(0o700)
                            .create(&child_target)
                            .unwrap();
                        fs::write(child_target.join("foreign"), b"preserve").unwrap();
                    }
                }
                drop(global);
                window.exec()
            });
            let result = fixture.work.close();
            let mut child = mutate.join().unwrap();
            drop(child.stdin.take());
            assert!(child.finish().unwrap().success());
            assert!(
                result.is_err(),
                "{kind}, global={wait_on_global}: {result:?}"
            );
            match kind {
                "lease" | "metadata" | "global" | "owner" => {
                    assert!(target.exists());
                    assert!(moved.exists());
                    assert_eq!(
                        fs::read(fixture.work.path.join("valuable")).unwrap(),
                        b"keep"
                    );
                    fs::remove_file(&target).unwrap();
                }
                "alias" => {
                    assert_eq!(fs::read(moved.join("valuable")).unwrap(), b"keep");
                    fs::remove_file(&target).unwrap();
                }
                _ => {
                    assert_eq!(fs::read(target.join("foreign")).unwrap(), b"preserve");
                    let original = match kind {
                        "registry" => moved
                            .join(&fixture.work.campaign_name)
                            .join("work/valuable"),
                        "campaign" => moved.join("work/valuable"),
                        _ => moved.join("valuable"),
                    };
                    assert_eq!(fs::read(original).unwrap(), b"keep");
                    fs::remove_dir_all(&target).unwrap();
                }
            }
            fs::rename(moved, target).unwrap();
            fixture.work.close().unwrap();
        }
    }
}

#[test]
fn global_coordination_wait_is_bounded_shared_and_checks_identity() {
    for replace in [false, true] {
        let mut fixture = Fixture::new();
        let global = regular(
            fixture.work.root.as_raw_fd(),
            OsStr::new(".global.lock"),
            false,
        )
        .unwrap();
        assert!(lock_until(&global, Instant::now() + CLEANUP_LOCK_BUDGET).unwrap());
        let root = fixture.root.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            if replace {
                fs::rename(root.join(".global.lock"), root.join(".old-global")).unwrap();
                regular(libc::AT_FDCWD, root.join(".global.lock").as_os_str(), true).unwrap();
            }
            drop(global);
        });
        let result = fixture.work.close();
        release.join().unwrap();
        if replace {
            assert!(result.is_err());
            assert_eq!(
                fs::read(fixture.work.path.join("valuable")).unwrap(),
                b"keep"
            );
            fs::remove_file(fixture.root.join(".global.lock")).unwrap();
            fs::rename(
                fixture.root.join(".old-global"),
                fixture.root.join(".global.lock"),
            )
            .unwrap();
            fixture.work.close().unwrap();
        } else {
            result.unwrap();
            assert!(!fixture.work.path.exists());
        }
    }
    let mut fixture = Fixture::new();
    let global = regular(
        fixture.work.root.as_raw_fd(),
        OsStr::new(".global.lock"),
        false,
    )
    .unwrap();
    assert!(lock_until(&global, Instant::now() + CLEANUP_LOCK_BUDGET).unwrap());
    let started = Instant::now();
    let result = fixture.work.close();
    let elapsed = started.elapsed();
    assert!(result.unwrap_err().to_string().contains("registry still"));
    assert!(elapsed >= CLEANUP_LOCK_BUDGET);
    assert!(elapsed < CLEANUP_LOCK_BUDGET + Duration::from_secs(1));
    assert!(fixture.work.lease.is_some());
    assert_eq!(
        fs::read(fixture.work.path.join("valuable")).unwrap(),
        b"keep"
    );
    drop(global);
    fixture.work.close().unwrap();
}

struct LockClock {
    now: Cell<Instant>,
    sleeps: RefCell<Vec<Duration>>,
}

impl LockClock {
    fn new() -> Self {
        Self {
            now: Cell::new(Instant::now()),
            sleeps: RefCell::new(Vec::new()),
        }
    }

    fn advance(&self, duration: Duration) {
        self.now.set(self.now.get() + duration);
    }

    fn sleep(&self, duration: Duration) {
        self.sleeps.borrow_mut().push(duration);
        self.advance(duration);
    }
}

#[test]
fn lock_retry_succeeds_after_two_interruptions_before_the_deadline() {
    let clock = LockClock::new();
    let started = clock.now.get();
    let deadline = started + Duration::from_millis(40);
    let mut attempts = 0;
    assert!(
        wait_for_lock_with_clock(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(io::Error::from_raw_os_error(libc::EINTR))
                } else {
                    Ok(true)
                }
            },
            deadline,
            || clock.now.get(),
            |duration| clock.sleep(duration),
        )
        .unwrap()
    );
    assert_eq!(attempts, 3);
    assert_eq!(*clock.sleeps.borrow(), [LOCK_RETRY_INTERVAL; 2]);
    assert_eq!(clock.now.get() - started, LOCK_RETRY_INTERVAL * 2);
}

#[test]
fn lock_retry_interruptions_and_contention_share_the_original_deadline() {
    let clock = LockClock::new();
    let deadline = clock.now.get() + Duration::from_millis(12);
    let mut attempts = 0;
    assert!(
        !wait_for_lock_with_clock(
            || {
                attempts += 1;
                assert!(attempts <= 4, "lock retries restarted the deadline");
                if attempts == 4 {
                    // The loop attempts the lock after sleeping to the deadline;
                    // account for that attempt's elapsed time without a real wait.
                    clock.advance(Duration::from_nanos(1));
                }
                if attempts % 2 == 1 {
                    Err(io::Error::from_raw_os_error(libc::EINTR))
                } else {
                    Ok(false)
                }
            },
            deadline,
            || clock.now.get(),
            |duration| clock.sleep(duration),
        )
        .unwrap()
    );
    assert_eq!(attempts, 4);
    assert_eq!(
        *clock.sleeps.borrow(),
        [
            LOCK_RETRY_INTERVAL,
            LOCK_RETRY_INTERVAL,
            Duration::from_millis(2)
        ]
    );
    assert_eq!(clock.now.get(), deadline + Duration::from_nanos(1));
}

#[test]
fn lock_retry_delay_or_oversleep_can_expire_before_a_third_attempt() {
    for delay_in_attempt in [true, false] {
        let clock = LockClock::new();
        let deadline = clock.now.get() + Duration::from_millis(40);
        let mut attempts = 0;
        assert!(
            !wait_for_lock_with_clock(
                || {
                    attempts += 1;
                    assert!(attempts <= 3);
                    if delay_in_attempt {
                        clock.advance(Duration::from_millis(45));
                    }
                    if attempts < 3 {
                        Err(io::Error::from_raw_os_error(libc::EINTR))
                    } else {
                        Ok(true)
                    }
                },
                deadline,
                || clock.now.get(),
                |duration| {
                    clock.sleep(duration);
                    clock.advance(Duration::from_millis(45));
                },
            )
            .unwrap()
        );
        assert_eq!(attempts, if delay_in_attempt { 1 } else { 2 });
        assert_eq!(
            *clock.sleeps.borrow(),
            if delay_in_attempt {
                vec![]
            } else {
                vec![LOCK_RETRY_INTERVAL]
            }
        );
        assert!(clock.now.get() > deadline);
    }
}

#[test]
fn lock_retry_errors_other_than_interrupted_are_immediate() {
    let mut attempts = 0;
    let error = wait_for_lock_with_clock(
        || {
            attempts += 1;
            Err(io::Error::from_raw_os_error(libc::EPERM))
        },
        Instant::now() + CLEANUP_LOCK_BUDGET,
        || panic!("non-interrupted errors must not consult the clock"),
        |_| panic!("non-interrupted errors must not sleep"),
    )
    .unwrap_err();
    assert_eq!(attempts, 1);
    assert_eq!(error.raw_os_error(), Some(libc::EPERM));
}

#[test]
fn startup_and_detached_recovery_do_not_wait_forever_for_registry_owner() {
    let mut fixture = Fixture::new();
    let global = regular(
        fixture.work.root.as_raw_fd(),
        OsStr::new(".global.lock"),
        false,
    )
    .unwrap();
    assert!(lock_until(&global, Instant::now() + CLEANUP_LOCK_BUDGET).unwrap());
    let started = Instant::now();
    assert!(Workspace::at(fixture.root.clone(), &[]).is_err());
    assert!(started.elapsed() < CLEANUP_LOCK_BUDGET + Duration::from_millis(500));
    let started = Instant::now();
    assert!(fixture.work.recovery().unwrap().recover().is_err());
    assert!(started.elapsed() < CLEANUP_LOCK_BUDGET + Duration::from_millis(500));
    assert_eq!(
        fs::read(fixture.work.path.join("valuable")).unwrap(),
        b"keep"
    );
    drop(global);
    fixture.work.close().unwrap();
}

#[test]
fn failed_close_drop_does_not_spend_another_budget_and_recovery_remains_idempotent() {
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("ff-profile-drop-{}", random_token().unwrap()));
    let mut work = Workspace::at(root.clone(), &[]).unwrap();
    let path = work.path.clone();
    fs::write(path.join("valuable"), b"keep").unwrap();
    let recovery = work.recovery().unwrap();
    let redundant = work.recovery().unwrap();
    let mut window = ForkWindow::new(work.lease_fd(), true);
    let mut child = window.exec();
    let result = work.close();
    let started = Instant::now();
    drop(work);
    let drop_elapsed = started.elapsed();
    let payload = fs::read(path.join("valuable"));
    drop(child.stdin.take());
    assert!(child.finish().unwrap().success());
    assert!(result.is_err());
    assert!(
        drop_elapsed < Duration::from_millis(250),
        "{drop_elapsed:?}"
    );
    assert_eq!(payload.unwrap(), b"keep");
    recovery.recover().unwrap();
    redundant.recover().unwrap();
    assert!(!path.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn replaced_named_lease_cannot_bypass_the_actual_owners_lock() {
    let mut fixture = Fixture::new();
    let mut window = ForkWindow::new(fixture.work.lease_fd(), true);
    let mut child = window.exec();
    let lease = fixture.work.path.parent().unwrap().join("lease");
    let retained = fixture.root.join("retained-lease");
    fs::rename(&lease, &retained).unwrap();
    let replacement = regular(libc::AT_FDCWD, lease.as_os_str(), true).unwrap();
    write_marker(&replacement, WORKSPACE_LEASE_MAGIC).unwrap();
    let recovery = fixture.work.recovery().unwrap();
    assert!(fixture.work.close().is_err());
    assert!(recovery.recover().is_err());
    let payload = fs::read(fixture.work.path.join("valuable"));
    drop(child.stdin.take());
    assert!(child.finish().unwrap().success());
    assert_eq!(payload.unwrap(), b"keep");
    fs::remove_file(&lease).unwrap();
    fs::rename(retained, lease).unwrap();
    fixture.work.close().unwrap();
}

#[test]
fn recovery_accepts_another_reaper_finishing_during_registry_wait() {
    let mut fixture = Fixture::new();
    let recovery = fixture.work.recovery().unwrap();
    let global = global_lock(&fixture.work.root, Instant::now() + CLEANUP_LOCK_BUDGET).unwrap();
    let owner = regular(fixture.work.root.as_raw_fd(), OsStr::new(".owner"), false).unwrap();
    fixture.work.lease.take();
    let waiting = std::thread::spawn(move || recovery.recover());
    std::thread::sleep(Duration::from_millis(50));
    reap(
        &fixture.work.root,
        &fixture.root,
        &fixture.work.campaign_name,
        None,
        (&global, &owner),
    )
    .unwrap();
    drop(global);
    waiting.join().unwrap().unwrap();
    assert!(!fixture.work.path.exists());
    fixture.work.closed = true;
}

#[test]
fn concurrent_workspace_closes_and_startup_serialize_without_deleting_live_owner() {
    let fixture = Fixture::new();
    let mut second = Workspace::at(fixture.root.clone(), &[]).unwrap();
    let first = fixture.work.path.clone();
    let root = fixture.root.clone();
    let startup = std::thread::spawn(move || {
        let mut third = Workspace::at(root, &[]).unwrap();
        assert!(first.exists());
        third.close().unwrap();
    });
    let close = std::thread::spawn(move || second.close());
    close.join().unwrap().unwrap();
    startup.join().unwrap();
    assert_eq!(
        fs::read(fixture.work.path.join("valuable")).unwrap(),
        b"keep"
    );
}

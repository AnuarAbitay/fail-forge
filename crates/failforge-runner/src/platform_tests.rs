use super::*;
use std::io::Read;
use std::process::Stdio;

#[cfg(target_os = "macos")]
#[path = "platform_tests/fork_exit_fixture.rs"]
mod fork_exit_fixture;

fn fixture(script: &str) -> ManagedChild {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    ManagedChild::spawn(&mut command).expect("bounded fixture must spawn")
}

fn wait_for_observation(child: &mut ManagedChild) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if child.observe().expect("owned child must remain observable") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture exceeded observation deadline"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn nonreaping_observation_preserves_real_status_and_retires_signalling() {
    // No descendants and an immediate finite exit: even controller failure
    // cannot leave a long-lived fixture. ManagedChild guards assertion unwind.
    let mut child = fixture("exit 37");
    wait_for_observation(&mut child);
    assert!(child.observe().unwrap());
    assert!(!child.is_retired());
    assert!(matches!(
        child.group_state().unwrap(),
        GroupState::NoLiveMembers { .. }
    ));
    // A signal after observing leader completion must not consume its status.
    match child.signal(libc::SIGTERM) {
        Ok(_) => {}
        Err(error) => {
            // Darwin may reject a signal addressed to a zombie-only group.
            // The error must remain visible without consuming the exit status.
            assert_eq!(error.raw_os_error(), Some(libc::EPERM));
        }
    }
    let status = child.reap().unwrap();
    assert_eq!(status.code(), Some(37));
    assert_eq!(child.reap().unwrap(), status);
    assert!(child.is_retired());
    assert_eq!(
        child.signal(libc::SIGKILL).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(child.group_state().unwrap(), GroupState::Unknown);
}

#[cfg(target_os = "macos")]
#[test]
fn zombie_only_owned_group_signal_is_already_gone_without_a_cleanup_error() {
    // Darwin returns EPERM for a group whose only member is an unreaped zombie.
    // Retaining WNOWAIT ownership still pins the numeric ID. A fresh native
    // group census can prove there is no live target without consuming status.
    let mut child = fixture("exit 47");
    wait_for_observation(&mut child);
    assert!(matches!(
        child.group_state().unwrap(),
        GroupState::NoLiveMembers { zombies: 1 }
    ));
    assert!(
        !child.signal(libc::SIGTERM).unwrap(),
        "a zombie-only owned group has no live recipient"
    );
    assert!(!child.signal(libc::SIGKILL).unwrap());
    assert!(!child.is_retired());
    assert_eq!(child.reap().unwrap().code(), Some(47));
    assert!(child.is_retired());
}

#[cfg(target_os = "macos")]
#[test]
fn group_permission_error_requires_both_terminal_ownership_and_quiescent_census() {
    for evidence in [
        Ok((false, GroupSignalTargets::NoLiveMembers)),
        Ok((false, GroupSignalTargets::ExitingMembers)),
        Ok((true, GroupSignalTargets::LiveMembers)),
        Ok((true, GroupSignalTargets::Unknown)),
        Err(io::Error::from_raw_os_error(libc::ECHILD)),
        Err(io::Error::from_raw_os_error(libc::EINTR)),
        Err(io::Error::from_raw_os_error(libc::EACCES)),
    ] {
        let result = group_signal_failure(io::Error::from_raw_os_error(libc::EPERM), || evidence);
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EPERM));
    }
    let result = group_signal_failure(io::Error::from_raw_os_error(libc::EPERM), || {
        Ok((true, GroupSignalTargets::NoLiveMembers))
    });
    assert!(!result.unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn terminal_owned_group_with_only_inexit_recipients_does_not_fabricate_signal_failure() {
    // Native evidence in native-group-race-flags.log proves this exact state:
    // terminal owned leader, EPERM, non-zombie members carrying INEXIT, then
    // NoLiveMembers. This policy probe does not claim cleanup is complete;
    // ordinary group_state must keep those exiting members live until gone.
    let result = group_signal_failure(io::Error::from_raw_os_error(libc::EPERM), || {
        Ok((true, GroupSignalTargets::ExitingMembers))
    });
    assert!(!result.unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn two_signal_censuses_require_all_live_members_to_be_inexit_without_changing_liveness() {
    let exiting = GroupSnapshot {
        live: vec![10],
        zombies: vec![9],
        exiting: vec![10],
    };
    let gone = GroupSnapshot {
        live: vec![],
        zombies: vec![9, 10],
        exiting: vec![],
    };
    let ordinary = GroupSnapshot {
        live: vec![11],
        zombies: vec![9],
        exiting: vec![],
    };
    let mixed = GroupSnapshot {
        live: vec![10, 11],
        zombies: vec![9],
        exiting: vec![10],
    };
    for (first, second) in [(&exiting, &exiting), (&exiting, &gone), (&gone, &exiting)] {
        assert_eq!(
            classify_group_signal_targets(first, second),
            GroupSignalTargets::ExitingMembers
        );
    }
    for (first, second) in [
        (&exiting, &ordinary),
        (&ordinary, &exiting),
        (&exiting, &mixed),
        (&mixed, &exiting),
    ] {
        assert_eq!(
            classify_group_signal_targets(first, second),
            GroupSignalTargets::LiveMembers
        );
    }
    assert_eq!(exiting.live, [10], "INEXIT must remain live for cleanup");
    assert_eq!(exiting.zombies, [9], "INEXIT must not be relabelled zombie");
    let old_zombies = GroupSnapshot {
        zombies: vec![9],
        ..Default::default()
    };
    assert_eq!(
        classify_group_signal_targets(&gone, &old_zombies),
        GroupSignalTargets::Unknown
    );
    assert_eq!(
        classify_group_signal_targets(&gone, &gone),
        GroupSignalTargets::NoLiveMembers
    );
}

#[test]
fn group_missing_and_nonpermission_errors_do_not_request_a_quiescence_probe() {
    // ESRCH is already the kernel's missing-target answer. Errors such as
    // EINVAL must remain errors even if an unrelated census could be empty.
    let absent = group_signal_failure(io::Error::from_raw_os_error(libc::ESRCH), || {
        panic!("missing group must not require another native probe")
    });
    assert!(!absent.unwrap());
    for errno in [libc::EINVAL, libc::EIO, libc::EINTR, libc::EACCES] {
        let failed = group_signal_failure(io::Error::from_raw_os_error(errno), || {
            panic!("unrelated native error must not be normalized")
        });
        assert_eq!(failed.unwrap_err().raw_os_error(), Some(errno));
    }
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "finite macOS fork/exit race census; run explicitly for native evidence"]
fn native_fork_exit_group_signal_race_census() {
    // The descendant acknowledges readiness after fork, then waits in a shell
    // builtin. Releasing its input races native exit with SIGTERM without
    // signalling an external program while its dynamic loader is starting.
    // Each worker retains the sole unreaped leader through every group signal.
    let mut totals = [0usize; 11];
    let mut samples = Vec::new();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..3)
            .map(|_| {
                scope.spawn(|| {
                    let mut totals = [0usize; 11];
                    let mut samples = Vec::new();
                    for _ in 0..250 {
                        let mut fixture = fork_exit_fixture::ForkExitFixture::spawn()
                            .expect("fork/exit fixture must become ready within three seconds");
                        let before = fixture.child().group_state().unwrap();
                        assert!(
                            matches!(before, GroupState::LiveMembers { .. }),
                            "the ready descendant must still be live before release: {before:?}"
                        );
                        fixture.release();
                        let child = fixture.child_mut();
                        // SAFETY: terminal WNOWAIT above pins this sole
                        // owned child's PID and the original group ID.
                        totals[8] += 1;
                        let sent = unsafe { libc::kill(-child.pid, libc::SIGTERM) };
                        if sent == 0 {
                            totals[9] += 1;
                        } else {
                            let error = io::Error::last_os_error();
                            if error.raw_os_error() == Some(libc::EPERM) {
                                totals[0] += 1;
                                let mut proof = None;
                                let result = group_signal_failure(error, || {
                                    let terminal = child.observe()?;
                                    let group = child.group_state()?;
                                    proof = Some((terminal, group.clone()));
                                    Ok((terminal, child.group_signal_targets()?))
                                });
                                let index = match &proof {
                                    Some((true, GroupState::NoLiveMembers { .. })) => 1,
                                    Some((true, GroupState::Unknown)) => 2,
                                    Some((true, GroupState::LiveMembers { .. })) => 3,
                                    _ => 4,
                                };
                                totals[index] += 1;
                                let mut member_flags = Vec::new();
                                if index == 3 {
                                    for pid in group_snapshot(child.pid).unwrap().live {
                                        let mut info = std::mem::MaybeUninit::<
                                            libc::proc_bsdshortinfo,
                                        >::uninit(
                                        );
                                        let size = std::mem::size_of::<libc::proc_bsdshortinfo>();
                                        // SAFETY: same read-only native
                                        // flavor and exact-size storage as
                                        // production group_snapshot. No
                                        // signal or wait addresses this
                                        // descendant numerical PID.
                                        let bytes = unsafe {
                                            libc::proc_pidinfo(
                                                pid,
                                                libc::PROC_PIDT_SHORTBSDINFO,
                                                1,
                                                info.as_mut_ptr().cast(),
                                                size as i32,
                                            )
                                        };
                                        if bytes == size as i32 {
                                            // SAFETY: exact-size native
                                            // success initialized info.
                                            let info = unsafe { info.assume_init() };
                                            if info.pbsi_pid == pid as u32
                                                && info.pbsi_pgid == child.pid as u32
                                                && info.pbsi_status != libc::SZOMB
                                            {
                                                member_flags
                                                    .push((info.pbsi_status, info.pbsi_flags));
                                                totals[6] += 1;
                                                // Apple proc_info.h:
                                                // PROC_FLAG_INEXIT=4.
                                                if info.pbsi_flags & PROC_FLAG_INEXIT == 0 {
                                                    totals[7] += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                                std::thread::sleep(Duration::from_millis(2));
                                let later = child.group_state().unwrap();
                                if result.is_err()
                                    && matches!(later, GroupState::NoLiveMembers { .. })
                                {
                                    totals[5] += 1;
                                    if samples.len() < 8 {
                                        samples.push((before, proof, member_flags, later));
                                    }
                                }
                            } else {
                                assert_eq!(
                                    error.raw_os_error(),
                                    Some(libc::ESRCH),
                                    "unexpected native group SIGTERM failure: {error}"
                                );
                                totals[10] += 1;
                            }
                        }
                        assert!(child.reap().unwrap().success());
                    }
                    (totals, samples)
                })
            })
            .collect();
        for worker in workers {
            let (counts, examples) = worker.join().unwrap();
            for (total, count) in totals.iter_mut().zip(counts) {
                *total += count;
            }
            samples.extend(examples);
        }
    });
    assert_eq!(totals[8], 750, "every fixture must exercise native SIGTERM");
    assert_eq!(totals[9] + totals[0] + totals[10], totals[8]);
    println!(
        "native_group_race cases=750 signal_attempts={} signal_sent={} signal_missing={} eperm={} proof_no_live={} proof_unknown={} proof_live={} proof_error_or_unobserved={} rejected_then_no_live={} flags_non_zombie_members={} flags_without_inexit={} samples={samples:?}",
        totals[8],
        totals[9],
        totals[10],
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals[4],
        totals[5],
        totals[6],
        totals[7],
    );
}

#[test]
fn externally_consumed_status_retires_association_without_fabricating_a_reap() {
    // Consuming a managed status elsewhere violates the embedding contract.
    // Exercise that violation with a finite child and verify that the backend
    // permanently stops using its numerical PID rather than trusting old state.
    let mut child = fixture("exit 41");
    wait_for_observation(&mut child);
    let mut native_status = 0;
    // SAFETY: this is the still-owned direct child with a terminal WNOWAIT
    // observation. No competing waiter exists. This deliberate external wait
    // consumes status only to exercise loss of association; it cannot block.
    assert_eq!(
        unsafe { libc::waitpid(child.pid, &mut native_status, 0) },
        child.pid
    );
    assert!(libc::WIFEXITED(native_status));
    assert_eq!(libc::WEXITSTATUS(native_status), 41);

    assert_eq!(
        child.observe().unwrap_err().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert!(child.is_retired());
    assert!(child.child.is_none());
    assert!(child.reaped_status.is_none());
    assert_eq!(child.group_state().unwrap(), GroupState::Unknown);
    assert_eq!(
        child.observe().unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        child.signal(libc::SIGKILL).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        child.reap().unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    // Every operation after ECHILD above is rejected by local state. Drop has
    // no Child left to signal or hand off, and no more native PID calls follow.
}

#[test]
fn status_consumed_between_observation_and_reap_retires_the_failed_wait() {
    // Unlike the earlier ECHILD-observe probe, consume status after the final
    // successful observation and enter reap directly. This exercises the
    // native wait-error retirement branch and guards against a cached success.
    let mut child = fixture("exit 43");
    wait_for_observation(&mut child);
    let mut native_status = 0;
    // SAFETY: this direct child has a terminal WNOWAIT observation and no other
    // waiter. The test deliberately violates sole ownership with a nonblocking
    // terminal wait; it sends no PID or group signal.
    assert_eq!(
        unsafe { libc::waitpid(child.pid, &mut native_status, 0) },
        child.pid
    );
    assert!(libc::WIFEXITED(native_status));
    assert_eq!(libc::WEXITSTATUS(native_status), 43);

    assert_eq!(child.reap().unwrap_err().raw_os_error(), Some(libc::ECHILD));
    assert!(child.is_retired());
    assert!(child.child.is_none());
    assert!(child.reaped_status.is_none());
    assert_eq!(child.group_state().unwrap(), GroupState::Unknown);
    assert_eq!(
        child.reap().unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        child.signal(libc::SIGKILL).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    // After ECHILD, the remaining operations above inspect local retired state.
    // Drop has no child left and cannot reuse the consumed numerical PID.
}

#[test]
fn auto_reaping_sigchld_dispositions_are_rejected_before_spawn() {
    const CHILD_MODE: &str = "FAILFORGE_TEST_SIGCHLD_DISPOSITION";
    const TEST_NAME: &str =
        "platform::tests::auto_reaping_sigchld_dispositions_are_rejected_before_spawn";

    if let Some(mode) = std::env::var_os(CHILD_MODE) {
        // This branch runs in a filtered copy of the native test executable.
        // Changing SIGCHLD cannot affect the controller or unrelated test runs.
        // SAFETY: sigaction's integer/pointer native fields and mask accept
        // zero initialization. sigemptyset below initializes the mask.
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        match mode.to_str() {
            Some("ignore") => action.sa_sigaction = libc::SIG_IGN,
            Some("no-cldwait") => {
                action.sa_sigaction = libc::SIG_DFL;
                action.sa_flags = libc::SA_NOCLDWAIT;
            }
            _ => panic!("unexpected isolated SIGCHLD probe mode"),
        }
        // SAFETY: sa_mask points to writable native sigset_t storage.
        assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
        let mut previous = std::mem::MaybeUninit::<libc::sigaction>::uninit();
        // SAFETY: all pointers address properly sized native action storage.
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, previous.as_mut_ptr()) },
            0
        );
        struct RestoreSigchld(libc::sigaction);
        impl Drop for RestoreSigchld {
            fn drop(&mut self) {
                // SAFETY: this saved action was initialized by sigaction and
                // belongs only to the isolated child process's signal state.
                unsafe { libc::sigaction(libc::SIGCHLD, &self.0, std::ptr::null_mut()) };
            }
        }
        // SAFETY: successful sigaction initialized the complete old action.
        let _restore = RestoreSigchld(unsafe { previous.assume_init() });
        // A missing executable would yield NotFound if command.spawn were
        // reached. Unsupported must instead originate from the disposition
        // preflight, so the test never needs a long-lived subordinate process.
        let mut command = Command::new("/failforge-auto-reap-probe-must-not-exec");
        let error = match ManagedChild::spawn(&mut command) {
            Ok(_) => panic!("spawn accepted an auto-reaping SIGCHLD disposition"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            "SIGCHLD disposition discards child status"
        );
        return;
    }

    // The controller owns only the isolated test executable. Kill is permitted
    // exclusively after try_wait proves that exact Child remains unreaped;
    // ECHILD/errors and cached terminal status never lead to a numeric kill.
    struct BoundedProbe(Child);
    impl Drop for BoundedProbe {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
                let deadline = Instant::now() + Duration::from_secs(2);
                while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
    for mode in ["ignore", "no-cldwait"] {
        let mut child = BoundedProbe(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST_NAME, "--test-threads=1"])
                .env(CHILD_MODE, mode)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "isolated {mode} SIGCHLD probe failed: {status}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "isolated {mode} SIGCHLD probe deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn live_leader_cannot_be_reaped_and_forced_status_remains_real() {
    // The parent-held writer keeps this direct shell blocked in its read
    // builtin. Controller death closes the writer and gives the child EOF;
    // scheduling delays therefore neither determine status nor leak a sleeper.
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read ignored"]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    assert_eq!(child.reap().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    child.signal(libc::SIGKILL).unwrap();
    wait_for_observation(&mut child);
    let status = child.reap().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert!(child.is_retired());
}

#[test]
fn forced_stop_targets_owned_leader_after_it_joins_the_controller_group() {
    // The pre-exec callback runs after Command's process_group(0) setup. It
    // moves only this direct fixture into the test controller's existing
    // group; successful spawn confirms that the move already completed.
    // A held stdin pipe keeps the shell blocked without readiness sleeps.
    // SAFETY: getpgrp takes no arguments and reads this controller's group ID.
    let controller_group = unsafe { libc::getpgrp() };
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read ignored"]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setpgid is async-signal-safe, takes only integers, and modifies
    // the fork child. No allocation or Rust lock is used by this callback.
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, controller_group) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    // SAFETY: getpgid reads membership of the still owned, unreaped child.
    assert_eq!(unsafe { libc::getpgid(child.pid) }, controller_group);
    assert_ne!(child.pid, controller_group);
    assert!(!child.observe().unwrap());
    assert!(matches!(
        child.group_state().unwrap(),
        GroupState::NoLiveMembers { .. }
    ));
    assert!(!child.signal(libc::SIGTERM).unwrap());
    assert!(child.signal(libc::SIGKILL).unwrap());
    wait_for_observation(&mut child);
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(child.reap().unwrap().signal(), Some(libc::SIGKILL));
    assert!(child.is_retired());
    assert_eq!(
        child.signal(libc::SIGKILL).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn native_drop_hands_the_child_to_the_background_reaper() {
    // A held stdin writer is an independent escape hatch: assertion unwind
    // closes it, so the shell's read builtin gets EOF without a stale PID kill.
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read ignored"]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    let _held_stdin = child.child.as_mut().unwrap().stdin.take().unwrap();
    let pid = child.pid;
    assert!(!child.observe().unwrap());
    drop(child);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // This is a non-consuming observation only. The background reaper
        // remains the sole wait-status owner; this test never signals this PID.
        match observe_pid(pid) {
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => break,
            Ok(_) => {}
            Err(error) => panic!("fallback observation failed: {error:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "background reaper did not consume child status"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // Once ECHILD is observed, no further numerical operation uses this PID.
}

#[test]
fn emergency_handoff_reaps_a_queued_live_child_after_controlled_eof() {
    // Unlike ManagedChild::Drop, this handoff sends no kill first. Keeping the
    // sole stdin writer open guarantees that initial try_wait sees a live
    // shell blocked in its read builtin, so the background queue is exercised.
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read ignored"]);
    command.env_clear().process_group(0);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let held_stdin = child.stdin.take().unwrap();
    let pid = child.id() as libc::pid_t;
    handoff_reaper(child);
    // This is also independent cleanup on assertion unwind: the finite shell
    // cannot remain blocked after the controller releases its only writer.
    drop(held_stdin);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        // WNOWAIT never competes for status ownership with the reaper thread.
        match observe_pid(pid) {
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => break,
            Ok(_) => {}
            Err(error) => panic!("queued fallback observation failed: {error:?}"),
        }
        assert!(
            Instant::now() < deadline,
            "queued background reaper did not consume status"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    // No signal, wait, or other numerical PID operation follows ECHILD.
}

#[test]
fn stdout_and_stderr_ownership_can_only_be_taken_once() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf '%s' \"$$\""]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ManagedChild::spawn(&mut command).unwrap();
    let stdout = child
        .take_stdout()
        .expect("stdout was configured as a pipe");
    let stderr = child
        .take_stderr()
        .expect("stderr was configured as a pipe");
    assert!(child.take_stdout().is_none());
    assert!(child.take_stderr().is_none());
    wait_for_observation(&mut child);
    let mut reported_pid = Vec::new();
    std::fs::File::from(stdout)
        .read_to_end(&mut reported_pid)
        .unwrap();
    assert_eq!(reported_pid, child.id().to_string().as_bytes());
    drop(stderr);
    assert!(child.reap().unwrap().success());
}

#[test]
fn proc_stat_handles_names_with_spaces_parentheses_and_non_utf8() {
    for bytes in [
        b"123 (name with spaces) S 1 42 42".as_slice(),
        b"123 (a) b)c) S 1 42 42".as_slice(),
        b"123 (invalid-\xff) S 1 42 42".as_slice(),
        b"123 (line\nname) S 1 42 42".as_slice(),
    ] {
        assert_eq!(parse_proc_stat(bytes).unwrap(), (42, b'S'));
    }
    assert_eq!(parse_proc_stat(b"2 (kthreadd) S 0 0 0").unwrap(), (0, b'S'));
    assert_eq!(
        parse_proc_stat(b"123 (tool) Z 1 42 42").unwrap(),
        (42, b'Z')
    );
}

#[test]
fn malformed_proc_stat_never_becomes_evidence_of_an_empty_group() {
    for bytes in [
        b"".as_slice(),
        b"123 missing-comm S 1 42".as_slice(),
        b"123 (name) SS 1 42".as_slice(),
        b"123 (name) S 1".as_slice(),
        b"123 (name) S 1 invalid".as_slice(),
        b"123 (name) S 1 -1".as_slice(),
        b"123 (name) S 1 99999999999999999999".as_slice(),
    ] {
        assert_eq!(
            parse_proc_stat(bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

#[test]
fn procfs_namespace_requires_one_matching_tgid() {
    for status in [
        b"NStgid:\t77\n".as_slice(),
        b"Name:\tinvalid-\xff\nNStgid: 77\r\nNSpid:\t77\n".as_slice(),
        b"NStgid:\t 77 \t".as_slice(),
    ] {
        assert!(procfs_namespace_matches(status, 77).unwrap());
    }
    assert!(!procfs_namespace_matches(b"NStgid:\t78\n", 77).unwrap());
}

#[test]
fn procfs_namespace_rejects_ancestor_and_coincident_identifiers() {
    for status in [
        b"NStgid:\t177 77\n".as_slice(),
        b"NStgid:\t77 77\n".as_slice(),
        b"NStgid:\t77 17\n".as_slice(),
        b"NStgid:\t177 17 77\n".as_slice(),
    ] {
        assert!(!procfs_namespace_matches(status, 77).unwrap());
    }
}

#[test]
fn procfs_namespace_fails_closed_on_missing_or_malformed_proof() {
    for status in [b"".as_slice(), b"Name:\ttool\nNSpid:\t77\n".as_slice()] {
        assert_eq!(
            procfs_namespace_matches(status, 77).unwrap_err().kind(),
            io::ErrorKind::Unsupported,
        );
    }
    for status in [
        b"NStgid:\n".as_slice(),
        b"NStgid:\t0\n".as_slice(),
        b"NStgid:\t-77\n".as_slice(),
        b"NStgid:\t+77\n".as_slice(),
        b"NStgid:\tunknown\n".as_slice(),
        b"NStgid:\t77 unknown\n".as_slice(),
        b"NStgid:\t99999999999999999999999\n".as_slice(),
        b"NStgid:\t77\nNStgid:\t77\n".as_slice(),
    ] {
        assert_eq!(
            procfs_namespace_matches(status, 77).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
    }
}

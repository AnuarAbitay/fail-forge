//! A ready descendant with no intervening exec for the native exit/signal race.

use super::super::ManagedChild;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

const OBSERVATION_BUDGET: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(2);
// Every command below is a shell builtin. In particular, the descendant never
// enters an executable loader between fork and the intended exit/signal race.
// FD 3 explicitly preserves the control pipe: a background job's default stdin
// can otherwise be /dev/null. The five-second read timeout is an independent
// lifetime bound, not an extension of the three-second readiness deadline.
const SCRIPT: &str = "exec 3<&0; (printf R; IFS= read -r -t 5 release <&3; exit 0) & exit 0";

pub(super) struct ForkExitFixture {
    // Field order releases EOF before the leader's ownership guard runs on
    // either an ordinary drop, a construction error, or assertion unwind.
    release: Option<ChildStdin>,
    leader: ManagedChild,
}

impl ForkExitFixture {
    pub(super) fn spawn() -> io::Result<Self> {
        let (fixture, ready, started) = Self::spawn_script(SCRIPT)?;
        fixture.await_ready(ready, started + OBSERVATION_BUDGET)
    }

    pub(super) fn child(&self) -> &ManagedChild {
        &self.leader
    }

    pub(super) fn child_mut(&mut self) -> &mut ManagedChild {
        &mut self.leader
    }

    pub(super) fn release(&mut self) {
        drop(self.release.take());
    }

    fn spawn_script(script: &str) -> io::Result<(Self, OwnedFd, Instant)> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let leader = ManagedChild::spawn(&mut command)?;
        let started = Instant::now();
        // Establish the ownership guard before configuring or extracting any
        // pipe. Every subsequent error still owns the exact unreaped leader.
        let mut fixture = Self {
            release: None,
            leader,
        };
        fixture.release = Some(
            fixture
                .leader
                .child
                .as_mut()
                .and_then(|child| child.stdin.take())
                .ok_or_else(|| io::Error::other("fixture control pipe is missing"))?,
        );
        let ready = fixture
            .leader
            .take_stdout()
            .ok_or_else(|| io::Error::other("fixture readiness pipe is missing"))?;
        set_nonblocking(&ready)?;
        Ok((fixture, ready, started))
    }

    fn await_ready(mut self, ready_pipe: OwnedFd, deadline: Instant) -> io::Result<Self> {
        let mut ready = false;
        loop {
            if !ready {
                match read_byte(&ready_pipe)? {
                    Some(b'R') => ready = true,
                    Some(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "fixture returned a malformed readiness marker",
                        ));
                    }
                    None => {}
                }
            }
            let terminal = self.leader.observe()?;
            if ready && terminal {
                return Ok(self);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "fixture exceeded observation deadline: ready={ready}, terminal={terminal}"
                    ),
                ));
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline - now));
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    // SAFETY: the borrowed OwnedFd keeps this exact pipe descriptor valid for
    // both integer-only fcntl calls. Existing descriptor flags are preserved.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: same valid borrowed descriptor and its previously read flags.
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_byte(fd: &OwnedFd) -> io::Result<Option<u8>> {
    let mut byte = 0u8;
    // SAFETY: the borrowed descriptor remains live, and byte is writable
    // storage for exactly the one byte requested from this nonblocking pipe.
    let bytes = unsafe { libc::read(fd.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
    match bytes {
        1 => Ok(Some(byte)),
        0 => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "fixture closed its output before readiness",
        )),
        -1 => {
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(error),
            }
        }
        _ => unreachable!("one-byte read returned an impossible length"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result::GroupState;

    fn wait_until_quiescent(fixture: &ForkExitFixture, budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            if matches!(
                fixture.child().group_state().unwrap(),
                GroupState::NoLiveMembers { .. }
            ) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "fixture descendant remained live"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn wait_for_output_eof(output: &OwnedFd) {
        let deadline = Instant::now() + OBSERVATION_BUDGET;
        loop {
            match read_byte(output) {
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return,
                Ok(None) => {}
                Ok(Some(byte)) => panic!("unexpected extra fixture output: {byte}"),
                Err(error) => panic!("cleanup pipe observation failed: {error}"),
            }
            assert!(Instant::now() < deadline, "fixture output remained open");
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn assert_protocol_error(script: &str, budget: Duration, kind: io::ErrorKind) {
        let (fixture, ready, started) = ForkExitFixture::spawn_script(script).unwrap();
        // This read end observes only the exact fixture's inherited writers.
        // It remains a valid lifetime oracle after the ownership guard drops;
        // no numerical PID query can accidentally address a reused child ID.
        let output = ready.try_clone().unwrap();
        let error = match fixture.await_ready(ready, started + budget) {
            Ok(_) => panic!("invalid fixture protocol was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), kind, "{error}");
        wait_for_output_eof(&output);
    }

    #[test]
    fn ready_fixture_keeps_a_live_descendant_and_pinned_terminal_leader_until_eof() {
        let mut fixture = ForkExitFixture::spawn().unwrap();
        assert!(fixture.child_mut().observe().unwrap());
        assert!(!fixture.child().is_retired());
        assert!(matches!(
            fixture.child().group_state().unwrap(),
            GroupState::LiveMembers { count: 1 }
        ));
        fixture.release();
        fixture.release();
        wait_until_quiescent(&fixture, OBSERVATION_BUDGET);
        assert_eq!(fixture.child_mut().reap().unwrap().code(), Some(0));
        assert!(fixture.child().is_retired());
    }

    #[test]
    fn missing_readiness_is_an_eof_error_and_releases_the_owned_child() {
        assert_protocol_error("exit 0", OBSERVATION_BUDGET, io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn malformed_readiness_is_rejected_and_cleans_up_a_live_leader() {
        assert_protocol_error(
            "printf X; IFS= read -r -t 5 release; exit 0",
            OBSERVATION_BUDGET,
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn absent_readiness_times_out_and_cleans_up_a_live_leader() {
        assert_protocol_error(
            "IFS= read -r -t 5 release; exit 0",
            Duration::from_millis(25),
            io::ErrorKind::TimedOut,
        );
    }

    #[test]
    fn readiness_does_not_substitute_for_terminal_leader_observation() {
        assert_protocol_error(
            "printf R; IFS= read -r -t 5 release; exit 0",
            Duration::from_millis(100),
            io::ErrorKind::TimedOut,
        );
    }

    #[test]
    fn assertion_unwind_releases_the_descendant_and_closes_the_owned_output() {
        let (fixture, ready, started) = ForkExitFixture::spawn_script(SCRIPT).unwrap();
        let output = ready.try_clone().unwrap();
        let fixture = fixture
            .await_ready(ready, started + OBSERVATION_BUDGET)
            .unwrap();
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _owned_fixture = fixture;
            panic!("controlled fixture assertion unwind");
        }));
        assert!(unwind.is_err());
        wait_for_output_eof(&output);
    }

    #[test]
    fn independent_finite_fallback_stops_a_descendant_without_controller_eof() {
        let mut fixture = ForkExitFixture::spawn().unwrap();
        assert!(fixture.release.is_some());
        wait_until_quiescent(&fixture, Duration::from_secs(6));
        assert!(fixture.release.is_some(), "controller EOF was not released");
        assert_eq!(fixture.child_mut().reap().unwrap().code(), Some(0));
    }
}

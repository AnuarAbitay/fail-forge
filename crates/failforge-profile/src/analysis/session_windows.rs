//! Native helper owned by Runner's Windows Job Object, private named-pipe
//! cancellation, and the same strict transcript validator as Unix.
use super::transcript::{Transcript, with_diagnostics};
use super::workspaces::{Recovery, Workspace};
use super::{Cancellation, FamilyApplicability, Runtime, failure};
use crate::ProfileError;
use failforge_dart::MAX_LINE_BYTES;
use failforge_runner::{CapturePolicy, CommandSpec, EnvSpec, OutputMode, Runner, TimeoutPolicy};
use failforge_workspace::windows_fs::{self as win, PrivateSecurity, random_token};
use serde_json::Value;
use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

const CANCEL_ENV: &str = "FAILFORGE_ADAPTER_CANCEL_PIPE";
struct NativeOwner {
    runner: Option<Runner>,
    handle: Option<failforge_runner::RunHandle>,
    control: Option<NamedPipeServer>,
    recovery: Option<Recovery>,
}
impl Drop for NativeOwner {
    fn drop(&mut self) {
        self.control.take(); // Broken pipe is observable cancellation in helper.
        let Some(mut handle) = self.handle.take() else {
            return;
        };
        // A dropped discovery future must not leave helper or Dart descendants
        // running. Terminating this private job also releases native lease locks.
        handle.force_cancel(failforge_runner::CancelReason::HandleDropped);
        let runner = self.runner.take();
        let recovery = self.recovery.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = handle.wait().await;
                if let Some(runner) = runner {
                    let _ = runner.shutdown().await;
                }
                if let Some(recovery) = recovery {
                    let _ = recovery.recover();
                }
            });
        }
        // Runtime teardown drops the supervisor's Job Object, killing the tree.
        // Stale registry recovery remains protected by its independent lease.
    }
}
fn control_pipe() -> io::Result<(String, NamedPipeServer)> {
    let name = format!(
        r"\\.\pipe\failforge-profile-{}{}",
        random_token()?,
        random_token()?
    );
    let mut security = PrivateSecurity::new()?;
    // SAFETY: the current-account security descriptor remains live through the
    // creation call; the private server is noninheritable and rejects remote users.
    let pipe = unsafe {
        ServerOptions::new()
            .access_inbound(false)
            .access_outbound(true)
            .max_instances(1)
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(&name, security.as_ptr().cast())?
    };
    Ok((name, pipe))
}
async fn request_cancel(
    control: &mut Option<NamedPipeServer>,
    signal: i32,
) -> Result<(), ProfileError> {
    if let Some(mut control) = control.take() {
        let byte = if signal == libc::SIGTERM { b'T' } else { b'I' };
        match control.write_all(&[byte]).await {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) async fn discover(
    runtime: &Runtime,
    package: &Path,
    workspace: &Workspace,
    payload: &Value,
    family: &str,
    requested: &[String],
    cancel: &Cancellation,
) -> Result<FamilyApplicability, ProfileError> {
    cancel.check()?;
    let source = payload["rules"]["includePaths"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .ok_or_else(|| failure("analysis request has no selected file"))?;
    let output_name = format!("discovery-{family}");
    let workspace_directory = win::directory(&workspace.path)?;
    let output_directory =
        win::create_directory_at(&workspace_directory, OsStr::new(&output_name))?;
    let output = workspace.path.join(&output_name);
    let payload_path = output.join("payload.json");
    let payload_raw =
        serde_json::to_vec(payload).map_err(|_| failure("cannot serialize analysis request"))?;
    let mut payload_file =
        win::regular_at(&output_directory, OsStr::new("payload.json"), true, true)?;
    payload_file.write_all(&payload_raw)?;
    payload_file.sync_all()?;
    win::require_private(&payload_file, false)?;
    win::same_entry(
        &workspace_directory,
        OsStr::new(&output_name),
        &output_directory,
    )?;
    let (name, control) = control_pipe()?;
    let mut env = EnvSpec {
        clear: true,
        set: runtime.environment(),
        remove: Default::default(),
    };
    env.set.insert(CANCEL_ENV.into(), name.into());
    let spec = CommandSpec {
        program: runtime.helper.clone().into(),
        args: vec![
            runtime
                .dart_sdk
                .join(failforge_config::platform::DART_BIN)
                .into(),
            runtime.adapter.clone().into(),
            payload_path.into(),
        ],
        cwd: package.into(),
        env,
        timeout_policy: TimeoutPolicy {
            timeout: None,
            grace_period: Duration::from_millis(50),
            forced_wait: Duration::from_secs(5),
            output_drain: Duration::from_secs(5),
        },
        capture_policy: CapturePolicy {
            mode: OutputMode::StreamAndCapture,
            stream_queue_bytes: MAX_LINE_BYTES * 2,
            tail_bytes_per_stream: 4097,
            capture_dir: Some(output.join("runner-capture")),
            ..Default::default()
        },
    };
    let runner = Runner::new();
    let lease = workspace.native_lease()?;
    let handle = runner
        .start_with_workspace_lease(spec, &lease)
        .map_err(|error| failure(format!("cannot start native helper: {error}")))?;
    drop(lease);
    let mut owner = NativeOwner {
        runner: Some(runner),
        handle: Some(handle),
        control: Some(control),
        recovery: Some(workspace.recovery()?),
    };
    // Connection is bounded; the helper opens cancellation before adapter spawn.
    failforge_runner::awake_timeout(
        Duration::from_secs(10),
        owner.control.as_ref().unwrap().connect(),
    )
    .await
    .map_err(|_| failure("native helper cancellation connection timed out"))??;
    let mut stream = owner.handle.as_mut().unwrap().take_stream().unwrap();
    let mut transcript = Transcript::new(package, family, requested, source);
    let mut line = Vec::new();
    let mut error = None;
    let mut exit = None;
    let mut eof = false;
    let mut signalled = false;
    while !eof || exit.is_none() {
        tokio::select! {
            signal = cancel.cancelled(), if !signalled => {
                signalled = true;
                if let Err(problem) = request_cancel(&mut owner.control, signal).await { error.get_or_insert(problem); }
            },
            chunk = stream.recv(), if !eof => {
                match chunk {
                    None => eof = true,
                    Some(mut chunk) => {
                        if chunk.stream == failforge_runner::OutputStream::Stdout && error.is_none() {
                            for byte in chunk.bytes() {
                                if *byte == b'\n' {
                                    if let Err(problem) = transcript.accept(&line) { error = Some(problem); break; }
                                    line.clear();
                                } else if line.len() >= MAX_LINE_BYTES {
                                    error = Some(failure("adapter campaign record exceeds eight MiB")); break;
                                } else { line.push(*byte); }
                            }
                        }
                        chunk.mark_displayed(chunk.bytes().len());
                    }
                }
            },
            outcome = owner.handle.as_mut().unwrap().wait(), if exit.is_none() => { exit = Some(outcome); },
        }
        if error.is_some() && owner.control.is_some() {
            let _ = request_cancel(&mut owner.control, libc::SIGINT).await;
        }
    }
    owner.control.take();
    let result = exit
        .unwrap()
        .map_err(|error| failure(format!("native helper process cleanup failed: {error}")))?;
    if stream.dropped_bytes() != (0, 0)
        || result.stdout.stream_dropped_bytes != 0
        || result.stderr.stream_dropped_bytes != 0
    {
        error.get_or_insert_with(|| failure("adapter campaign stream lost bytes"));
    }
    win::same_entry(
        &workspace_directory,
        OsStr::new(&output_name),
        &output_directory,
    )?;
    let mut stderr_file =
        win::regular_at(&output_directory, OsStr::new("adapter.stderr"), true, true)?;
    stderr_file.write_all(&result.stderr.tail)?;
    stderr_file.sync_all()?;
    win::require_private(&stderr_file, false)?;
    owner.handle.take();
    owner
        .runner
        .take()
        .unwrap()
        .shutdown()
        .await
        .map_err(|error| failure(format!("native helper shutdown failed: {error}")))?;
    owner.recovery.take(); // Normal caller closes workspace after input checks.
    if error.is_none() && !line.is_empty() {
        error = Some(failure(
            "adapter campaign ended with an unterminated record",
        ));
    }
    if let Some(error) = error {
        return Err(with_diagnostics(error, &output, family));
    }
    transcript
        .finish(
            result
                .status
                .ok_or_else(|| failure("native helper exit status unavailable"))?,
            cancel.signum(),
        )
        .map_err(|error| with_diagnostics(error, &output, family))
}

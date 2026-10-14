//! Run external Linux/macOS commands on an existing Tokio runtime.
//!
//! Configuration preserves native OS strings and never invokes an implicit
//! shell. A nonzero child exit is an ordinary result. Cancellation, timeout,
//! real OS status, capture errors and cleanup evidence are reported separately.
//!
//! ```
//! use failforge_runner::{CommandSpec, CompletionCause, Runner};
//!
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let runner = Runner::new();
//! let spec = CommandSpec {
//!     program: "/usr/bin/printf".into(),
//!     args: vec!["%s".into(), "hello".into()],
//!     cwd: std::env::current_dir()?,
//!     env: Default::default(),
//!     timeout_policy: Default::default(),
//!     capture_policy: Default::default(),
//! };
//! let mut handle = runner.start(spec)?;
//! let outcome = handle.wait().await;
//! runner.shutdown().await?;
//! let result = outcome?;
//! assert_eq!(result.cause, CompletionCause::ProcessCompleted);
//! assert!(result.status.unwrap().success());
//! assert_eq!(result.stdout.tail, b"hello");
//! assert!(result.cleanup.is_complete());
//! # Ok(())
//! # }
//! ```
//!
//! Keep the runtime alive through [`Runner::shutdown`]. Process groups are
//! observational cleanup scopes; descendants can escape them. Runtime abort,
//! controller SIGKILL, and blocking OS/filesystem calls have explicit limits.

mod output;
mod platform;
mod prepare;
mod result;
mod spec;
mod supervisor;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("FailForge's first-stage process backend supports Linux and macOS only");

pub use output::{OutputStream, StreamChunk, StreamReceiver};
pub use result::{
    CancelReason, CaptureIssue, CaptureSummary, CleanupReport, CommandResult, CompletionCause,
    GroupState, RunTimings, RunnerError, SignalOutcome, SignalRecord,
};
pub use spec::{CapturePolicy, CommandSpec, EnvSpec, OutputMode, SpecError, TimeoutPolicy};
pub use supervisor::{RunHandle, RunOutcome, Runner};

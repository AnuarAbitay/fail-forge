//! Private, leased Dart namespace operations for the transitional controller.
#[cfg(unix)]
use std::io::Write;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::ExitCode;

#[cfg(unix)]
use crate::signals::{SignalKind, signal};
use failforge_dart::namespace::NamespaceError;
#[cfg(unix)]
use failforge_dart::namespace::isolate_subject;
#[cfg(unix)]
use failforge_runner::WorkspaceLease;
#[cfg(unix)]
use failforge_workspace::Cancellation;
#[cfg(unix)]
use failforge_workspace::parse_json;
#[cfg(unix)]
use serde::Deserialize;
#[cfg(unix)]
use serde_json::Value;
#[cfg(unix)]
use serde_json::json;

#[cfg(unix)]
use crate::namespace_planner::{PlanProject, PlanRuntime};

#[cfg(unix)]
#[derive(Debug, Deserialize)]
#[serde(
    tag = "operation",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Request {
    Isolate {
        project: PathBuf,
        copy: PathBuf,
        framework: PathBuf,
        alias: String,
        namespace_plan: Option<Value>,
    },
    Plan {
        project: PlanProject,
        directory: PathBuf,
        runtime: PlanRuntime,
    },
}

pub(crate) fn invalid(message: &str) -> NamespaceError {
    NamespaceError::Invalid(message.into())
}

pub(crate) fn absolute(path: &Path) -> Result<(), NamespaceError> {
    if path.is_absolute() && path.to_str().is_some_and(|text| !text.contains('\0')) {
        Ok(())
    } else {
        Err(invalid("namespace paths must be absolute Unicode paths"))
    }
}

#[cfg(unix)]
fn decode(raw: &[u8]) -> Result<Request, NamespaceError> {
    let request: Request = serde_json::from_value(parse_json(raw)?)
        .map_err(|_| invalid("invalid private namespace request"))?;
    match &request {
        Request::Isolate {
            project,
            copy,
            framework,
            ..
        } => {
            for path in [project, copy, framework] {
                absolute(path)?;
            }
        }
        Request::Plan {
            project,
            directory,
            runtime,
        } => {
            for path in [
                &project.root,
                directory,
                &runtime.dart_sdk,
                &runtime.adapter_root,
            ] {
                absolute(path)?;
            }
            if let Some(path) = &runtime.flutter_sdk {
                absolute(path)?;
            }
            if project.name.is_empty() || project.name.contains('\0') {
                return Err(invalid("invalid namespace subject name"));
            }
        }
    }
    Ok(request)
}

#[cfg(unix)]
async fn process(
    request: Request,
    lease: Option<WorkspaceLease>,
    readiness: Option<crate::readiness::Readiness>,
) -> Result<(), NamespaceError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancel = Cancellation::default();
    let listening = cancel.clone();
    let listener = tokio::spawn(async move {
        let signum = tokio::select! { _ = interrupt.recv() => libc::SIGINT, _ = terminate.recv() => libc::SIGTERM };
        listening.request(signum);
    });
    if let Some(readiness) = readiness {
        readiness.acknowledge()?;
    }
    let result = match request {
        Request::Isolate {
            project,
            copy,
            framework,
            alias,
            namespace_plan,
        } => {
            let task_cancel = cancel.clone();
            tokio::task::spawn_blocking(move || {
                match isolate_subject(
                    &project,
                    &copy,
                    &framework,
                    &alias,
                    namespace_plan.as_ref(),
                    &task_cancel,
                ) {
                    Ok(isolation) => Ok(json!({"isolation":isolation})),
                    Err(NamespaceError::PlanRequired) => Ok(json!({"planRequired":true})),
                    Err(error) => Err(error),
                }
            })
            .await
            .map_err(|_| invalid("namespace operation failed"))?
        }
        Request::Plan {
            project,
            directory,
            runtime,
        } => {
            crate::namespace_planner::prepare(
                &project,
                &directory,
                &runtime,
                lease.as_ref(),
                &cancel,
            )
            .await
        }
    };
    // A full stdout pipe can delay publication after the work has finished.
    // Keep signal handling alive until the final byte is accepted, and avoid
    // blocking the runtime that receives cancellation notifications.
    let publication = match result {
        Ok(value) => {
            let publishing = cancel.clone();
            tokio::task::spawn_blocking(move || -> Result<(), NamespaceError> {
                publishing.check()?;
                let response = crate::private_response::encode(value)?;
                crate::private_response::publish(&response, &publishing)?;
                Ok(())
            })
            .await
            .map_err(|_| invalid("namespace response publication failed"))
            .and_then(|result| result)
        }
        Err(error) => Err(error),
    };
    listener.abort();
    let _ = listener.await;
    publication
}

#[cfg(unix)]
pub(crate) fn execute() -> ExitCode {
    let result = (|| {
        for key in [
            crate::workspace_handoff::HANDOFF_ENV,
            crate::workspace_handoff::CAPS_ENV,
            crate::test_context::CONTEXT_ENV,
            crate::test_evidence::EVIDENCE_ENV,
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected private namespace capability"));
            }
        }
        let descriptor = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing private namespace request"))?;
        let request = decode(&crate::private_request::read(&descriptor)?)?;
        let lease = WorkspaceLease::from_env()?;
        let readiness = crate::readiness::Readiness::from_env()?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(process(request, lease, readiness))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            ExitCode::from(match error {
                NamespaceError::Cancelled(libc::SIGTERM) => 143,
                NamespaceError::Cancelled(_) => 130,
                _ => 125,
            })
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn private_requests_require_unique_known_fields_and_absolute_paths() {
        let isolate = json!({"operation":"isolate","project":"/subject","copy":"/copy", "framework":"/subject","alias":"failforge_subject","namespacePlan":null});
        let plan = json!({"operation":"plan","project":{"root":"/subject","name":"subject","flutter":false},"directory":"/report/plan","runtime":{"dartSdk":"/sdk","flutterSdk":null,"adapterRoot":"/adapter"}});
        for value in [isolate, plan] {
            assert!(decode(&serde_json::to_vec(&value).unwrap()).is_ok());
            let mut unknown = value.clone();
            unknown["extra"] = true.into();
            assert!(decode(&serde_json::to_vec(&unknown).unwrap()).is_err());
        }
        assert!(decode(br#"{"operation":"isolate","project":"/s","project":"/s","copy":"/c","framework":"/s","alias":"alias"}"#).is_err());
        assert!(decode(br#"{"operation":"isolate","project":"relative","copy":"/c","framework":"/s","alias":"alias"}"#).is_err());
        assert!(decode(br#"{"operation":"plan","project":{"root":"/s","name":"s","flutter":1},"directory":"/d","runtime":{"dartSdk":"/sdk","adapterRoot":"/adapter"}}"#).is_err());
    }
}

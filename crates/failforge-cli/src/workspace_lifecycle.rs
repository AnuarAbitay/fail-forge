//! One-shot lifecycle operations on inherited directory/lease capabilities.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use failforge_workspace::{Cancellation, InputError, lifecycle};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::signal::unix::{SignalKind, signal};

use crate::workspace_handoff::{self, Handoff};

#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    Start {
        root: PathBuf,
    },
    Allocate {
        root: PathBuf,
        campaign: String,
        kind: String,
    },
    Remove {
        root: PathBuf,
        campaign: String,
        name: String,
    },
    Close {
        root: PathBuf,
        campaign: String,
    },
}

enum Transport {
    Start(Handoff),
    Capabilities(lifecycle::LifecycleCaps),
}

fn invalid(message: &str) -> InputError {
    InputError::Invalid(message.into())
}

fn absolute(path: &Path) -> bool {
    path.is_absolute() && path.to_str().is_some_and(|text| !text.contains('\0'))
}

fn token_name(name: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|token| {
            token.len() == 32
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    })
}

fn decode(bytes: &[u8]) -> Result<Request, InputError> {
    let request: Request = serde_json::from_value(failforge_workspace::parse_json(bytes)?)
        .map_err(|_| invalid("invalid native workspace lifecycle request"))?;
    let (root, campaign) = match &request {
        Request::Start { root } => (root, None),
        Request::Allocate {
            root,
            campaign,
            kind,
        } => {
            if !matches!(kind.as_str(), "snapshot" | "attempt") {
                return Err(invalid("invalid_workspace_kind"));
            }
            (root, Some(campaign))
        }
        Request::Remove {
            root,
            campaign,
            name,
        } => {
            if !token_name(name, &["snapshot-", "attempt-"]) {
                return Err(invalid("invalid_workspace_name"));
            }
            (root, Some(campaign))
        }
        Request::Close { root, campaign } => (root, Some(campaign)),
    };
    if !absolute(root) || campaign.is_some_and(|name| !token_name(name, &["campaign-"])) {
        return Err(invalid("invalid native workspace lifecycle path"));
    }
    Ok(request)
}

fn transport(request: &Request) -> Result<Transport, InputError> {
    // close must not unknowingly keep a second inherited lease alive.
    if std::env::var_os(failforge_runner::WORKSPACE_LEASE_ENV).is_some() {
        return Err(invalid("unexpected inherited campaign lease"));
    }
    match request {
        Request::Start { .. } => Ok(Transport::Start(Handoff::from_env()?)),
        Request::Close { .. } => Ok(Transport::Capabilities(workspace_handoff::capabilities(3)?)),
        _ => Ok(Transport::Capabilities(workspace_handoff::capabilities(4)?)),
    }
}

fn publish(value: Value) -> Result<(), InputError> {
    // An allocation that already committed must publish its ownership receipt.
    // The controller retains its latched cancellation and then performs cleanup.
    let response = crate::private_response::encode(value)?;
    crate::private_response::publish(&response, &Cancellation::default())
}

fn perform(
    request: Request,
    transport: Transport,
    cancel: &Cancellation,
) -> Result<(), InputError> {
    match (request, transport) {
        (Request::Start { root }, Transport::Start(handoff)) => {
            let started = lifecycle::start(&root, cancel)?;
            let result = (|| {
                let response = crate::private_response::encode(json!({"root":started.root_path,
                    "campaign":started.root_path.join(&started.campaign_name),
                    "recovered":started.recovered, "skipped":started.skipped}))?;
                handoff.send(&started.caps)?;
                crate::private_response::publish(&response, &Cancellation::default())
            })();
            if result.is_err() {
                // If delivery failed, release our lease and try a normal reap.
                // A queued/received SCM_RIGHTS lease still prevents deletion.
                let mut caps = started.caps;
                drop(caps.lease.take());
                let _ = lifecycle::close(
                    &root,
                    &started.campaign_name,
                    caps,
                    &Cancellation::default(),
                );
            }
            result
        }
        (
            Request::Allocate {
                root,
                campaign,
                kind,
            },
            Transport::Capabilities(caps),
        ) => {
            let allocation = lifecycle::allocate(&root, &campaign, caps, &kind, cancel)?;
            publish(json!({"name":allocation.name,"path":allocation.path}))
        }
        (
            Request::Remove {
                root,
                campaign,
                name,
            },
            Transport::Capabilities(caps),
        ) => {
            lifecycle::remove(&root, &campaign, caps, &name, cancel)?;
            publish(json!({}))
        }
        (Request::Close { root, campaign }, Transport::Capabilities(caps)) => {
            let result = lifecycle::close(&root, &campaign, caps, cancel)?;
            publish(json!({"cleanupDeferred":result.cleanup_deferred,"skipped":result.skipped}))
        }
        _ => Err(invalid("workspace request capability mismatch")),
    }
}

async fn process(
    request: Request,
    transport: Transport,
    readiness: Option<crate::readiness::Readiness>,
) -> Result<(), InputError> {
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
    let result = tokio::task::spawn_blocking(move || perform(request, transport, &cancel))
        .await
        .map_err(|_| invalid("native workspace lifecycle operation failed"))?;
    listener.abort();
    let _ = listener.await;
    result
}

pub(crate) fn execute() -> ExitCode {
    let result = (|| {
        let request_fd = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing native workspace lifecycle request"))?;
        let request = decode(&crate::private_request::read(&request_fd)?)?;
        let transport = transport(&request)?;
        let readiness = crate::readiness::Readiness::from_env()?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(process(request, transport, readiness))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            ExitCode::from(match error {
                InputError::Cancelled(libc::SIGTERM) => 143,
                InputError::Cancelled(_) => 130,
                _ => 125,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_runner_use_the_same_inherited_lease_protocol() {
        assert_eq!(
            lifecycle::LEASE_MAGIC,
            failforge_runner::WORKSPACE_LEASE_MAGIC
        );
    }

    #[test]
    fn requests_reject_unknown_fields_aliases_and_path_traversal() {
        let campaign = format!("campaign-{}", "a".repeat(32));
        let name = format!("attempt-{}", "b".repeat(32));
        for value in [
            json!({"operation":"start","root":"/tmp/registry"}),
            json!({"operation":"allocate","root":"/tmp/registry","campaign":campaign,"kind":"snapshot"}),
            json!({"operation":"remove","root":"/tmp/registry","campaign":campaign,"name":name}),
            json!({"operation":"close","root":"/tmp/registry","campaign":campaign}),
        ] {
            assert!(decode(&serde_json::to_vec(&value).unwrap()).is_ok());
            let mut extra = value.clone();
            extra["unexpected"] = true.into();
            assert!(decode(&serde_json::to_vec(&extra).unwrap()).is_err());
            for path in ["relative", "/nul\0path"] {
                let mut invalid = value.clone();
                invalid["root"] = path.into();
                assert!(decode(&serde_json::to_vec(&invalid).unwrap()).is_err());
            }
        }
        for bad in [
            "../campaign",
            "pending-00000000000000000000000000000000",
            "campaign-abc",
            "/campaign-00000000000000000000000000000000",
            "campaign-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(
                decode(
                    &serde_json::to_vec(
                        &json!({"operation":"close","root":"/tmp/r","campaign":bad})
                    )
                    .unwrap()
                )
                .is_err()
            );
        }
        assert!(decode(br#"{"operation":"start","root":"/tmp/a","root":"/tmp/b"}"#).is_err());
        assert!(decode(&serde_json::to_vec(&json!({"operation":"allocate","root":"/tmp/r","campaign":campaign,"kind":"../attempt"})).unwrap()).is_err());
        assert!(decode(&serde_json::to_vec(&json!({"operation":"remove","root":"/tmp/r","campaign":campaign,"name":"../outside"})).unwrap()).is_err());
    }
}

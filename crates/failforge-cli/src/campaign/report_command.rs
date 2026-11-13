//! Pure report finalization after the lifecycle and all observers have drained.
use std::io::Write;
use std::process::ExitCode;

use failforge_dart::namespace::NamespaceError;
use failforge_workspace::Cancellation;
use serde_json::json;

use crate::namespace::invalid;

pub(crate) fn execute() -> ExitCode {
    let result = (|| -> Result<(), NamespaceError> {
        for key in [
            crate::workspace::handoff::HANDOFF_ENV,
            crate::workspace::handoff::CAPS_ENV,
            crate::evidence::context::CONTEXT_ENV,
            crate::evidence::EVIDENCE_ENV,
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected report capability"));
            }
        }
        let descriptor = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing final report request"))?;
        let raw = crate::transport::request::read(&descriptor)?;
        let ready = crate::transport::readiness::Readiness::from_env()?;
        if let Some(ready) = ready {
            ready.acknowledge()?;
        }
        // No project inspection, mutation, filesystem publication or callbacks
        // occur here. Cancellation of the campaign must still allow its report.
        let summary = crate::campaign::report::build(&raw)?;
        let terminal = crate::campaign::report_terminal::format_summary(&summary)?;
        let progress = crate::campaign::progress::from_summary(&summary)?;
        let response = crate::transport::response::encode(json!({
            "summary": summary, "terminal": terminal, "progress": progress
        }))?;
        crate::transport::response::publish(&response, &Cancellation::default())?;
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            ExitCode::from(125)
        }
    }
}

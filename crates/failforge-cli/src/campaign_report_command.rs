//! Pure report finalization after the lifecycle and all observers have drained.
use std::io::Write;
use std::process::ExitCode;

use failforge_dart::namespace::NamespaceError;
use failforge_workspace::Cancellation;
use serde_json::json;

use crate::dart_namespace::invalid;

pub(crate) fn execute() -> ExitCode {
    let result = (|| -> Result<(), NamespaceError> {
        for key in [
            crate::workspace_handoff::HANDOFF_ENV,
            crate::workspace_handoff::CAPS_ENV,
            crate::test_context::CONTEXT_ENV,
            crate::test_evidence::EVIDENCE_ENV,
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
        ] {
            if std::env::var_os(key).is_some() {
                return Err(invalid("unexpected report capability"));
            }
        }
        let descriptor = std::env::var_os("FAILFORGE_WORKSPACE_REQUEST_FD")
            .ok_or_else(|| invalid("missing final report request"))?;
        let raw = crate::private_request::read(&descriptor)?;
        let ready = crate::readiness::Readiness::from_env()?;
        if let Some(ready) = ready {
            ready.acknowledge()?;
        }
        // No project inspection, mutation, filesystem publication or callbacks
        // occur here. Cancellation of the campaign must still allow its report.
        let summary = crate::campaign_report::build(&raw)?;
        let terminal = crate::campaign_report_terminal::format_summary(&summary)?;
        let progress = crate::campaign_progress::from_summary(&summary)?;
        let response = crate::private_response::encode(json!({
            "summary": summary, "terminal": terminal, "progress": progress
        }))?;
        crate::private_response::publish(&response, &Cancellation::default())?;
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

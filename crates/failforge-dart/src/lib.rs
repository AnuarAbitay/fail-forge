//! The Dart adapter protocol, exact byte patches, and a bounded process client.
//!
//! This crate supplies integration primitives, not a mutation engine. Static
//! validation does not prove compilation or establish KILLED/SURVIVED outcomes.
//! Original project files are never written by the patch helpers.
//!
//! [`validate_test_configuration`] checks the actual working copy's prepared
//! package roots and subject alias before launching source or kernel test code.
//! Callers reject configuration failures before collecting test evidence.
//!
//! [`classify_dart_result`] validates complete Dart/Flutter reporter captures
//! against a typed native process result. Its primary verdict still requires
//! campaign baseline, author-test selection and input-integrity checks before
//! it can establish a mutation outcome. [`classify_dart_capture`] supports
//! offline inspection of captures with an existing textual Runner receipt.
//! [`analyze_catalogue`] records author identities and structural evidence while
//! the working copy exists. [`apply_selection`] compares this snapshot against
//! the baseline, preserving an incomplete catalogue unless an independent
//! ordinary failure supplies a source-bound witness.
//! [`initialization_handoff`] derives eligible loader signatures and enforces
//! ordered original/mutant/original controls through a private continuation.
//! Its confirmation remains subject to the controller's source-integrity and
//! cancellation checks before publication.

pub mod attempt_workspace;
mod client;
pub mod discovery;
mod evidence;
mod initialization;
pub mod namespace;
mod patch;
mod protocol;
mod test_catalog;
mod test_configuration;
mod test_outcome;
mod test_reporter;

pub use client::{AdapterClient, ClientClose, ClientError, ClientOptions};
pub use evidence::{TestEvidence, TestRunClassification, classify_test_run};
pub use initialization::{
    ConfirmationSignature, InitializationAttempt, InitializationContext, InitializationDecision,
    InitializationError, InitializationEvidence, InitializationHandoff, InitializationObservation,
    InitializationRequest, InitializationRole, InitializationState, NormalizedDiagnostic,
    NormalizedLoadFailure, SignatureKind, analyze_initialization_evidence, evaluate_initialization,
    initialization_handoff,
};
pub use patch::{PatchError, apply_mutant, read_and_apply_mutant, validate_relative_target};
pub use protocol::{
    AdapterCommand, AdapterFailure, ByteEdit, MAX_LINE_BYTES, Mutant, MutationFamily,
    PROTOCOL_VERSION, ProtocolError, RequestEnvelope, ResponseEnvelope, ResponseKind, Symbol,
    ValidationState, sha256_hex,
};
pub use test_catalog::{
    CatalogError, CatalogSnapshot, SelectionContext, analyze_catalogue, apply_selection,
    parse_selection_context, validate_selection_context,
};
pub use test_configuration::validate_test_configuration;
pub use test_outcome::{
    ReporterMode, RunnerSummaryError, classify_dart_capture, classify_dart_result,
    parse_cli_summary,
};

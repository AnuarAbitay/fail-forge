//! The Dart adapter protocol, exact byte patches, and a bounded process client.
//!
//! This crate supplies integration primitives, not a mutation engine. Static
//! validation does not prove compilation or establish KILLED/SURVIVED outcomes.
//! Original project files are never written by the patch helpers.
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

mod client;
mod evidence;
mod patch;
mod protocol;
mod test_catalog;
mod test_outcome;
mod test_reporter;

pub use client::{AdapterClient, ClientError, ClientOptions};
pub use evidence::{TestEvidence, TestRunClassification, classify_test_run};
pub use patch::{PatchError, apply_mutant, read_and_apply_mutant, validate_relative_target};
pub use protocol::{
    AdapterCommand, AdapterFailure, ByteEdit, MAX_LINE_BYTES, Mutant, MutationFamily,
    PROTOCOL_VERSION, ProtocolError, RequestEnvelope, ResponseEnvelope, ResponseKind, Symbol,
    ValidationState, sha256_hex,
};
pub use test_catalog::{
    CatalogError, CatalogSnapshot, SelectionContext, analyze_catalogue, apply_selection,
    parse_selection_context,
};
pub use test_outcome::{ReporterMode, classify_dart_capture, classify_dart_result};

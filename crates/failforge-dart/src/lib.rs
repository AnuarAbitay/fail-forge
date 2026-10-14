//! The Dart adapter protocol, exact byte patches, and a bounded process client.
//!
//! This crate supplies integration primitives, not a mutation engine. Static
//! validation does not prove compilation or establish KILLED/SURVIVED outcomes.
//! Original project files are never written by the patch helpers.

mod client;
mod evidence;
mod patch;
mod protocol;

pub use client::{AdapterClient, ClientError, ClientOptions};
pub use evidence::{TestEvidence, TestRunClassification, classify_test_run};
pub use patch::{PatchError, apply_mutant, read_and_apply_mutant, validate_relative_target};
pub use protocol::{
    AdapterCommand, AdapterFailure, ByteEdit, MAX_LINE_BYTES, Mutant, MutationFamily,
    PROTOCOL_VERSION, ProtocolError, RequestEnvelope, ResponseEnvelope, ResponseKind, Symbol,
    ValidationState, sha256_hex,
};

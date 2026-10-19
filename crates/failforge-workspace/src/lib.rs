//! Shared prepared-project inspection, immutable byte bindings, and workspaces.

mod cancellation;
mod copy;
mod inputs;

pub use cancellation::Cancellation;
pub use copy::{copy_campaign_snapshot, copy_project};
pub use inputs::{
    BoundAlias, BoundTree, Hashes, PreparedProject, bound_tree, copy_snapshot, dependency_roots,
    hash_file, inspect_project, metadata_json, parse_json, read_regular, read_regular_prefix,
    source_hashes, verify_source,
};

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("cannot read or copy workspace inputs")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("workspace operation cancelled")]
    Cancelled(i32),
}

pub(crate) fn failure(message: impl Into<String>) -> InputError {
    InputError::Invalid(message.into())
}

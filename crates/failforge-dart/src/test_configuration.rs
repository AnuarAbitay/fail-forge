//! Validate prepared package bindings before either source or kernel execution.
use std::path::Path;

use failforge_workspace::{InputError, dependency_roots};

/// Check the supported local package configuration and its subject binding.
///
/// Use the configuration in the actual working copy, after namespace isolation.
/// The subject alias must resolve to that copy, while all package roots must
/// remain distinct. This check does not depend on the test runner's entry point
/// or on diagnostic text emitted by author code.
///
/// The caller must retain its workspace ownership and verify input integrity
/// after execution; this preflight is not protection against concurrent edits.
pub fn validate_test_configuration(
    working_copy: &Path,
    subject_alias: &str,
) -> Result<(), InputError> {
    let root = working_copy.canonicalize()?;
    let packages = dependency_roots(&root.join(".dart_tool/package_config.json"))?;
    if subject_alias.is_empty() || packages.get(subject_alias) != Some(&root) {
        return Err(InputError::Invalid(
            "test package configuration does not identify the working copy".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "test_configuration_tests.rs"]
mod tests;

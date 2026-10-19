//! File-scoped static applicability through the Rust adapter cleanup owner.
//! No author tests, package downloads, or mutant execution happen here.
use failforge_workspace as inputs;
pub use failforge_workspace::Cancellation;
mod session;
mod workspaces;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use failforge_config::{Context, from_profile_mapping};
use serde_json::json;

use crate::{ProfileDraft, ProfileError};
use inputs::{BoundTree, Hashes, PreparedProject};

pub const MAX_DISCOVERY_CANDIDATES: usize = 10_000;

#[derive(Clone, Debug, Default)]
pub struct FamilyApplicability {
    pub family: String,
    pub active_operators: Vec<String>,
    pub discovered_candidates: usize,
    pub statically_valid_candidates: usize,
    pub valid_counts: Vec<(String, usize)>,
}

#[derive(Debug)]
pub struct ApplicabilityResult {
    pub families: Vec<FamilyApplicability>,
    original: Option<(PathBuf, Hashes)>,
    binding: Option<InputBinding>,
}

impl ApplicabilityResult {
    /// Recheck all original, prepared dependency, SDK, adapter and native tool
    /// inputs immediately before publishing the generated profile.
    pub fn verify_unchanged(&self) -> Result<(), ProfileError> {
        self.verify_unchanged_with_cancellation(&Cancellation::default())
    }

    pub fn verify_unchanged_with_cancellation(
        &self,
        cancel: &Cancellation,
    ) -> Result<(), ProfileError> {
        if let Some((root, hashes)) = &self.original {
            inputs::verify_source(root, hashes, cancel)?;
        }
        if let Some(binding) = &self.binding {
            binding.verify(cancel)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(families: Vec<FamilyApplicability>) -> Self {
        Self {
            families,
            original: None,
            binding: None,
        }
    }
}

fn failure(message: impl Into<String>) -> ProfileError {
    ProfileError::Analysis(message.into())
}

#[derive(Debug)]
struct Runtime {
    helper: PathBuf,
    dart_sdk: PathBuf,
    flutter_sdk: Option<PathBuf>,
    adapter: PathBuf,
    installation: PathBuf,
}

impl Runtime {
    fn environment(&self) -> BTreeMap<OsString, OsString> {
        let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
        for key in [
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
            "FAILFORGE_RUNNER_READY_FD",
            "FAILFORGE_TEST_EVIDENCE_FD",
            "FAILFORGE_TEST_CONTEXT_FD",
            "FAILFORGE_WORKSPACE_REQUEST_FD",
        ] {
            environment.remove(std::ffi::OsStr::new(key));
        }
        for key in [
            "CI",
            "FLUTTER_SUPPRESS_ANALYTICS",
            "DART_SUPPRESS_ANALYTICS",
        ] {
            environment.insert(key.into(), "true".into());
        }
        let cache = self.installation.join(".toolchain/pub-cache");
        if cache.is_dir() {
            environment
                .entry("PUB_CACHE".into())
                .or_insert(cache.into_os_string());
        }
        environment
    }

    fn validate(&self, flutter: bool) -> Result<(), ProfileError> {
        use std::os::unix::fs::PermissionsExt;
        let executable = |path: &Path| {
            path.metadata()
                .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
        };
        if !executable(&self.helper)
            || !executable(&self.dart_sdk.join("bin/dart"))
            || !self
                .adapter
                .join("bin/failforge_dart_adapter.dart")
                .is_file()
            || !self
                .adapter
                .join(".dart_tool/package_config.json")
                .is_file()
        {
            return Err(ProfileError::Config("applicability analysis requires a prepared SDK, native adapter helper and adapter; use --template for a catalogue without analysis".into()));
        }
        if flutter
            && !self
                .flutter_sdk
                .as_ref()
                .is_some_and(|sdk| executable(&sdk.join("bin/flutter")))
        {
            return Err(ProfileError::Config(
                "Flutter project requires a prepared --flutter-sdk".into(),
            ));
        }
        Ok(())
    }
}

struct InputBinding {
    trees: BTreeMap<PathBuf, BoundTree>,
    files: BTreeMap<PathBuf, String>,
    dependencies: BTreeMap<PathBuf, BTreeMap<String, PathBuf>>,
    environment: BTreeMap<OsString, OsString>,
    installation: PathBuf,
}

impl std::fmt::Debug for InputBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputBinding")
            .field("trees", &self.trees.len())
            .field("files", &self.files.len())
            .field("dependency_configurations", &self.dependencies.len())
            .finish_non_exhaustive()
    }
}

impl InputBinding {
    fn capture(
        runtime: &Runtime,
        original: &PreparedProject,
        cancel: &Cancellation,
    ) -> Result<Self, ProfileError> {
        let adapter_config = runtime.adapter.join(".dart_tool/package_config.json");
        let adapter_dependencies = inputs::dependency_roots(&adapter_config)?;
        let mut roots: BTreeSet<_> = original
            .packages
            .values()
            .filter(|path| **path != original.root)
            .cloned()
            .collect();
        roots.extend(
            adapter_dependencies
                .values()
                .filter(|path| **path != runtime.adapter)
                .cloned(),
        );
        roots.extend([
            runtime.adapter.clone(),
            runtime.dart_sdk.join("bin"),
            runtime.dart_sdk.join("lib"),
        ]);
        let mut files = BTreeSet::from([
            runtime.helper.clone(),
            adapter_config.clone(),
            std::env::current_exe()?.canonicalize()?,
        ]);
        let graph = runtime.adapter.join(".dart_tool/package_graph.json");
        if graph.is_file() {
            files.insert(graph);
        }
        if let Some(flutter) = &runtime.flutter_sdk {
            for relative in ["bin/flutter", "bin/cache/flutter_tools.snapshot"] {
                let path = flutter.join(relative);
                if path.is_file() {
                    files.insert(path.canonicalize()?);
                }
            }
            let engine = flutter.join("bin/cache/artifacts/engine");
            if engine.is_dir() {
                roots.insert(engine);
            }
        }
        let trees = roots
            .into_iter()
            .map(|root| inputs::bound_tree(&root, cancel).map(|hashes| (root, hashes)))
            .collect::<Result<_, _>>()?;
        let files = files
            .into_iter()
            .map(|path| inputs::hash_file(&path, cancel).map(|hash| (path, hash)))
            .collect::<Result<_, _>>()?;
        let dependencies = BTreeMap::from([
            (
                original.root.join(".dart_tool/package_config.json"),
                original.packages.clone(),
            ),
            (adapter_config, adapter_dependencies),
        ]);
        Ok(Self {
            trees,
            files,
            dependencies,
            environment: runtime.environment(),
            installation: runtime.installation.clone(),
        })
    }

    fn verify(&self, cancel: &Cancellation) -> Result<(), ProfileError> {
        for (root, expected) in &self.trees {
            if inputs::bound_tree(root, cancel)? != *expected {
                return Err(failure(
                    "SDK, dependency or adapter inputs changed during applicability analysis",
                ));
            }
        }
        for (path, expected) in &self.files {
            if inputs::hash_file(path, cancel)? != *expected {
                return Err(failure(
                    "native tool or adapter metadata changed during applicability analysis",
                ));
            }
        }
        for (path, expected) in &self.dependencies {
            if inputs::dependency_roots(path)? != *expected {
                return Err(failure(
                    "prepared dependency roots changed during applicability analysis",
                ));
            }
        }
        let runtime = Runtime {
            helper: PathBuf::new(),
            dart_sdk: PathBuf::new(),
            flutter_sdk: None,
            adapter: PathBuf::new(),
            installation: self.installation.clone(),
        };
        if runtime.environment() != self.environment {
            return Err(failure("analysis environment changed before publication"));
        }
        Ok(())
    }
}

pub async fn analyze_profile(
    draft: &ProfileDraft,
    context: &Context,
    helper: &Path,
    cancel: &Cancellation,
) -> Result<ApplicabilityResult, ProfileError> {
    cancel.check()?;
    let profile = from_profile_mapping(
        &draft.values,
        &draft.project,
        context,
        draft.output.parent(),
    )?;
    let source = draft
        .source
        .strip_prefix(&draft.project)
        .map_err(|_| {
            ProfileError::Config("applicability source must remain inside selected package".into())
        })?
        .to_str()
        .ok_or_else(|| failure("source path must be Unicode"))?;
    if profile.campaigns.iter().any(|config| {
        config.rules.include.as_deref() != Some(&[source.to_owned()])
            || !config.rules.exclude.is_empty()
            || !config.rules.components.is_empty()
            || config.rules.classes.is_some()
            || config.rules.methods.is_some()
    }) {
        return Err(ProfileError::Config(
            "applicability analysis requires exactly the selected source file".into(),
        ));
    }
    let original = inputs::inspect_project(profile.campaigns.first().map(|config| &config.project)
        .unwrap_or(&draft.project), cancel).map_err(|error| match error {
            inputs::InputError::Cancelled(signal) => ProfileError::Cancelled(signal),
            _ => ProfileError::Config(format!("applicability analysis requires prepared project dependencies; prepare the package first or use --template: {error}")),
        })?;
    if draft
        .framework_flutter
        .is_some_and(|flutter| original.flutter != flutter)
    {
        return Err(ProfileError::Config(
            "prepared test framework changed during profile generation; retry".into(),
        ));
    }
    // The generated profile always includes a selected family. Fully disabled
    // operators still retain runtime defaults in the native configuration.
    let runtime_config = if let Some(config) = profile.campaigns.first() {
        config.clone()
    } else {
        let mut values = draft.values.clone();
        values["mutations"] =
            json!({"classic":{"enabled":true,"operators":{"RELATIONAL_BOUNDARY":true}}});
        from_profile_mapping(&values, &draft.project, context, draft.output.parent())?
            .campaigns
            .remove(0)
    };
    let mut runtime = Runtime {
        helper: helper.canonicalize().map_err(|_| ProfileError::Config(
            "applicability analysis requires the prepared native adapter helper; use --template for a catalogue without analysis".into()))?,
        dart_sdk: runtime_config.dart_sdk,
        flutter_sdk: runtime_config.flutter_sdk,
        adapter: runtime_config.adapter_root,
        installation: context.installation.clone(),
    };
    if original.flutter && runtime.flutter_sdk.is_none() {
        if let Some(candidate) = runtime.dart_sdk.ancestors().nth(3) {
            if candidate.join("bin/flutter").is_file() {
                runtime.flutter_sdk = Some(candidate.to_path_buf());
            }
        }
    }
    runtime.validate(original.flutter)?;
    let original_hashes = inputs::source_hashes(&original.root, cancel)?;
    let binding = InputBinding::capture(&runtime, &original, cancel)?;
    let mut protected: Vec<_> = binding.trees.keys().cloned().collect();
    protected.extend([
        original.root.clone(),
        runtime.dart_sdk.clone(),
        runtime.adapter.clone(),
    ]);
    protected.extend(runtime.flutter_sdk.iter().cloned());
    let mut workspace = workspaces::Workspace::new(&protected)?;
    let outcome = async {
        let snapshot = inputs::copy_snapshot(
            &original,
            &workspace.path.join("package"),
            &original_hashes,
            cancel,
        )?;
        let snapshot_hashes = inputs::source_hashes(&snapshot.root, cancel)?;
        let mut families = vec![];
        for selection in profile
            .mutations
            .iter()
            .filter(|selection| selection.present)
        {
            cancel.check()?;
            let active = selection.active_operators();
            let Some(config) = profile
                .campaigns
                .iter()
                .find(|config| config.family == selection.family)
            else {
                families.push(FamilyApplicability {
                    family: selection.family.clone(),
                    ..Default::default()
                });
                continue;
            };
            if !original.flutter
                && active
                    .iter()
                    .any(|operator| operator == "METHOD_CHANNEL_ERROR")
            {
                return Err(ProfileError::Config(
                    "METHOD_CHANNEL_ERROR requires a prepared Flutter project".into(),
                ));
            }
            let mut payload = json!({"workspaceRoot":snapshot.root,"packageRoot":".",
                "dartSdkPath":runtime.dart_sdk,"rules":config.rules.to_adapter_rules(),
                "limits":{"maxMutants":MAX_DISCOVERY_CANDIDATES}});
            if original.flutter {
                payload["flutterSdkPath"] = json!(runtime.flutter_sdk);
            }
            let result = session::discover(
                &runtime,
                &snapshot.root,
                &workspace,
                &payload,
                &selection.family,
                &active,
                cancel,
            )
            .await?;
            families.push(result);
            inputs::verify_source(&snapshot.root, &snapshot_hashes, cancel)?;
            inputs::verify_source(&original.root, &original_hashes, cancel)?;
            binding.verify(cancel)?;
        }
        cancel.check()?;
        Ok(families)
    }
    .await;
    // Close after the owner process drains on every return path. A surviving
    // cleanup owner retains its flock, so failed cleanup cannot publish output.
    let cleanup = workspace.close();
    cleanup?;
    let families = outcome?;
    cancel.check()?;
    binding.verify(cancel)?;
    inputs::verify_source(&original.root, &original_hashes, cancel)?;
    Ok(ApplicabilityResult {
        families,
        original: Some((original.root, original_hashes)),
        binding: Some(binding),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_latches_before_and_after_subscription() {
        let cancel = Cancellation::default();
        cancel.request(libc::SIGTERM);
        cancel.request(libc::SIGINT);
        assert_eq!(cancel.cancelled().await, libc::SIGTERM);
        assert!(matches!(
            cancel.check().map_err(ProfileError::from),
            Err(ProfileError::Cancelled(libc::SIGTERM))
        ));
        let cancel = Cancellation::default();
        let other = cancel.clone();
        tokio::spawn(async move {
            other.request(libc::SIGINT);
        });
        assert_eq!(cancel.cancelled().await, libc::SIGINT);
    }
}

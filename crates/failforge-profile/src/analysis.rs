//! File-scoped static applicability through the Rust adapter cleanup owner.
//! No author tests, package downloads, or mutant execution happen here.
use failforge_workspace as inputs;
pub use failforge_workspace::Cancellation;
#[cfg(unix)]
mod session;
#[cfg(windows)]
#[path = "analysis/session_windows.rs"]
mod session;
mod transcript;
#[cfg(unix)]
mod workspaces;
#[cfg(windows)]
#[path = "analysis/workspaces_windows.rs"]
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

fn environment_key_is(key: &std::ffi::OsStr, expected: &str) -> bool {
    #[cfg(unix)]
    {
        key == expected
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
        let key: Vec<_> = key.encode_wide().collect();
        let expected: Vec<_> = expected.encode_utf16().collect();
        // SAFETY: live UTF-16 buffers and read-only ordinal comparison. Native
        // environment names have lengths representable by the API's i32 count.
        unsafe {
            CompareStringOrdinal(
                key.as_ptr(),
                key.len() as i32,
                expected.as_ptr(),
                expected.len() as i32,
                1,
            ) == CSTR_EQUAL
        }
    }
}

impl Runtime {
    fn environment(&self) -> BTreeMap<OsString, OsString> {
        self.environment_from(std::env::vars_os().collect())
    }

    fn environment_from(
        &self,
        mut environment: BTreeMap<OsString, OsString>,
    ) -> BTreeMap<OsString, OsString> {
        #[cfg(windows)]
        // Windows carries hidden per-drive state under keys such as =C:.
        // Every managed program/cwd here is absolute, and explicit EnvSpec keys
        // must be ordinary application names rather than these OS internals.
        environment.retain(|key, _| !key.as_encoded_bytes().contains(&b'='));
        for key in [
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_ADAPTER_CANCEL_FD",
            "FAILFORGE_ADAPTER_CANCEL_PIPE",
            "FAILFORGE_CLI_CANCEL_PIPE",
            "FAILFORGE_RUNNER_READY_FD",
            "FAILFORGE_TEST_EVIDENCE_FD",
            "FAILFORGE_TEST_CONTEXT_FD",
            "FAILFORGE_WORKSPACE_REQUEST_FD",
            "FAILFORGE_WORKSPACE_HANDOFF_FD",
            "FAILFORGE_WORKSPACE_CAPS_FDS",
            "FAILFORGE_REPORT_FDS",
        ] {
            environment.retain(|name, _| !environment_key_is(name, key));
        }
        for key in [
            "CI",
            "FLUTTER_SUPPRESS_ANALYTICS",
            "DART_SUPPRESS_ANALYTICS",
        ] {
            environment.retain(|name, _| !environment_key_is(name, key));
            environment.insert(key.into(), "true".into());
        }
        let cache = self.installation.join(".toolchain/pub-cache");
        if cache.is_dir()
            && !environment
                .keys()
                .any(|key| environment_key_is(key, "PUB_CACHE"))
        {
            environment.insert("PUB_CACHE".into(), cache.into_os_string());
        }
        environment
    }

    fn validate(&self, flutter: bool) -> Result<(), ProfileError> {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let executable = |path: &Path| {
            path.metadata().is_ok_and(|info| {
                info.is_file() && {
                    #[cfg(unix)]
                    {
                        info.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(windows)]
                    {
                        true
                    }
                }
            })
        };
        if !executable(&self.helper)
            || !executable(&self.dart_sdk.join(failforge_config::platform::DART_BIN))
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
                .is_some_and(|sdk| executable(&sdk.join(failforge_config::platform::FLUTTER_BIN)))
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
        if let Some(flutter) = runtime.flutter_sdk.as_ref().filter(|_| original.flutter) {
            for relative in [
                failforge_config::platform::FLUTTER_BIN,
                "bin/cache/flutter_tools.snapshot",
            ] {
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
    #[cfg(windows)]
    let source = source.replace('\\', "/");
    #[cfg(windows)]
    let source = source.as_str();
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
            if candidate
                .join(failforge_config::platform::FLUTTER_BIN)
                .is_file()
            {
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
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct BindingFixture {
        root: PathBuf,
        original: PreparedProject,
        runtime: Runtime,
    }

    impl BindingFixture {
        fn new(flutter: bool) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "failforge-profile-input-binding-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            for relative in [
                "package/.dart_tool",
                "framework/lib",
                "adapter/.dart_tool",
                "flutter/bin/cache/dart-sdk/bin",
                "flutter/bin/cache/dart-sdk/lib",
                "flutter/bin/cache/artifacts/engine",
            ] {
                fs::create_dir_all(root.join(relative)).unwrap();
            }
            let root = root.canonicalize().unwrap();
            let framework = if flutter { "flutter_test" } else { "test" };
            fs::write(root.join("package/pubspec.yaml"), "name: subject\n").unwrap();
            fs::write(
                root.join("package/.dart_tool/package_config.json"),
                json!({"configVersion": 2, "packages": [
                    {"name": "subject", "rootUri": "../"},
                    {"name": framework, "rootUri": "../../framework/"}
                ]})
                .to_string(),
            )
            .unwrap();
            fs::write(
                root.join("package/.dart_tool/package_graph.json"),
                json!({"roots": ["subject"], "packages": [
                    {"name": "subject", "dependencies": [framework]},
                    {"name": framework, "dependencies": []}
                ]})
                .to_string(),
            )
            .unwrap();
            fs::write(
                root.join("adapter/.dart_tool/package_config.json"),
                json!({"configVersion": 2, "packages": [
                    {"name": "adapter", "rootUri": "../"}
                ]})
                .to_string(),
            )
            .unwrap();
            for relative in [
                "helper",
                "flutter/bin/flutter",
                "flutter/bin/cache/flutter_tools.snapshot",
                "flutter/bin/cache/artifacts/engine/runtime",
                "flutter/bin/cache/dart-sdk/bin/dart",
                "flutter/bin/cache/dart-sdk/lib/core.dart",
            ] {
                fs::write(root.join(relative), "original bytes").unwrap();
            }
            let original =
                inputs::inspect_project(&root.join("package"), &Cancellation::default()).unwrap();
            let runtime = Runtime {
                helper: root.join("helper"),
                dart_sdk: root.join("flutter/bin/cache/dart-sdk"),
                flutter_sdk: Some(root.join("flutter")),
                adapter: root.join("adapter"),
                installation: root.clone(),
            };
            Self {
                root,
                original,
                runtime,
            }
        }

        fn binding(&self) -> InputBinding {
            InputBinding::capture(&self.runtime, &self.original, &Cancellation::default()).unwrap()
        }
    }

    impl Drop for BindingFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn dart_project_binds_dart_inputs_without_unused_flutter_tools_or_engine() {
        let fixture = BindingFixture::new(false);
        assert!(!fixture.original.flutter);
        assert!(fixture.runtime.flutter_sdk.is_some());
        let binding = fixture.binding();
        let flutter = fixture.runtime.flutter_sdk.as_ref().unwrap();
        assert!(
            !binding
                .trees
                .contains_key(&flutter.join("bin/cache/artifacts/engine"))
        );
        for relative in [
            failforge_config::platform::FLUTTER_BIN,
            "bin/cache/flutter_tools.snapshot",
        ] {
            assert!(!binding.files.contains_key(&flutter.join(relative)));
        }
        for relative in [
            "bin/flutter",
            "bin/cache/flutter_tools.snapshot",
            "bin/cache/artifacts/engine/runtime",
        ] {
            fs::write(flutter.join(relative), "unused Flutter input changed").unwrap();
        }
        binding.verify(&Cancellation::default()).unwrap();

        fs::write(
            fixture.runtime.dart_sdk.join("lib/core.dart"),
            "Dart input changed",
        )
        .unwrap();
        assert!(binding.verify(&Cancellation::default()).is_err());
    }

    #[test]
    fn flutter_project_binds_tools_and_engine_and_detects_each_changed_input() {
        let fixture = BindingFixture::new(true);
        assert!(fixture.original.flutter);
        let binding = fixture.binding();
        let flutter = fixture.runtime.flutter_sdk.as_ref().unwrap();
        assert!(
            binding
                .trees
                .contains_key(&flutter.join("bin/cache/artifacts/engine"))
        );
        for relative in [
            failforge_config::platform::FLUTTER_BIN,
            "bin/cache/flutter_tools.snapshot",
        ] {
            assert!(binding.files.contains_key(&flutter.join(relative)));
        }
        binding.verify(&Cancellation::default()).unwrap();
        for relative in [
            "bin/flutter",
            "bin/cache/flutter_tools.snapshot",
            "bin/cache/artifacts/engine/runtime",
        ] {
            let path = flutter.join(relative);
            fs::write(&path, "Flutter input changed").unwrap();
            assert!(
                binding.verify(&Cancellation::default()).is_err(),
                "{relative}"
            );
            fs::write(path, "original bytes").unwrap();
            binding.verify(&Cancellation::default()).unwrap();
        }
    }

    #[test]
    fn analysis_environment_drops_private_workspace_transports() {
        let runtime = Runtime {
            helper: "/unused/helper".into(),
            dart_sdk: "/unused/dart".into(),
            flutter_sdk: None,
            adapter: "/unused/adapter".into(),
            installation: "/unused/installation".into(),
        };
        let private = [
            "FAILFORGE_WORKSPACE_LEASE_FD",
            "FAILFORGE_WORKSPACE_REQUEST_FD",
            "FAILFORGE_WORKSPACE_HANDOFF_FD",
            "FAILFORGE_WORKSPACE_CAPS_FDS",
            "FAILFORGE_REPORT_FDS",
        ];
        let mut input = BTreeMap::from([("PATH".into(), "/tools/bin".into())]);
        for key in private {
            input.insert(key.into(), "7".into());
        }
        let environment = runtime.environment_from(input);
        for key in private {
            assert!(!environment.contains_key(std::ffi::OsStr::new(key)));
        }
        assert_eq!(
            environment.get(std::ffi::OsStr::new("PATH")),
            Some(&OsString::from("/tools/bin"))
        );
        assert_eq!(
            environment.get(std::ffi::OsStr::new("CI")),
            Some(&OsString::from("true"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn explicit_environment_overrides_fold_windows_keys_and_omit_hidden_drive_state() {
        let fixture = BindingFixture::new(false);
        let captured = BTreeMap::from([
            ("ci".into(), "false".into()),
            ("flutter_suppress_analytics".into(), "false".into()),
            ("PuB_cAcHe".into(), "existing cache".into()),
            ("=C:".into(), "C:\\previous".into()),
            (
                "failforge_adapter_cancel_pipe".into(),
                "private control".into(),
            ),
        ]);
        let env = fixture.runtime.environment_from(captured);
        assert_eq!(
            env.get(std::ffi::OsStr::new("CI")),
            Some(&OsString::from("true"))
        );
        assert!(!env.contains_key(std::ffi::OsStr::new("ci")));
        assert_eq!(
            env.get(std::ffi::OsStr::new("PuB_cAcHe")),
            Some(&OsString::from("existing cache"))
        );
        assert!(!env.contains_key(std::ffi::OsStr::new("=C:")));
        assert!(!env.contains_key(std::ffi::OsStr::new("failforge_adapter_cancel_pipe")));
    }

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

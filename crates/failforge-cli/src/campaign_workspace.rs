//! Native campaign preflight, fresh snapshot ownership and explicit cleanup.
#[cfg(windows)]
use failforge_workspace::windows_fs as win;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use failforge_config::catalogue::{FAMILIES, default_operators};
use failforge_dart::namespace::NamespaceError;
use failforge_workspace::lifecycle::{self, BoundAllocation, LifecycleCaps, Skip};
use failforge_workspace::{
    Cancellation, Hashes, PreparedProject, copy_campaign_snapshot, inspect_project, parse_json,
    source_hashes, verify_source,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::attempt::Config;
use crate::dart_namespace::{absolute, invalid};
use crate::family_execution::DirectoryGuard;
use crate::retained_replay::{LoadedReplay, ReplayPaths};
use crate::workspace_inputs::RuntimeInputs;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    Direct,
    Profile,
}

impl Mode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Profile => "profile",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Family {
    pub(crate) family: String,
    pub(crate) rules: Value,
    pub(crate) workers: usize,
    pub(crate) config: Config,
}

impl Family {
    pub(crate) fn to_value(&self) -> Value {
        json!({"family":self.family,"rules":self.rules,"workers":self.workers,
            "config":{"commandTimeout":self.config.command_timeout,
                "testTimeout":self.config.test_timeout}})
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub(crate) operation: String,
    pub(crate) mode: Mode,
    pub(crate) run_id: String,
    pub(crate) project: PathBuf,
    pub(crate) output: Option<PathBuf>,
    pub(crate) registry: Option<PathBuf>,
    pub(crate) runtime: RuntimeInputs,
    pub(crate) environment: BTreeMap<String, String>,
    pub(crate) max_mutants: usize,
    pub(crate) families: Vec<Family>,
    #[serde(default)]
    pub(crate) replay: Option<ReplayPaths>,
}

#[cfg(unix)]
fn default_registry() -> Result<PathBuf, NamespaceError> {
    // SAFETY: geteuid has no pointer arguments or side effects.
    Ok(
        std::env::temp_dir().join(format!("failforge-workspaces-{}", unsafe {
            libc::geteuid()
        })),
    )
}

#[cfg(windows)]
fn default_registry() -> Result<PathBuf, NamespaceError> {
    Ok(std::env::temp_dir().join(format!("failforge-workspaces-{}", win::current_sid()?)))
}

pub(crate) fn decode(raw: &[u8]) -> Result<Request, NamespaceError> {
    let value = parse_json(raw)?;
    for pointer in ["", "/runtime", "/environment"] {
        if !value.pointer(pointer).is_some_and(Value::is_object) {
            return Err(invalid("campaign bootstrap requires named objects"));
        }
    }
    if value.get("output").is_none() || value.get("registry").is_none() {
        return Err(invalid("campaign output and registry fields are required"));
    }
    if value
        .get("replay")
        .is_some_and(|replay| !replay.is_null() && !replay.is_object())
    {
        return Err(invalid("retained replay paths must be a named object"));
    }
    for family in value["families"]
        .as_array()
        .ok_or_else(|| invalid("campaign families must be an array"))?
    {
        if !family.is_object() || !family["config"].is_object() || !family["rules"].is_object() {
            return Err(invalid("campaign families require named objects"));
        }
    }
    let mut request: Request = serde_json::from_value(value)
        .map_err(|_| invalid("invalid native campaign bootstrap request"))?;
    validate_request(&request)?;
    request
        .families
        .sort_by_key(|family| FAMILIES.iter().position(|name| *name == family.family));
    Ok(request)
}

fn validate_request(request: &Request) -> Result<(), NamespaceError> {
    if request.operation != "campaign-run"
        || request.run_id.len() != 32
        || !request
            .run_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid("invalid campaign operation or run identity"));
    }
    absolute(&request.project)?;
    if let Some(output) = &request.output {
        absolute(output)?;
    }
    if let Some(registry) = &request.registry {
        absolute(registry)?;
    }
    if let Some(replay) = &request.replay {
        if request.mode != Mode::Direct
            || request.families.len() != 1
            || request.families[0].family != "classic"
        {
            return Err(invalid(
                "retained replay requires one direct classic family",
            ));
        }
        for path in [&replay.transcript, &replay.baseline, &replay.framework] {
            absolute(path)?;
        }
    }
    // Reuse strict family, rule, timeout, worker and environment validation.
    // The real project is inspected only in preflight, without filesystem writes.
    let project = PreparedProject {
        root: request.project.clone(),
        name: "subject".into(),
        packages: BTreeMap::new(),
        graph: json!({}),
        flutter: false,
        framework_collision: false,
    };
    crate::campaign_execution::decode_value(json!({"operation":"campaign",
        "mode":request.mode.as_str(),"project":project,
        "original":{"root":request.project,"hashes":{}},
        "output":request.output.clone().unwrap_or_else(||request.project.join(".failforge/runs").join(&request.run_id)),
        "runtime":runtime_value(&request.runtime),
        "workspace":{"root":match &request.registry { Some(path) => path.clone(), None => default_registry()? },
            "campaign":format!("campaign-{}",request.run_id)},
        "frozen":{},"environment":request.environment,"maxMutants":request.max_mutants,
        "families":request.families.iter().map(Family::to_value).collect::<Vec<_>>()}))?;
    Ok(())
}

fn runtime_value(runtime: &RuntimeInputs) -> Value {
    json!({"installation":runtime.installation,"runner":runtime.runner,
        "dartSdk":runtime.dart_sdk,"flutterSdk":runtime.flutter_sdk,
        "adapterRoot":runtime.adapter_root})
}

pub(crate) struct Preflight {
    pub(crate) original: PreparedProject,
    pub(crate) hashes: Hashes,
    pub(crate) runtime: RuntimeInputs,
    pub(crate) output: PathBuf,
    pub(crate) registry: PathBuf,
    pub(crate) families: Vec<Family>,
    pub(crate) run_id: String,
    pub(crate) mode: Mode,
    pub(crate) replay: Option<Arc<LoadedReplay>>,
    original_guard: DirectoryGuard,
}

fn executable(path: &Path) -> Result<(), NamespaceError> {
    if !path.is_file() {
        return Err(invalid("missing executable campaign tool"));
    }
    #[cfg(unix)]
    let name = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| invalid("invalid executable path"))?;
    // SAFETY: name is terminated and remains live during access.
    #[cfg(unix)]
    if unsafe { libc::access(name.as_ptr(), libc::X_OK) } != 0 {
        return Err(invalid("native campaign tools must be executable"));
    }
    Ok(())
}

fn validate_runtime(runtime: &mut RuntimeInputs, flutter: bool) -> Result<(), NamespaceError> {
    if flutter && runtime.flutter_sdk.is_none() {
        if let Some(candidate) = runtime.dart_sdk.ancestors().nth(3) {
            if candidate
                .join(failforge_config::platform::FLUTTER_BIN)
                .is_file()
            {
                runtime.flutter_sdk = Some(candidate.to_owned());
            }
        }
    }
    for path in [
        &runtime.runner,
        &runtime.dart_sdk.join(failforge_config::platform::DART_BIN),
    ] {
        executable(path)?;
    }
    for path in [
        runtime.adapter_root.join("bin/failforge_dart_adapter.dart"),
        runtime.adapter_root.join(".dart_tool/package_config.json"),
    ] {
        if !path.is_file() {
            return Err(invalid("campaign requires a prepared Dart adapter"));
        }
    }
    if flutter {
        executable(
            &runtime
                .flutter_sdk
                .as_ref()
                .ok_or_else(|| invalid("Flutter project requires a Flutter SDK"))?
                .join(failforge_config::platform::FLUTTER_BIN),
        )?;
    }
    Ok(())
}

pub(crate) fn literal_includes(project: &Path, rules: &Value) -> Result<(), NamespaceError> {
    let selections = rules
        .get("includePaths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            rules
                .get("excludePaths")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
    for pattern in selections {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| invalid("path patterns must be strings"))?;
        if pattern.starts_with('/')
            || pattern.contains(['\\', '\0'])
            || pattern.split('/').any(|part| part == "..")
        {
            return Err(invalid("path patterns must stay package-relative"));
        }
    }
    for pattern in rules
        .get("includePaths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let pattern = pattern
            .as_str()
            .ok_or_else(|| invalid("path patterns must be strings"))?;
        if !pattern.contains(['*', '?', '[', ']', '{', '}']) && !project.join(pattern).is_file() {
            return Err(invalid(
                "literal include must name an existing project file",
            ));
        }
    }
    Ok(())
}

fn is_alias(info: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        info.file_type().is_symlink()
    }
    #[cfg(windows)]
    {
        win::is_reparse(info)
    }
}

fn no_symlink_ancestors(path: &Path, fresh: bool) -> Result<(), NamespaceError> {
    absolute(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                #[cfg(windows)]
                {
                    current.push(component);
                    continue;
                }
                #[cfg(unix)]
                return Err(invalid("campaign output path must be normalized"));
            }
            Component::RootDir | Component::Normal(_) => current.push(component),
            _ => return Err(invalid("campaign output path must be normalized")),
        }
        match fs::symlink_metadata(&current) {
            Ok(info) if is_alias(&info) => {
                return Err(invalid("campaign output ancestors must not be symlinks"));
            }
            Ok(_) if fresh && current == path => {
                return Err(invalid("campaign output already exists"));
            }
            Ok(info) if !info.is_dir() => {
                return Err(invalid("campaign output ancestor is not a directory"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if path.file_name().is_none() {
        return Err(invalid("campaign output needs a directory name"));
    }
    Ok(())
}

fn canonical_missing(path: &Path) -> Result<PathBuf, NamespaceError> {
    absolute(path)?;
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("campaign registry path must be normalized"));
    }
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(info) => {
                if ancestor == path && is_alias(&info) {
                    return Err(invalid("campaign registry must not be a symlink"));
                }
                if !info.is_dir() && !is_alias(&info) {
                    return Err(invalid("campaign registry ancestor is not a directory"));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| invalid("invalid campaign registry path"))?,
                );
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| invalid("invalid campaign registry parent"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut result = ancestor.canonicalize()?;
    for name in missing.into_iter().rev() {
        result.push(name);
    }
    Ok(result)
}

#[cfg(unix)]
fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
#[cfg(windows)]
fn overlaps(left: &Path, right: &Path) -> bool {
    path_starts_with(left, right) || path_starts_with(right, left)
}
#[cfg(unix)]
pub(crate) fn path_starts_with(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}
#[cfg(windows)]
pub(crate) fn path_starts_with(path: &Path, root: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};
    let mut path = path.components();
    for expected in root.components() {
        let Some(actual) = path.next() else {
            return false;
        };
        let left: Vec<_> = actual.as_os_str().encode_wide().collect();
        let right: Vec<_> = expected.as_os_str().encode_wide().collect();
        // SAFETY: read-only native ordinal case-insensitive comparison of live
        // UTF-16 component buffers, whose bounded path lengths fit native i32.
        if unsafe {
            CompareStringOrdinal(
                left.as_ptr(),
                left.len() as i32,
                right.as_ptr(),
                right.len() as i32,
                1,
            )
        } != CSTR_EQUAL
        {
            return false;
        }
    }
    true
}

pub(crate) fn preflight(
    request: &Request,
    cancel: &Cancellation,
) -> Result<Preflight, NamespaceError> {
    validate_request(request)?;
    cancel.check()?;
    let original = inspect_project(&request.project, cancel)?;
    let original_guard = DirectoryGuard::open(&original.root)?;
    let replay = request
        .replay
        .as_ref()
        .map(|paths| LoadedReplay::load(paths, &original, cancel).map(Arc::new))
        .transpose()?;
    let mut runtime = request.runtime.clone();
    validate_runtime(&mut runtime, original.flutter)?;
    let mut families = request.families.clone();
    families.sort_by_key(|family| FAMILIES.iter().position(|name| *name == family.family));
    for family in &mut families {
        cancel.check()?;
        if request.mode == Mode::Profile {
            literal_includes(&original.root, &family.rules)?;
        }
        if family.family != "classic" && family.rules.get("operators").is_none() {
            family.rules["operators"] = json!(
                default_operators(&family.family, original.flutter)
                    .ok_or_else(|| invalid("unknown mutation family"))?
            );
        }
        if !original.flutter
            && family.rules["operators"]
                .as_array()
                .is_some_and(|operators| operators.iter().any(|op| op == "METHOD_CHANNEL_ERROR"))
        {
            return Err(invalid(
                "METHOD_CHANNEL_ERROR requires a prepared Flutter project",
            ));
        }
    }
    let hashes = source_hashes(&original.root, cancel)?;
    let output = request
        .output
        .clone()
        .unwrap_or_else(|| original.root.join(".failforge/runs").join(&request.run_id));
    no_symlink_ancestors(&output, true)?;
    if path_starts_with(&original.root, &output)
        || path_starts_with(&output, &original.root)
            && !path_starts_with(&output, &original.root.join(".failforge"))
    {
        return Err(invalid(
            "campaign output inside original must be under .failforge",
        ));
    }
    let registry = canonical_missing(&match &request.registry {
        Some(path) => path.clone(),
        None => default_registry()?,
    })?;
    let mut protected = vec![
        runtime.dart_sdk.canonicalize()?,
        runtime.adapter_root.canonicalize()?,
        runtime.runner.canonicalize()?,
    ];
    if let Some(flutter) = &runtime.flutter_sdk {
        protected.push(flutter.canonicalize()?);
    }
    protected.extend(
        original
            .packages
            .values()
            .filter(|path| **path != original.root)
            .cloned(),
    );
    if let Some(replay) = &replay {
        protected.extend(replay.protected_paths());
    }
    if protected.iter().any(|path| overlaps(&output, path)) || overlaps(&output, &registry) {
        return Err(invalid("campaign report overlaps protected inputs"));
    }
    protected.push(original.root.clone());
    if protected.iter().any(|path| overlaps(&registry, path)) {
        return Err(invalid(
            "temporary workspace registry must be outside campaign inputs",
        ));
    }
    cancel.check()?;
    original_guard.verify()?;
    Ok(Preflight {
        original,
        hashes,
        runtime,
        output,
        registry,
        families,
        run_id: request.run_id.clone(),
        mode: request.mode,
        replay,
        original_guard,
    })
}

#[cfg(unix)]
fn verify_named(directory: &File, path: &Path) -> Result<(), NamespaceError> {
    let held = directory.metadata()?;
    let named = fs::symlink_metadata(path)?;
    if !named.is_dir() || held.dev() != named.dev() || held.ino() != named.ino() {
        return Err(invalid("campaign report directory changed"));
    }
    Ok(())
}

#[cfg(unix)]
fn create_directory(path: &Path) -> Result<DirectoryGuard, NamespaceError> {
    no_symlink_ancestors(path, true)?;
    let mut current = PathBuf::from("/");
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&current)?;
    let components = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (index, name) in components.iter().enumerate() {
        let name = CString::new(name.as_bytes())
            .map_err(|_| invalid("invalid campaign output component"))?;
        verify_named(&directory, &current)?;
        // SAFETY: one terminated component is resolved relative to an owned FD.
        let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } == 0;
        if !created {
            let error = io::Error::last_os_error();
            if index + 1 == components.len() || error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        // SAFETY: the returned descriptor is unique and refuses a final symlink.
        let descriptor = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful openat returned this unique descriptor.
        let child = unsafe { File::from_raw_fd(descriptor) };
        if created {
            // SAFETY: chmod acts on the created directory, never a pathname alias.
            if unsafe { libc::fchmod(child.as_raw_fd(), 0o700) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        verify_named(&directory, &current)?;
        current.push(components[index]);
        verify_named(&child, &current)?;
        directory = child;
    }
    no_symlink_ancestors(path, false)?;
    let guard = DirectoryGuard::open(path)?;
    verify_named(&directory, path)?;
    Ok(guard)
}

#[cfg(windows)]
fn verify_named(directory: &File, path: &Path) -> Result<(), NamespaceError> {
    if win::identity(&win::directory(path)?)? != win::identity(directory)? {
        return Err(invalid("campaign report directory changed"));
    }
    Ok(())
}
#[cfg(windows)]
fn create_directory(path: &Path) -> Result<DirectoryGuard, NamespaceError> {
    no_symlink_ancestors(path, true)?;
    let mut parts = path.components();
    let prefix = parts
        .next()
        .ok_or_else(|| invalid("campaign output needs a volume"))?;
    if !matches!(prefix, Component::Prefix(_)) || parts.next() != Some(Component::RootDir) {
        return Err(invalid("campaign output must be absolute"));
    }
    let mut volume = prefix.as_os_str().to_os_string();
    volume.push("\\");
    let mut current = PathBuf::from(volume);
    let mut directory = win::directory(&current)?;
    let components: Vec<_> = parts.collect();
    for (index, part) in components.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(invalid("campaign output path must be normalized"));
        };
        verify_named(&directory, &current)?;
        let child = match win::create_directory_at(&directory, name) {
            Ok(child) => child,
            Err(error)
                if index + 1 < components.len() && error.kind() == io::ErrorKind::AlreadyExists =>
            {
                win::directory_at(&directory, name)?
            }
            Err(error) => return Err(error.into()),
        };
        verify_named(&directory, &current)?;
        current.push(name);
        verify_named(&child, &current)?;
        directory = child;
    }
    no_symlink_ancestors(path, false)?;
    let guard = DirectoryGuard::open(path)?;
    verify_named(&directory, path)?;
    Ok(guard)
}

pub(crate) fn create_output(preflight: &Preflight) -> Result<DirectoryGuard, NamespaceError> {
    verify_original(preflight)?;
    let output = create_directory(&preflight.output)?;
    if preflight.mode == Mode::Profile {
        for family in &preflight.families {
            output.verify()?;
            create_directory(&preflight.output.join("families").join(&family.family))?;
        }
    }
    output.verify()?;
    Ok(output)
}

pub(crate) fn verify_original(preflight: &Preflight) -> Result<(), NamespaceError> {
    preflight.original_guard.verify()?;
    if let Some(replay) = &preflight.replay {
        replay.verify()?;
    }
    verify_source(
        &preflight.original.root,
        &preflight.hashes,
        &Cancellation::default(),
    )?;
    preflight.original_guard.verify()
}

#[cfg(unix)]
fn block_cleanup(campaign: &File) -> io::Result<()> {
    let name = CString::new(".cleanup-blocked").expect("constant filename");
    // The existing V1 reaper rejects an extra control entry before touching work.
    // Create it under the retained campaign inode while our lease is still held.
    // SAFETY: one terminated constant name is relative to an owned directory.
    let descriptor = unsafe {
        libc::openat(
            campaign.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::AlreadyExists {
            // Its contents and type are irrelevant: never overwrite another
            // entry, and any extra name prevents automatic recursive deletion.
            return campaign.sync_all();
        }
        return Err(error);
    }
    // SAFETY: successful openat returned this unique newly created descriptor.
    let mut marker = unsafe { File::from_raw_fd(descriptor) };
    // SAFETY: chmod acts on our exclusively created file, never a pathname.
    if unsafe { libc::fchmod(marker.as_raw_fd(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    marker.write_all(b"FAILFORGE_CLEANUP_BLOCKED_V1\n")?;
    marker.sync_all()?;
    campaign.sync_all()
}

#[cfg(windows)]
fn block_cleanup(campaign: &File) -> io::Result<()> {
    let mut marker = match win::regular_at(
        campaign,
        std::ffi::OsStr::new(".cleanup-blocked"),
        true,
        true,
    ) {
        Ok(marker) => marker,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    marker.write_all(b"FAILFORGE_CLEANUP_BLOCKED_V1\n")?;
    marker.sync_all()
}

pub(crate) struct Workspace {
    root: PathBuf,
    campaign: String,
    caps: LifecycleCaps,
    recovery: Value,
    snapshot: Option<BoundAllocation>,
    project: Option<PreparedProject>,
    snapshot_path: Option<PathBuf>,
}

impl Workspace {
    pub(crate) fn start(
        preflight: &Preflight,
        cancel: &Cancellation,
    ) -> Result<Self, NamespaceError> {
        verify_original(preflight)?;
        let started = lifecycle::start(&preflight.registry, cancel)?;
        Ok(Self {
            root: started.root_path,
            campaign: started.campaign_name,
            caps: started.caps,
            recovery: json!({"recovered":started.recovered,"skipped":started.skipped}),
            snapshot: None,
            project: None,
            snapshot_path: None,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn campaign(&self) -> &str {
        &self.campaign
    }

    pub(crate) fn directory(&self) -> PathBuf {
        self.root.join(&self.campaign)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn snapshot_path(&self) -> Option<&Path> {
        self.snapshot_path.as_deref()
    }

    pub(crate) fn recovery(&self) -> Value {
        self.recovery.clone()
    }

    pub(crate) fn caps(&self) -> Result<LifecycleCaps, NamespaceError> {
        Ok(self.caps.try_clone()?)
    }

    pub(crate) fn snapshot(
        &mut self,
        preflight: &Preflight,
        cancel: &Cancellation,
    ) -> Result<PreparedProject, NamespaceError> {
        self.snapshot_using(preflight, cancel, |source, destination, cancel| {
            Ok(copy_campaign_snapshot(source, destination, cancel)?)
        })
    }

    fn snapshot_using(
        &mut self,
        preflight: &Preflight,
        cancel: &Cancellation,
        copy: impl FnOnce(&Path, &Path, &Cancellation) -> Result<PreparedProject, NamespaceError>,
    ) -> Result<PreparedProject, NamespaceError> {
        if self.snapshot.is_some() {
            return Err(invalid("campaign snapshot was already allocated"));
        }
        verify_original(preflight)?;
        let allocation = lifecycle::allocate_bound(
            &self.root,
            &self.campaign,
            self.caps.try_clone()?,
            "snapshot",
            cancel,
        )?;
        let destination = allocation.path().join("package");
        self.snapshot_path = Some(destination.clone());
        self.snapshot = Some(allocation);
        let project = copy(&preflight.original.root, &destination, cancel)?;
        self.snapshot.as_ref().unwrap().verify(cancel)?;
        verify_original(preflight)?;
        self.project = Some(project.clone());
        Ok(project)
    }

    pub(crate) fn bind_inputs(
        &self,
        preflight: &Preflight,
        environment: &BTreeMap<String, String>,
        cancel: &Cancellation,
    ) -> Result<Value, NamespaceError> {
        let project = self
            .project
            .as_ref()
            .ok_or_else(|| invalid("campaign snapshot has not completed"))?;
        self.snapshot
            .as_ref()
            .ok_or_else(|| invalid("campaign snapshot allocation is missing"))?
            .verify(cancel)?;
        Ok(crate::workspace_inputs::bind_inputs(
            &preflight.runtime,
            project,
            environment,
            cancel,
        )?)
    }

    /// Call only after every child owner has drained. Cleanup never consults the
    /// execution cancellation token and one failure cannot skip later cleanup.
    pub(crate) fn finish(mut self, preflight: &Preflight) -> Cleanup {
        let cancel = Cancellation::default();
        let mut result = Cleanup {
            original_unchanged: true,
            snapshot_cleaned: self.snapshot.is_none(),
            cleanup_deferred: false,
            errors: vec![],
            skipped: vec![],
        };
        if let Err(error) = verify_original(preflight) {
            result.original_unchanged = false;
            result.errors.push(error.to_string());
        }
        let mut blocked = false;
        if let Some(mut snapshot) = self.snapshot.take() {
            match snapshot.remove(&cancel) {
                Ok(()) => result.snapshot_cleaned = true,
                Err(error) => {
                    blocked = true;
                    result.errors.push(error.to_string());
                }
            }
            drop(snapshot);
        }
        if blocked {
            result.cleanup_deferred = true;
            if let Err(error) = block_cleanup(&self.caps.campaign) {
                result.errors.push(error.to_string());
            }
            result.skipped.push(Skip {
                name: self.campaign.clone(),
                reason: "workspace_allocation_cleanup_failed".into(),
                name_bytes: None,
            });
        }
        self.caps.lease.take();
        if blocked {
            // A generic reaper must not bypass the allocation's failed binding
            // check, now or on the next campaign's automatic recovery.
            return result;
        }
        match lifecycle::close(&self.root, &self.campaign, self.caps, &cancel) {
            Ok(closed) => {
                result.cleanup_deferred = closed.cleanup_deferred;
                result.skipped = closed.skipped;
            }
            Err(error) => {
                result.cleanup_deferred = true;
                result.errors.push(error.to_string());
            }
        }
        result
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Cleanup {
    pub(crate) original_unchanged: bool,
    pub(crate) snapshot_cleaned: bool,
    pub(crate) cleanup_deferred: bool,
    pub(crate) errors: Vec<String>,
    pub(crate) skipped: Vec<Skip>,
}

#[cfg(all(test, unix))]
#[path = "campaign_workspace_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn windows_output_paths_preserve_volume_and_reject_existing_directory() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "failforge-campaign-output-{}",
            win::random_token().unwrap()
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("nested/report");
        let guard = create_directory(&path).unwrap();
        guard.verify().unwrap();
        win::require_private(&win::directory(&path).unwrap(), true).unwrap();
        assert!(create_directory(&path).is_err());
        assert!(no_symlink_ancestors(&root.join("a/../b"), true).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn windows_protected_overlap_is_ordinal_case_insensitive() {
        assert!(overlaps(Path::new(r"C:\SDK\bin"), Path::new(r"c:\sdk")));
        assert!(!overlaps(Path::new(r"C:\SDK-other"), Path::new(r"c:\sdk")));
        assert!(
            default_registry()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("failforge-workspaces-S-")
        );
    }
}

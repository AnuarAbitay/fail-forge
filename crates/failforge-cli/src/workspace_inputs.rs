//! Private filesystem operations; the campaign retains scheduling and leases.
use std::collections::BTreeMap;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::ExitCode;

#[cfg(unix)]
use crate::signals::{SignalKind, signal};
#[cfg(unix)]
use failforge_runner::WorkspaceLease;
use failforge_workspace::{self as inputs, BoundTree, Cancellation, InputError, PreparedProject};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(unix)]
const REQUEST_ENV: &str = "FAILFORGE_WORKSPACE_REQUEST_FD";

#[cfg(unix)]
#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
enum Request {
    Inspect {
        root: PathBuf,
    },
    SourceHashes {
        root: PathBuf,
    },
    DependencyRoots {
        path: PathBuf,
    },
    BoundTree {
        root: PathBuf,
    },
    CopyProject {
        source: PathBuf,
        destination: PathBuf,
    },
    CopySnapshot {
        source: PathBuf,
        destination: PathBuf,
    },
    BindInputs {
        runtime: Box<RuntimeInputs>,
        project: PreparedProject,
        environment: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimeInputs {
    pub(crate) installation: PathBuf,
    pub(crate) runner: PathBuf,
    pub(crate) dart_sdk: PathBuf,
    pub(crate) flutter_sdk: Option<PathBuf>,
    pub(crate) adapter_root: PathBuf,
}

fn invalid(message: &str) -> InputError {
    InputError::Invalid(message.into())
}

#[cfg(unix)]
fn absolute(path: &Path) -> Result<(), InputError> {
    if !path.is_absolute() || path.to_str().is_none_or(|text| text.contains('\0')) {
        return Err(invalid(
            "workspace request paths must be absolute Unicode paths",
        ));
    }
    Ok(())
}

#[cfg(unix)]
impl Request {
    fn validate(&self) -> Result<(), InputError> {
        match self {
            Self::Inspect { root } | Self::SourceHashes { root } | Self::BoundTree { root } => {
                absolute(root)
            }
            Self::DependencyRoots { path } => absolute(path),
            Self::CopyProject {
                source,
                destination,
            }
            | Self::CopySnapshot {
                source,
                destination,
            } => {
                absolute(source)?;
                absolute(destination)
            }
            Self::BindInputs {
                runtime, project, ..
            } => {
                for path in [
                    &runtime.installation,
                    &runtime.runner,
                    &runtime.dart_sdk,
                    &runtime.adapter_root,
                    &project.root,
                ] {
                    absolute(path)?;
                }
                if let Some(path) = &runtime.flutter_sdk {
                    absolute(path)?;
                }
                for path in project.packages.values() {
                    absolute(path)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(unix)]
fn decode(bytes: &[u8]) -> Result<Request, InputError> {
    let request: Request = serde_json::from_value(inputs::parse_json(bytes)?)
        .map_err(|_| invalid("invalid native workspace request"))?;
    request.validate()?;
    Ok(request)
}

#[cfg(unix)]
fn value<T: serde::Serialize>(value: T) -> Result<Value, InputError> {
    serde_json::to_value(value)
        .map_err(|_| invalid("workspace result cannot be represented as JSON"))
}

#[cfg(unix)]
fn dispatch(request: Request, cancellation: &Cancellation) -> Result<Value, InputError> {
    cancellation.check()?;
    match request {
        Request::Inspect { root } => value(inputs::inspect_project(&root, cancellation)?),
        Request::SourceHashes { root } => value(inputs::source_hashes(&root, cancellation)?),
        Request::DependencyRoots { path } => value(inputs::dependency_roots(&path)?),
        Request::BoundTree { root } => value(inputs::bound_tree(&root, cancellation)?),
        Request::CopyProject {
            source,
            destination,
        } => value(inputs::copy_project(&source, &destination, cancellation)?),
        Request::CopySnapshot {
            source,
            destination,
        } => value(inputs::copy_campaign_snapshot(
            &source,
            &destination,
            cancellation,
        )?),
        Request::BindInputs {
            runtime,
            project,
            environment,
        } => bind_inputs(&runtime, &project, &environment, cancellation),
    }
}

/// Hash each canonical tree once per binding, never across executions. Reusing a
/// completed binding would hide edits between a baseline and a mutant attempt.
struct Trees<'a> {
    values: BTreeMap<PathBuf, BoundTree>,
    cancellation: &'a Cancellation,
}

impl Trees<'_> {
    fn get(&mut self, path: &Path) -> Result<BoundTree, InputError> {
        self.cancellation.check()?;
        let root = path.canonicalize()?;
        if !self.values.contains_key(&root) {
            self.values
                .insert(root.clone(), inputs::bound_tree(&root, self.cancellation)?);
        }
        Ok(self.values[&root].clone())
    }

    fn dependencies(
        &mut self,
        roots: &BTreeMap<String, PathBuf>,
        own: &Path,
    ) -> Result<BTreeMap<String, BoundTree>, InputError> {
        roots
            .iter()
            .filter(|(_, path)| path.as_path() != own)
            .map(|(name, path)| self.get(path).map(|tree| (name.clone(), tree)))
            .collect()
    }
}

pub(crate) fn bind_inputs(
    runtime: &RuntimeInputs,
    project: &PreparedProject,
    environment: &BTreeMap<String, String>,
    cancellation: &Cancellation,
) -> Result<Value, InputError> {
    let adapter_config = runtime.adapter_root.join(".dart_tool/package_config.json");
    let adapter_dependencies = inputs::dependency_roots(&adapter_config)?;
    let current_dependencies =
        inputs::dependency_roots(&project.root.join(".dart_tool/package_config.json"))?;
    if current_dependencies != project.packages {
        return Err(invalid("prepared dependency roots changed during campaign"));
    }
    let mut paths = vec![runtime.runner.clone(), adapter_config];
    let graph = runtime.adapter_root.join(".dart_tool/package_graph.json");
    if graph.is_file() {
        paths.push(graph);
    }
    let mut sdk_roots = vec![runtime.dart_sdk.join("bin"), runtime.dart_sdk.join("lib")];
    // A bundled Dart SDK can be used without invoking Flutter. Bind Flutter's
    // tools and engine only when the prepared test framework actually uses them.
    if let Some(flutter) = runtime.flutter_sdk.as_ref().filter(|_| project.flutter) {
        for relative in [
            failforge_config::platform::FLUTTER_BIN,
            "bin/cache/flutter_tools.snapshot",
        ] {
            let path = flutter.join(relative);
            if path.is_file() {
                paths.push(path);
            }
        }
        let engine = flutter.join("bin/cache/artifacts/engine");
        if engine.is_dir() {
            sdk_roots.push(engine);
        }
    }
    let mut trees = Trees {
        values: BTreeMap::new(),
        cancellation,
    };
    let source = inputs::source_hashes(&project.root, cancellation)?;
    let dependencies = trees.dependencies(&project.packages, &project.root)?;
    let adapter_bound = trees.dependencies(&adapter_dependencies, &runtime.adapter_root)?;
    let sdk = sdk_roots
        .into_iter()
        .map(|path| trees.get(&path).map(|tree| (path, tree)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let adapter = trees.get(&runtime.adapter_root)?;
    let tools = paths
        .into_iter()
        .map(|path| inputs::hash_file(&path, cancellation).map(|hash| (path, hash)))
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(
        json!({"project":source,"dependencies":dependencies,"adapterDependencies":adapter_bound,
        "adapterDependencyRoots":adapter_dependencies,"sdk":sdk,"adapter":adapter,
        "executionEngine":"rust","tools":tools,"environmentSha256":environment_hash(environment)}),
    )
}

// Preserve the existing binding contract: json.dumps(sorted(env.items())) with
// default ASCII escaping. Environment values themselves never enter a report.
fn environment_json(environment: &BTreeMap<String, String>) -> String {
    fn string(value: &str, output: &mut String) {
        use std::fmt::Write;
        output.push('"');
        for character in value.chars() {
            match character {
                '"' => output.push_str("\\\""),
                '\\' => output.push_str("\\\\"),
                '\u{8}' => output.push_str("\\b"),
                '\u{c}' => output.push_str("\\f"),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                ' '..='~' => output.push(character),
                other => {
                    for unit in other.encode_utf16(&mut [0; 2]) {
                        write!(output, "\\u{unit:04x}").expect("String writer");
                    }
                }
            }
        }
        output.push('"');
    }
    let mut result = String::from("[");
    for (index, (key, val)) in environment.iter().enumerate() {
        if index != 0 {
            result.push_str(", ");
        }
        result.push('[');
        string(key, &mut result);
        result.push_str(", ");
        string(val, &mut result);
        result.push(']');
    }
    result.push(']');
    result
}

fn environment_hash(environment: &BTreeMap<String, String>) -> String {
    format!(
        "{:x}",
        Sha256::digest(environment_json(environment).as_bytes())
    )
}

#[cfg(unix)]
async fn process(
    request: Request,
    readiness: Option<crate::readiness::Readiness>,
) -> Result<(), InputError> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let cancellation = Cancellation::default();
    let listening = cancellation.clone();
    let listener = tokio::spawn(async move {
        let signum = tokio::select! { _ = interrupt.recv() => libc::SIGINT, _ = terminate.recv() => libc::SIGTERM };
        listening.request(signum);
    });
    if let Some(readiness) = readiness {
        readiness.acknowledge()?;
    }
    let working = cancellation.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = dispatch(request, &working)?;
        working.check()?;
        let response = crate::private_response::encode(result)?;
        crate::private_response::publish(&response, &working)
    })
    .await
    .map_err(|_| invalid("native workspace operation failed"))?;
    listener.abort();
    let _ = listener.await;
    result?;
    cancellation.check()
}

#[cfg(unix)]
pub(crate) fn execute() -> ExitCode {
    let result = (|| -> Result<(), InputError> {
        // Hold any inherited campaign lease until all filesystem work ends.
        let _lease = WorkspaceLease::from_env()?;
        let readiness = crate::readiness::Readiness::from_env()?;
        let descriptor = std::env::var_os(REQUEST_ENV)
            .ok_or_else(|| invalid("missing native workspace request"))?;
        let request = decode(&crate::private_request::read(&descriptor)?)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(process(request, readiness))
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "failforge: {error}");
            ExitCode::from(match error {
                InputError::Cancelled(libc::SIGTERM) => 143,
                InputError::Cancelled(_) => 130,
                _ => 125,
            })
        }
    }
}

#[cfg(all(test, unix))]
#[path = "workspace_inputs_tests.rs"]
mod tests;

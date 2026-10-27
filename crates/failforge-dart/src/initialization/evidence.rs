//! Runtime loader evidence. A signature is an observation, never a kill verdict.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::InitializationError;
use crate::test_catalog::{path_uri, python_json_string, resolve_existing};
use crate::test_outcome::{MAX_CAPTURE_BYTES, MAX_EVENT_BYTES, MAX_EVENTS, ReporterMode};
use crate::test_reporter::{LoadFailure, Reporter, parse_event};

const MAX_SIGNATURE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureKind {
    SubjectStack,
    AuthorNullCheck,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedDiagnostic {
    pub error: String,
    pub stack_trace: String,
    pub is_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedLoadFailure {
    pub suite_path: String,
    pub test_name: String,
    pub errors: Vec<NormalizedDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationSignature {
    pub kind: SignatureKind,
    pub signature: Vec<NormalizedLoadFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializationObservation {
    pub suites: Vec<String>,
    pub signature: Option<ConfirmationSignature>,
    pub failed_suite: Option<String>,
}

/// Extract selected suites and eligible runtime diagnostics from a complete,
/// validated reporter capture. The caller must first authenticate the capture
/// against the native Runner receipt. No original/mutant controls run here.
pub fn analyze_initialization_evidence(
    raw: &[u8],
    cwd: &Path,
    mode: ReporterMode,
    classification: &Value,
    mutant: Option<&Value>,
    alias: &str,
) -> Result<InitializationObservation, InitializationError> {
    if raw.len() > MAX_CAPTURE_BYTES {
        return Err(InitializationError("capture_limit_exceeded"));
    }
    if raw.iter().filter(|byte| **byte == b'\n').count() >= MAX_EVENTS {
        return Err(InitializationError("event_limit_exceeded"));
    }
    let directory = Directory::new(cwd)?;
    let mut reporter = Reporter::new(match mode {
        ReporterMode::Dart => "dart",
        ReporterMode::Flutter => "flutter",
    });
    let mut suites = BTreeSet::new();
    let mut suite_budget = MAX_SIGNATURE_BYTES;
    for line in raw.split(|byte| *byte == b'\n') {
        if line.iter().all(|byte| matches!(byte, b' ' | b'\t' | b'\r')) {
            continue;
        }
        if line.len() > MAX_EVENT_BYTES {
            return Err(InitializationError("event_limit_exceeded"));
        }
        let event = parse_event(line).map_err(|error| InitializationError(error.0))?;
        reporter
            .consume(&event)
            .map_err(|error| InitializationError(error.0))?;
        if event["type"] == "suite" {
            let suite = event["suite"]["path"]
                .as_str()
                .expect("accepted suite path");
            let relative = directory.relative(suite)?;
            suite_budget = suite_budget
                .checked_sub(
                    relative
                        .len()
                        .saturating_add(std::mem::size_of::<String>() * 3),
                )
                .ok_or(InitializationError("initialization_suite_limit_exceeded"))?;
            if !suites.insert(relative) {
                return Err(InitializationError("duplicate_initialization_suite"));
            }
        }
    }
    if !reporter.finished {
        return Err(InitializationError("missing_done_event"));
    }
    let mut observation = InitializationObservation {
        suites: suites.into_iter().collect(),
        signature: None,
        failed_suite: None,
    };
    // Never derive a runtime signature from externally supplied diagnostics:
    // they must be exactly the accepted native reporter observations.
    if classification["outcome"] != "LOAD_FAILURE"
        || classification["reporterComplete"] != true
        || classification["loadFailures"]
            != serde_json::to_value(&reporter.load_failures).expect("serializable diagnostics")
        || classification["tests"]
            != serde_json::to_value(&reporter.counts).expect("serializable counts")
    {
        return Ok(observation);
    }
    let Some(target) = mutant
        .and_then(|value| value.get("file"))
        .and_then(Value::as_str)
        .filter(|target| safe_target(target))
    else {
        return Ok(observation);
    };
    if alias.len() > 1024 || alias.contains('\0') {
        return Ok(observation);
    }
    if let Some(signature) = subject_signature(&reporter.load_failures, &directory, target, alias) {
        observation.signature = Some(ConfirmationSignature {
            kind: SignatureKind::SubjectStack,
            signature,
        });
    } else if let Some((signature, suite)) =
        null_signature(&reporter.load_failures, &directory, classification)
    {
        observation.signature = Some(ConfirmationSignature {
            kind: SignatureKind::AuthorNullCheck,
            signature,
        });
        observation.failed_suite = Some(suite);
    }
    Ok(observation)
}

fn safe_target(target: &str) -> bool {
    target.starts_with("lib/")
        && target.ends_with(".dart")
        && target.len() <= 32 * 1024
        && !target.contains(['\\', '\0'])
        && target
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

struct Directory {
    logical: PathBuf,
    resolved: PathBuf,
    spellings: Vec<String>,
}

impl Directory {
    fn new(cwd: &Path) -> Result<Self, InitializationError> {
        if !cwd.is_absolute() || cwd.to_str().is_none_or(|path| path.contains('\0')) {
            return Err(InitializationError("invalid_initialization_cwd"));
        }
        // pathlib.Path strips redundant separators and `.` while retaining
        // `..`; stack locations follow that lexical spelling before resolving.
        let logical: PathBuf = cwd.components().collect();
        let cwd = logical.as_path();
        let resolved =
            resolve_existing(cwd).map_err(|_| InitializationError("invalid_initialization_cwd"))?;
        let mut spellings = vec![
            text(cwd)?.to_owned(),
            text(&resolved)?.to_owned(),
            path_uri(cwd).map_err(|_| InitializationError("invalid_initialization_cwd"))?,
            path_uri(&resolved).map_err(|_| InitializationError("invalid_initialization_cwd"))?,
        ];
        #[cfg(windows)]
        {
            let variants: Vec<_> = spellings
                .iter()
                .flat_map(|value| {
                    [
                        value.replace('\\', "/"),
                        value.strip_prefix(r"\\?\").unwrap_or(value).to_owned(),
                    ]
                })
                .collect();
            spellings.extend(variants);
        }
        spellings.sort();
        spellings.dedup();
        spellings.sort_by_key(|spelling| std::cmp::Reverse(spelling.len()));
        Ok(Self {
            logical: cwd.to_owned(),
            resolved,
            spellings,
        })
    }

    fn relative(&self, name: &str) -> Result<String, InitializationError> {
        if name.contains('\0') {
            return Err(InitializationError("invalid_initialization_suite"));
        }
        let path = self.resolved.join(name);
        let resolved = resolve_existing(&path)
            .map_err(|_| InitializationError("invalid_initialization_suite"))?;
        let relative = resolved
            .strip_prefix(&self.resolved)
            .map_err(|_| InitializationError("non_author_initialization_suite"))?;
        if relative.components().next() != Some(Component::Normal("test".as_ref())) {
            return Err(InitializationError("non_author_initialization_suite"));
        }
        Ok(text(relative)?.replace(std::path::MAIN_SEPARATOR, "/"))
    }

    fn locations(&self, relative: &Path, resolved: bool) -> Option<Vec<String>> {
        let mut paths = vec![
            text(relative).ok()?.to_owned(),
            text(&self.logical.join(relative)).ok()?.to_owned(),
            path_uri(&self.logical.join(relative)).ok()?,
        ];
        if resolved {
            paths.extend([
                text(&self.resolved.join(relative)).ok()?.to_owned(),
                path_uri(&self.resolved.join(relative)).ok()?,
            ]);
        }
        #[cfg(windows)]
        {
            // Suite identities use '/', while native test runner stack frames
            // use '\\'. Derive only exact path spellings, leaving URIs intact.
            let native_relative: PathBuf = relative.components().collect();
            paths.extend([
                text(&native_relative).ok()?.to_owned(),
                text(&self.logical.join(&native_relative)).ok()?.to_owned(),
            ]);
            if resolved {
                paths.push(text(&self.resolved.join(&native_relative)).ok()?.to_owned());
            }
            let variants: Vec<_> = paths
                .iter()
                .flat_map(|value| {
                    [
                        value.replace('\\', "/"),
                        value.strip_prefix(r"\\?\").unwrap_or(value).to_owned(),
                    ]
                })
                .collect();
            paths.extend(variants);
        }
        Some(paths)
    }

    fn normalize(&self, text: &str, budget: &mut usize) -> Option<String> {
        if text.len() > *budget {
            return None;
        }
        let mut output = text.to_owned();
        for spelling in &self.spellings {
            let occurrences = output.matches(spelling).count();
            let length = output
                .len()
                .checked_sub(occurrences.checked_mul(spelling.len())?)?
                .checked_add(occurrences.checked_mul("<workspace>".len())?)?;
            if length > *budget {
                return None;
            }
            output = output.replace(spelling, "<workspace>");
        }
        *budget = budget.checked_sub(output.len())?;
        Some(output)
    }
}

fn text(path: &Path) -> Result<&str, InitializationError> {
    path.to_str()
        .ok_or(InitializationError("non_utf8_initialization_path"))
}

static LOADER_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^Failed to load ["'][^\n]*?["']: "#).expect("fixed loader prefix")
});
static COORDINATES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A\d+:\d+(?:[\s\x1c-\x1f]|\z)").expect("fixed stack coordinates")
});

fn whitespace(character: char) -> bool {
    character.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&character)
}

// Search literal locations without compiling patterns from untrusted paths.
// Consuming one trailing whitespace reproduces the legacy ordered-frame rule.
fn frame_end(stack: &str, locations: &[String], start: usize) -> Option<usize> {
    let mut found = None;
    for location in locations {
        for (relative, _) in stack[start..].match_indices(location) {
            let index = start + relative;
            let previous = stack[..index].chars().next_back();
            if previous.is_some_and(|value| !whitespace(value)) {
                continue;
            }
            if index == start && start != 0 && previous != Some('\n') {
                continue;
            }
            let tail_start = index + location.len();
            let tail = &stack[tail_start..];
            let Some(separator) = tail
                .chars()
                .next()
                .filter(|value| *value == ':' || whitespace(*value))
            else {
                continue;
            };
            if let Some(coordinates) = COORDINATES.find(&tail[separator.len_utf8()..]) {
                let end = tail_start + separator.len_utf8() + coordinates.end();
                if found.is_none_or(|(before, _)| index < before) {
                    found = Some((index, end));
                }
            }
        }
    }
    found.map(|(_, end)| end)
}

fn runtime_error(body: &str) -> bool {
    const TYPES: &[&str] = &[
        "RangeError",
        "StateError",
        "TypeError",
        "NoSuchMethodError",
        "ArgumentError",
        "UnsupportedError",
        "UnimplementedError",
        "AssertionError",
        "FormatException",
        "Exception",
    ];
    TYPES.iter().any(|kind| {
        body.strip_prefix(kind).is_some_and(|tail| {
            tail.chars()
                .next()
                .is_none_or(|value| !value.is_alphanumeric() && value != '_')
        })
    }) || body.starts_with("Bad state:")
        || ["Invalid argument", "Invalid arguments"]
            .iter()
            .any(|prefix| {
                body.strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with([' ', ':', '(']))
            })
        || body.starts_with("Null check operator used on a null value")
        || body
            .strip_prefix("type '")
            .and_then(|tail| tail.split_once('\''))
            .is_some_and(|(name, tail)| {
                !name.is_empty()
                    && !name.contains('\n')
                    && tail.starts_with(" is not a subtype of type ")
            })
        || body == "Stack Overflow"
}

fn normalized(
    failure: &LoadFailure,
    directory: &Directory,
    budget: &mut usize,
) -> Option<NormalizedLoadFailure> {
    *budget = budget.checked_sub(std::mem::size_of::<NormalizedLoadFailure>())?;
    Some(NormalizedLoadFailure {
        suite_path: directory.normalize(&failure.suite_path, budget)?,
        test_name: directory.normalize(&failure.test_name, budget)?,
        errors: failure
            .errors
            .iter()
            .map(|error| {
                *budget = budget.checked_sub(std::mem::size_of::<NormalizedDiagnostic>())?;
                Some(NormalizedDiagnostic {
                    error: directory.normalize(&error.error, budget)?,
                    stack_trace: directory.normalize(&error.stack_trace, budget)?,
                    is_failure: error.is_failure,
                })
            })
            .collect::<Option<_>>()?,
    })
}

fn subject_signature(
    failures: &[LoadFailure],
    directory: &Directory,
    target: &str,
    alias: &str,
) -> Option<Vec<NormalizedLoadFailure>> {
    if failures.is_empty() {
        return None;
    }
    let locations = vec![
        format!("package:{alias}/{}", &target[4..]),
        text(&directory.logical.join(target)).ok()?.to_owned(),
        path_uri(&directory.logical.join(target)).ok()?,
    ];
    let mut result = Vec::with_capacity(failures.len());
    let mut budget = MAX_SIGNATURE_BYTES;
    for failure in failures {
        if failure.kind != "unknown" || failure.errors.is_empty() {
            return None;
        }
        let suite = Path::new(&failure.suite_path);
        let relative = if suite.is_absolute() {
            resolve_existing(suite)
                .ok()?
                .strip_prefix(&directory.resolved)
                .ok()?
                .to_owned()
        } else {
            suite.components().collect()
        };
        let authors = directory.locations(&relative, false)?;
        for error in &failure.errors {
            let body = LOADER_PREFIX.replace(&error.error, "");
            let subject = frame_end(&error.stack_trace, &locations, 0)?;
            if error.is_failure
                || !runtime_error(&body)
                || frame_end(&error.stack_trace, &authors, subject).is_none()
            {
                return None;
            }
        }
        result.push(normalized(failure, directory, &mut budget)?);
    }
    // Python's sorted-key ASCII JSON order differs from raw Unicode ordering.
    let mut sort_budget = MAX_SIGNATURE_BYTES;
    let mut keyed = Vec::with_capacity(result.len());
    for failure in result {
        // ASCII escaping can expand one byte into six. Account for the sort
        // representation before constructing it, including JSON punctuation.
        let strings = failure.errors.iter().fold(
            failure
                .suite_path
                .len()
                .saturating_add(failure.test_name.len()),
            |sum, error| {
                sum.saturating_add(error.error.len())
                    .saturating_add(error.stack_trace.len())
            },
        );
        let bound = strings
            .saturating_mul(6)
            .saturating_add(failure.errors.len().saturating_mul(64))
            .saturating_add(64);
        if bound > sort_budget {
            return None;
        }
        let key = signature_sort_key(&failure);
        sort_budget =
            sort_budget.checked_sub(key.len().saturating_add(std::mem::size_of::<String>()))?;
        keyed.push((key, failure));
    }
    keyed.sort_by(|(left, _), (right, _)| left.cmp(right));
    Some(keyed.into_iter().map(|(_, failure)| failure).collect())
}

fn signature_sort_key(failure: &NormalizedLoadFailure) -> String {
    let mut output = String::from("{\"errors\": [");
    for (index, error) in failure.errors.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        output.push_str("{\"error\": ");
        python_json_string(&error.error, &mut output);
        output.push_str(", \"isFailure\": ");
        output.push_str(if error.is_failure { "true" } else { "false" });
        output.push_str(", \"stackTrace\": ");
        python_json_string(&error.stack_trace, &mut output);
        output.push('}');
    }
    output.push_str("], \"suitePath\": ");
    python_json_string(&failure.suite_path, &mut output);
    output.push_str(", \"testName\": ");
    python_json_string(&failure.test_name, &mut output);
    output.push('}');
    output
}

static AUTHOR_TAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A(?::|[ \t]+)[1-9][0-9]*:[1-9][0-9]*[ \t]+main(?:\.<fn>)?[ \t]*\r?\z")
        .expect("fixed author frame")
});
static FRAMEWORK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\A(?:package:test_api(?:/src/backend/declarer\.dart)?(?:[ \t]+[1-9][0-9]*:[1-9][0-9]*)?[ \t]+Declarer\.group|package:test_core/src/scaffolding\.dart[ \t]+[1-9][0-9]*:[1-9][0-9]*[ \t]+group)[ \t]*\r?\z").expect("fixed framework frame")
});
static EXIT_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\Aexit:[1-9][0-9]{0,2}\z").expect("fixed exit code"));

fn null_signature(
    failures: &[LoadFailure],
    directory: &Directory,
    evidence: &Value,
) -> Option<(Vec<NormalizedLoadFailure>, String)> {
    if evidence["reason"] != "suite_load_failure_unattributed"
        || evidence["phase"] != "suite_loading"
        || evidence
            .get("testsPartial")
            .is_some_and(|value| value != false)
        || evidence.get("timeoutDiagnostics").is_some_and(truthy)
        || evidence["runner"]["cause"] != "ProcessCompleted"
        || evidence["runner"]["cleanup"] != "complete"
        || !evidence["runner"]["status"]
            .as_str()
            .is_some_and(|status| EXIT_CODE.is_match(status))
        || evidence["tests"]["failed"] != 0
        || evidence["tests"]["loadingFailed"] != 1
        || failures.len() != 1
    {
        return None;
    }
    let failure = &failures[0];
    if failure.kind != "unknown"
        || failure.errors.len() != 1
        || failure.suite_path.is_empty()
        || failure.suite_path.contains('\0')
        || (!cfg!(windows) && failure.suite_path.contains('\\'))
        || Path::new(&failure.suite_path)
            .components()
            .any(|part| part == Component::ParentDir)
        || failure.test_name != format!("loading {}", failure.suite_path)
    {
        return None;
    }
    let relative = directory.relative(&failure.suite_path).ok()?;
    if Path::new(&relative).extension() != Some("dart".as_ref()) {
        return None;
    }
    let error = &failure.errors[0];
    if error.is_failure
        || !['\'', '"'].iter().any(|quote| {
            error.error
                == format!(
                    "Failed to load {quote}{}{quote}: Null check operator used on a null value",
                    failure.suite_path
                )
        })
        || error.stack_trace.is_empty()
        || error.stack_trace.chars().count() > 65536
    {
        return None;
    }
    let mut lines = error.stack_trace.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.is_empty() || lines.len() > 64 {
        return None;
    }
    let locations = directory.locations(Path::new(&relative), true)?;
    let author = |line: &str| {
        locations.iter().any(|location| {
            line.strip_prefix(location)
                .is_some_and(|tail| AUTHOR_TAIL.is_match(tail))
        })
    };
    if !author(lines[0])
        || lines
            .iter()
            .any(|line| !author(line) && !FRAMEWORK.is_match(line))
    {
        return None;
    }
    let mut budget = MAX_SIGNATURE_BYTES;
    Some((vec![normalized(failure, directory, &mut budget)?], relative))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        Value::String(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64() != Some(0.0),
    }
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;

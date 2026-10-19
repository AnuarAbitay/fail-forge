//! A bounded, source-normalized catalogue from accepted native reporter events.
//! Broad identities preserve the existing selection contract; the stricter
//! structural view is optional and alone can support setup-suppression evidence.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::CatalogError;
use crate::test_outcome::{MAX_CAPTURE_BYTES, MAX_EVENT_BYTES, MAX_EVENTS, ReporterMode};
use crate::test_reporter::{Reporter, parse_event};

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;

const MAX_GRAPH_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogSnapshot {
    pub raw_sha256: String,
    pub cwd: String,
    pub identities: Vec<[String; 2]>,
    pub view: Option<CatalogView>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SuiteKey(pub String, pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Location(pub Option<u64>, pub Option<u64>, pub Option<String>);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupDescriptor {
    pub name: String,
    pub location: Location,
    pub metadata_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupKey {
    pub suite: SuiteKey,
    pub chain: Vec<GroupDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TestKey {
    pub suite: SuiteKey,
    pub name: String,
    pub chain: Vec<GroupDescriptor>,
    pub location: Location,
    pub metadata_json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Group {
    pub key: GroupKey,
    pub test_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Done {
    pub test_id: u64,
    pub result: String,
    pub hidden: bool,
    pub skipped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnostic {
    pub error: String,
    pub stack_trace: String,
    pub is_failure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Test {
    pub key: TestKey,
    pub suite: SuiteKey,
    pub chain: Vec<GroupDescriptor>,
    pub name: String,
    pub location: Location,
    pub done: Done,
    pub errors: Vec<Diagnostic>,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogView {
    pub success: bool,
    pub failed: usize,
    pub suites: Vec<SuiteKey>,
    pub groups: Vec<Group>,
    pub tests: Vec<Test>,
}

struct Rows {
    suites: Vec<(u64, Map<String, Value>)>,
    groups: Vec<(u64, Map<String, Value>)>,
    tests: Vec<(u64, Map<String, Value>)>,
    done: HashMap<u64, Done>,
    errors: HashMap<u64, Vec<Diagnostic>>,
}

impl Rows {
    fn new() -> Self {
        Self {
            suites: Vec::new(),
            groups: Vec::new(),
            tests: Vec::new(),
            done: HashMap::new(),
            errors: HashMap::new(),
        }
    }

    /// Called only after Reporter accepted the complete event. It retains only
    /// catalogue-relevant rows, not arbitrary printing or tool output.
    fn record(&mut self, event: Value) {
        let Value::Object(mut event) = event else {
            return;
        };
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .expect("accepted event kind");
        match kind {
            "suite" | "group" | "testStart" => {
                let field = match kind {
                    "suite" => "suite",
                    "group" => "group",
                    _ => "test",
                };
                let Value::Object(row) = event.remove(field).expect("accepted event row") else {
                    unreachable!("accepted event object")
                };
                let identifier = row["id"].as_u64().expect("accepted event id");
                match field {
                    "suite" => self.suites.push((identifier, row)),
                    "group" => self.groups.push((identifier, row)),
                    _ => self.tests.push((identifier, row)),
                }
            }
            "testDone" => {
                let identifier = event["testID"].as_u64().expect("accepted test id");
                self.done.insert(
                    identifier,
                    Done {
                        test_id: identifier,
                        result: event["result"]
                            .as_str()
                            .expect("accepted test result")
                            .to_owned(),
                        hidden: event["hidden"].as_bool().expect("accepted hidden flag"),
                        skipped: event["skipped"].as_bool().expect("accepted skipped flag"),
                    },
                );
            }
            "error" => {
                let identifier = event["testID"].as_u64().expect("accepted test id");
                self.errors.entry(identifier).or_default().push(Diagnostic {
                    error: event["error"].as_str().expect("accepted error").to_owned(),
                    stack_trace: event["stackTrace"]
                        .as_str()
                        .expect("accepted stack")
                        .to_owned(),
                    is_failure: event["isFailure"].as_bool().expect("accepted failure flag"),
                });
            }
            _ => {}
        }
    }
}

struct Budget(usize);
impl Budget {
    fn new() -> Self {
        Self(MAX_GRAPH_BYTES)
    }
    fn consume(&mut self, bytes: usize) -> Result<(), CatalogError> {
        self.0 = self
            .0
            .checked_sub(bytes)
            .ok_or(CatalogError("catalogue_graph_limit_exceeded"))?;
        Ok(())
    }
}

/// Extract the accepted catalogue once, including complete loader-failed runs.
/// Invalid/incomplete reporter captures cannot supply even broad identities.
/// A structural view is withheld when source normalization is ambiguous or the
/// capture contains loading failures or observed framework timeout diagnostics.
pub fn analyze_catalogue(
    raw: &[u8],
    cwd: &Path,
    mode: ReporterMode,
) -> Result<CatalogSnapshot, CatalogError> {
    if raw.len() > MAX_CAPTURE_BYTES {
        return Err(CatalogError("capture_limit_exceeded"));
    }
    if raw.iter().filter(|byte| **byte == b'\n').count() >= MAX_EVENTS {
        return Err(CatalogError("event_limit_exceeded"));
    }
    let lexical_cwd = absolute(cwd)?;
    // Stack traces may retain the caller's symlink spelling. Preserve that
    // logical absolute cwd for later author-frame matching, while all source
    // containment checks below use the separately resolved filesystem path.
    let logical_cwd = Path::new("/").join(collapse(&lexical_cwd).join("/"));
    let normalized_cwd = resolve_existing(&lexical_cwd)?;
    let mut reporter = Reporter::new(match mode {
        ReporterMode::Dart => "dart",
        ReporterMode::Flutter => "flutter",
    });
    let mut rows = Rows::new();
    for line in raw.split(|byte| *byte == b'\n') {
        if line.iter().all(|byte| matches!(byte, b' ' | b'\t' | b'\r')) {
            continue;
        }
        if line.len() > MAX_EVENT_BYTES {
            return Err(CatalogError("event_limit_exceeded"));
        }
        let event = parse_event(line).map_err(|error| CatalogError(error.0))?;
        reporter
            .consume(&event)
            .map_err(|error| CatalogError(error.0))?;
        rows.record(event);
    }
    if !reporter.finished {
        return Err(CatalogError("missing_done_event"));
    }
    let identities = identities(&rows, &lexical_cwd)?;
    let view = if reporter.counts.loading_failed > 0
        || reporter
            .tests
            .iter()
            .any(|(_, test)| test.timeout_diagnostic_observed)
    {
        None
    } else {
        strict_view(
            &rows,
            &normalized_cwd,
            reporter.success.expect("completed reporter success"),
            reporter.counts.failed,
        )
        .ok()
    };
    Ok(CatalogSnapshot {
        raw_sha256: format!("{:x}", Sha256::digest(raw)),
        cwd: text(&logical_cwd)?.to_owned(),
        identities,
        view,
    })
}

fn identities(rows: &Rows, cwd: &Path) -> Result<Vec<[String; 2]>, CatalogError> {
    let suites: HashMap<_, _> = rows
        .suites
        .iter()
        .map(|(id, row)| {
            let raw = row["path"].as_str().expect("accepted suite path");
            let path = Path::new(raw);
            let normalized = if path.is_absolute() {
                relative_lexical(path, cwd)
            } else {
                lexical_string(path)
            };
            (*id, normalized)
        })
        .collect();
    let mut budget = Budget::new();
    let mut identities = Vec::new();
    for (_, row) in &rows.tests {
        if row["groupIDs"]
            .as_array()
            .expect("accepted group IDs")
            .is_empty()
        {
            continue;
        }
        let suite = &suites[&row["suiteID"].as_u64().expect("accepted suite ID")];
        let name = row["name"].as_str().expect("accepted test name");
        budget.consume(
            std::mem::size_of::<[String; 2]>()
                .saturating_add(suite.len())
                .saturating_add(name.len()),
        )?;
        identities.push([suite.clone(), name.to_owned()]);
    }
    identities.sort();
    Ok(identities)
}

fn strict_view(
    rows: &Rows,
    cwd: &Path,
    success: bool,
    failed: usize,
) -> Result<CatalogView, CatalogError> {
    let mut budget = Budget::new();
    let mut suites = HashMap::new();
    let mut unique_suites = HashSet::new();
    let mut ordered_suites = Vec::new();
    for (id, row) in &rows.suites {
        let suite = SuiteKey(
            relative(row["path"].as_str().expect("accepted suite path"), cwd)?,
            row["platform"]
                .as_str()
                .expect("accepted suite platform")
                .to_owned(),
        );
        budget.consume(suite_size(&suite).saturating_mul(3))?;
        if !unique_suites.insert(suite.clone()) {
            return Err(CatalogError("ambiguous_suite"));
        }
        ordered_suites.push(suite.clone());
        suites.insert(*id, suite);
    }
    let mut groups = Vec::<Group>::new();
    let mut group_indices = HashMap::<u64, usize>::new();
    let mut group_parents = HashMap::new();
    let mut unique_groups = HashSet::new();
    for (id, row) in &rows.groups {
        let metadata = row
            .get("metadata")
            .filter(|value| value.is_object())
            .ok_or(CatalogError("invalid_declared_group"))?;
        let parent = row
            .get("parentID")
            .ok_or(CatalogError("invalid_declared_group"))?;
        let parent = if parent.is_null() {
            None
        } else {
            Some(
                parent
                    .as_u64()
                    .ok_or(CatalogError("invalid_declared_group"))?,
            )
        };
        let suite = &suites[&row["suiteID"].as_u64().expect("accepted suite ID")];
        let descriptor = GroupDescriptor {
            name: row["name"]
                .as_str()
                .expect("accepted group name")
                .to_owned(),
            location: location(row, cwd)?,
            metadata_json: canonical_metadata(metadata),
        };
        let ancestors = match parent {
            Some(parent) => {
                &groups[*group_indices
                    .get(&parent)
                    .ok_or(CatalogError("invalid_declared_group"))?]
                .key
                .chain[..]
            }
            None => &[],
        };
        let size = suite_size(suite)
            .saturating_add(chain_size(ancestors))
            .saturating_add(descriptor_size(&descriptor));
        budget.consume(size.saturating_mul(2))?;
        let mut chain = ancestors.to_vec();
        chain.push(descriptor);
        let key = GroupKey {
            suite: suite.clone(),
            chain,
        };
        if !unique_groups.insert(key.clone()) {
            return Err(CatalogError("ambiguous_group_ancestry"));
        }
        group_indices.insert(*id, groups.len());
        group_parents.insert(*id, parent);
        groups.push(Group {
            key,
            test_count: row["testCount"].as_u64().expect("accepted group count"),
        });
    }
    let mut tests = Vec::new();
    for (id, row) in &rows.tests {
        let ids = row["groupIDs"].as_array().expect("accepted group IDs");
        if ids.is_empty() {
            continue;
        }
        let suite = &suites[&row["suiteID"].as_u64().expect("accepted suite ID")];
        let mut previous = None;
        let mut chain = Vec::new();
        for id in ids {
            let id = id.as_u64().expect("accepted group ID");
            if group_parents[&id] != previous {
                return Err(CatalogError("noncanonical_test_group_chain"));
            }
            previous = Some(id);
            let descriptor = groups[group_indices[&id]]
                .key
                .chain
                .last()
                .expect("nonempty group chain");
            budget.consume(descriptor_size(descriptor).saturating_mul(2))?;
            chain.push(descriptor.clone());
        }
        let location = location(row, cwd)?;
        if location.2.as_ref().is_some_and(|path| path != &suite.0) {
            return Err(CatalogError("test_location_outside_suite"));
        }
        let name = row["name"].as_str().expect("accepted test name");
        let metadata = &row["metadata"];
        let metadata_json = canonical_metadata(metadata);
        let errors = rows.errors.get(id).map(Vec::as_slice).unwrap_or_default();
        let error_size = errors.iter().fold(0usize, |sum, error| {
            sum.saturating_add(std::mem::size_of::<Diagnostic>())
                .saturating_add(error.error.len())
                .saturating_add(error.stack_trace.len())
        });
        budget.consume(
            suite_size(suite)
                .saturating_mul(2)
                .saturating_add(name.len().saturating_mul(2))
                .saturating_add(location_size(&location).saturating_mul(2))
                .saturating_add(metadata_json.len().saturating_mul(2))
                .saturating_add(error_size)
                .saturating_add(std::mem::size_of::<Test>()),
        )?;
        let key = TestKey {
            suite: suite.clone(),
            name: name.to_owned(),
            chain: chain.clone(),
            location: location.clone(),
            metadata_json,
        };
        tests.push(Test {
            key,
            suite: suite.clone(),
            chain,
            name: name.to_owned(),
            location,
            done: rows.done[id].clone(),
            errors: errors.to_vec(),
            metadata: metadata.clone(),
        });
    }
    Ok(CatalogView {
        success,
        failed,
        suites: ordered_suites,
        groups,
        tests,
    })
}

pub(crate) fn canonical_metadata(value: &Value) -> String {
    // serde_json's default Map is a BTreeMap, so nested object keys are sorted.
    // This string is an identity key, not the Python-formatted witness digest.
    serde_json::to_string(value).expect("finite parsed JSON metadata")
}

fn suite_size(suite: &SuiteKey) -> usize {
    std::mem::size_of::<SuiteKey>()
        .saturating_add(suite.0.len())
        .saturating_add(suite.1.len())
}
fn location_size(location: &Location) -> usize {
    std::mem::size_of::<Location>().saturating_add(location.2.as_ref().map_or(0, String::len))
}
fn descriptor_size(descriptor: &GroupDescriptor) -> usize {
    std::mem::size_of::<GroupDescriptor>()
        .saturating_add(descriptor.name.len())
        .saturating_add(location_size(&descriptor.location))
        .saturating_add(descriptor.metadata_json.len())
}
fn chain_size(chain: &[GroupDescriptor]) -> usize {
    chain
        .iter()
        .fold(0usize, |sum, row| sum.saturating_add(descriptor_size(row)))
}

fn location(row: &Map<String, Value>, cwd: &Path) -> Result<Location, CatalogError> {
    let absent = |field| row.get(field).is_none_or(Value::is_null);
    if absent("line") && absent("column") && absent("url") {
        return Ok(Location(None, None, None));
    }
    let positive = |field| {
        row.get(field)
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or(CatalogError("invalid_source_location"))
    };
    let line = positive("line")?;
    let column = positive("column")?;
    let url = row
        .get("url")
        .and_then(Value::as_str)
        .ok_or(CatalogError("invalid_source_location"))?;
    Ok(Location(
        Some(line),
        Some(column),
        Some(relative(url, cwd)?),
    ))
}

fn text(path: &Path) -> Result<&str, CatalogError> {
    path.to_str().ok_or(CatalogError("non_utf8_author_path"))
}

fn absolute(path: &Path) -> Result<PathBuf, CatalogError> {
    if text(path)?.contains('\0') {
        return Err(CatalogError("invalid_author_path"));
    }
    Ok(if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| CatalogError("invalid_author_cwd"))?
            .join(path)
    })
}

/// Resolve every existing prefix while preserving missing components. This
/// mirrors Path.resolve(strict=False) and remains usable after baseline cleanup.
pub(crate) fn resolve_existing(path: &Path) -> Result<PathBuf, CatalogError> {
    let mut pending: VecDeque<_> = absolute(path)?
        .components()
        .map(|component| component.as_os_str().to_owned())
        .collect();
    let mut output = PathBuf::new();
    let mut links = 0;
    while let Some(segment) = pending.pop_front() {
        match Path::new(&segment).components().next() {
            Some(Component::RootDir) => {
                output = PathBuf::from("/");
            }
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                output.pop();
            }
            Some(Component::Normal(_)) => {
                output.push(&segment);
                match std::fs::symlink_metadata(&output) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        links += 1;
                        if links > 40 {
                            return Err(CatalogError("author_path_symlink_loop"));
                        }
                        let target = std::fs::read_link(&output)
                            .map_err(|_| CatalogError("invalid_author_path"))?;
                        output.pop();
                        for component in target.components().rev() {
                            pending.push_front(component.as_os_str().to_owned());
                        }
                    }
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        ) => {}
                    Err(_) => return Err(CatalogError("invalid_author_path")),
                }
            }
            _ => return Err(CatalogError("invalid_author_path")),
        }
    }
    text(&output)?;
    Ok(output)
}

fn relative(value: &str, cwd: &Path) -> Result<String, CatalogError> {
    if value.is_empty() || value.contains('\0') {
        return Err(CatalogError("invalid_author_path"));
    }
    let decoded = if let Some(uri) = value.strip_prefix("file:") {
        local_uri(uri)?
    } else {
        value.to_owned()
    };
    if decoded.contains('\0') {
        return Err(CatalogError("invalid_author_path"));
    }
    let path = Path::new(&decoded);
    let resolved = resolve_existing(&if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    })?;
    let relative = resolved
        .strip_prefix(cwd)
        .map_err(|_| CatalogError("non_author_path"))?;
    if relative.components().next() != Some(Component::Normal("test".as_ref()))
        || relative.extension() != Some("dart".as_ref())
    {
        return Err(CatalogError("non_author_path"));
    }
    Ok(text(relative)?.to_owned())
}

fn local_uri(uri: &str) -> Result<String, CatalogError> {
    let uri = uri.split(['?', '#']).next().unwrap_or_default();
    let path = if let Some(authority) = uri.strip_prefix("//") {
        let (host, path) = authority.split_once('/').unwrap_or((authority, ""));
        if !matches!(host, "" | "localhost") {
            return Err(CatalogError("non_local_author_path"));
        }
        format!("/{path}")
    } else {
        uri.to_owned()
    };
    let mut decoded = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = bytes
                .get(index + 1)
                .and_then(|value| (*value as char).to_digit(16));
            let low = bytes
                .get(index + 2)
                .and_then(|value| (*value as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(CatalogError("invalid_author_uri"));
            };
            decoded.push((high * 16 + low) as u8);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| CatalogError("invalid_author_uri"))
}

pub(crate) fn path_uri(path: &Path) -> Result<String, CatalogError> {
    if !path.is_absolute() {
        return Err(CatalogError("invalid_author_path"));
    }
    let mut uri = String::from("file://");
    for byte in text(path)?.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(uri, "%{byte:02X}").expect("String formatting");
        }
    }
    Ok(uri)
}

fn lexical_string(path: &Path) -> String {
    let value = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            Component::ParentDir => Some("..".to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/");
    if value.is_empty() {
        ".".to_owned()
    } else {
        value
    }
}

fn collapse(path: &Path) -> Vec<String> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => components.push(value.to_string_lossy().into_owned()),
            Component::ParentDir => {
                components.pop();
            }
            _ => {}
        }
    }
    components
}

fn relative_lexical(path: &Path, cwd: &Path) -> String {
    let target = collapse(path);
    let base = collapse(cwd);
    let common = target
        .iter()
        .zip(&base)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = vec!["..".to_owned(); base.len() - common];
    relative.extend_from_slice(&target[common..]);
    if relative.is_empty() {
        ".".to_owned()
    } else {
        relative.join("/")
    }
}

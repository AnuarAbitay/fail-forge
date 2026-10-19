//! Validation of package:test JSON reporter events. Synthetic loading tests are
//! tracked separately from authored tests, and rejected events never establish a
//! test verdict. Callers taking progress snapshots commit only accepted events:
//! validation intentionally preserves the legacy partial-state error semantics.

mod strict_json;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};

pub(crate) use strict_json::{parse_event, parse_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EvidenceError(pub(crate) &'static str, bool);

impl EvidenceError {
    fn invalid(reason: &'static str) -> Self {
        Self(reason, false)
    }

    pub(crate) fn is_interrupted_candidate(self) -> bool {
        self.1
    }
}

type Result<T> = std::result::Result<T, EvidenceError>;

// A short capture can otherwise expand into a large retained graph when every
// test repeats one long suite path. Include copied strings and conservative
// container overhead before allocating; caller framing limits alone do not
// bound this amplification.
const MAX_RETAINED_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
struct RetainedBudget {
    used: usize,
    maximum: usize,
}

impl RetainedBudget {
    fn reserve(&mut self, bytes: usize) -> Result<()> {
        let used = self
            .used
            .checked_add(bytes)
            .filter(|used| *used <= self.maximum)
            .ok_or(EvidenceError::invalid("reporter_memory_limit_exceeded"))?;
        self.used = used;
        Ok(())
    }
}

fn diagnostic_bytes(error: &Diagnostic) -> usize {
    std::mem::size_of::<Diagnostic>()
        .saturating_mul(2)
        .saturating_add(error.error.len())
        .saturating_add(error.stack_trace.len())
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Counts {
    pub(crate) started: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
    pub(crate) hidden: usize,
    pub(crate) loading_passed: usize,
    pub(crate) loading_failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Diagnostic {
    pub(crate) error: String,
    pub(crate) stack_trace: String,
    pub(crate) is_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LoadFailure {
    pub(crate) suite_path: String,
    pub(crate) test_name: String,
    pub(crate) errors: Vec<Diagnostic>,
    pub(crate) kind: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct TestState {
    pub(crate) loading: bool,
    pub(crate) done: bool,
    pub(crate) suite_path: String,
    pub(crate) test_name: String,
    pub(crate) result: Option<String>,
    pub(crate) timeout_diagnostic_observed: bool,
    errors: Vec<Diagnostic>,
    started_at: u64,
}

#[derive(Debug)]
pub(crate) struct Reporter {
    pub(crate) counts: Counts,
    pub(crate) started: bool,
    pub(crate) finished: bool,
    pub(crate) success: Option<bool>,
    pub(crate) all_suites: Option<u64>,
    pub(crate) last_time: u64,
    // Preserve reporter insertion order for bounded timeout diagnostics while
    // using the index for constant-time lookups across a long capture.
    pub(crate) tests: Vec<(u64, TestState)>,
    pub(crate) compile_load_failures: usize,
    pub(crate) other_load_failures: usize,
    pub(crate) load_failures: Vec<LoadFailure>,
    flutter: bool,
    ids: HashSet<u64>,
    suites: HashMap<u64, String>,
    groups: HashMap<u64, u64>,
    test_indices: HashMap<u64, usize>,
    memory: RetainedBudget,
}

fn require(condition: bool, reason: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(EvidenceError::invalid(reason))
    }
}

fn integer(value: Option<&Value>, reason: &'static str) -> Result<u64> {
    value
        .and_then(Value::as_u64)
        .ok_or(EvidenceError::invalid(reason))
}

fn string<'a>(value: Option<&'a Value>, reason: &'static str) -> Result<&'a str> {
    value
        .and_then(Value::as_str)
        .ok_or(EvidenceError::invalid(reason))
}

fn object<'a>(value: Option<&'a Value>, reason: &'static str) -> Result<&'a Map<String, Value>> {
    value
        .and_then(Value::as_object)
        .ok_or(EvidenceError::invalid(reason))
}

impl Reporter {
    pub(crate) fn new(mode: &str) -> Self {
        Self {
            counts: Counts::default(),
            started: false,
            finished: false,
            success: None,
            all_suites: None,
            last_time: 0,
            tests: Vec::new(),
            compile_load_failures: 0,
            other_load_failures: 0,
            load_failures: Vec::new(),
            flutter: mode == "flutter",
            ids: HashSet::new(),
            suites: HashMap::new(),
            groups: HashMap::new(),
            test_indices: HashMap::new(),
            memory: RetainedBudget {
                used: 0,
                maximum: MAX_RETAINED_BYTES,
            },
        }
    }

    pub(crate) fn test(&self, identifier: u64) -> Option<&TestState> {
        self.test_indices
            .get(&identifier)
            .map(|index| &self.tests[*index].1)
    }

    fn reserve(&mut self, identifier: Option<&Value>) -> Result<u64> {
        let identifier = integer(identifier, "invalid_event_id")?;
        require(!self.ids.contains(&identifier), "duplicate_event_id")?;
        self.memory.reserve(std::mem::size_of::<u64>() * 4)?;
        self.ids.insert(identifier);
        Ok(identifier)
    }

    fn flutter_tool_event(&self, event: &[Value]) -> Result<()> {
        require(!self.finished, "events_after_done")?;
        require(self.started, "missing_start_event")?;
        require(event.len() == 1, "invalid_flutter_tool_event")?;
        let metadata = object(event.first(), "invalid_flutter_tool_event")?;
        require(
            metadata.len() == 2
                && metadata.contains_key("event")
                && metadata.contains_key("params"),
            "invalid_flutter_tool_event",
        )?;
        require(
            metadata["event"] == "test.startedProcess",
            "unknown_flutter_tool_event",
        )?;
        let params = object(metadata.get("params"), "invalid_flutter_tool_event")?;
        require(
            params.len() == 1
                && params
                    .get("vmServiceUri")
                    .is_some_and(|uri| uri.is_null() || uri.is_string()),
            "invalid_flutter_tool_event",
        )
    }

    pub(crate) fn consume(&mut self, event: &Value) -> Result<()> {
        if let Some(event) = event.as_array().filter(|_| self.flutter) {
            return self.flutter_tool_event(event);
        }
        let event = object(Some(event), "non_object_event")?;
        let kind = string(event.get("type"), "missing_event_type")?;
        require(!self.finished, "events_after_done")?;
        let timestamp = integer(event.get("time"), "invalid_event_time")?;
        require(timestamp >= self.last_time, "invalid_event_time")?;
        self.last_time = timestamp;
        require(self.started || kind == "start", "missing_start_event")?;
        match kind {
            "start" => {
                require(!self.started, "duplicate_start_event")?;
                require(
                    event
                        .get("protocolVersion")
                        .is_some_and(|version| version == "0.1.1"),
                    "unsupported_reporter_protocol",
                )?;
                require(
                    integer(event.get("pid"), "invalid_reporter_pid")? > 0,
                    "invalid_reporter_pid",
                )?;
                require(
                    event.get("runnerVersion").is_some_and(|version| {
                        version.is_string() || self.flutter && version.is_null()
                    }),
                    "invalid_runner_version",
                )?;
                self.started = true;
            }
            "suite" => {
                let suite = object(event.get("suite"), "invalid_suite_event")?;
                let identifier = self.reserve(suite.get("id"))?;
                string(suite.get("platform"), "invalid_suite_metadata")?;
                let path = string(suite.get("path"), "invalid_suite_metadata")?;
                self.memory.reserve(
                    path.len()
                        .saturating_add(std::mem::size_of::<(u64, String)>() * 2),
                )?;
                self.suites.insert(identifier, path.to_owned());
            }
            "group" => {
                let group = object(event.get("group"), "invalid_group_event")?;
                let identifier = self.reserve(group.get("id"))?;
                let suite = integer(group.get("suiteID"), "unknown_group_suite")?;
                require(self.suites.contains_key(&suite), "unknown_group_suite")?;
                if let Some(parent) = group.get("parentID").filter(|parent| !parent.is_null()) {
                    let parent = integer(Some(parent), "unknown_parent_group")?;
                    require(
                        self.groups.get(&parent) == Some(&suite),
                        "unknown_parent_group",
                    )?;
                }
                string(group.get("name"), "invalid_group_metadata")?;
                integer(group.get("testCount"), "invalid_group_metadata")?;
                self.memory.reserve(std::mem::size_of::<(u64, u64)>() * 2)?;
                self.groups.insert(identifier, suite);
            }
            "allSuites" => {
                require(self.all_suites.is_none(), "duplicate_all_suites_event")?;
                self.all_suites = Some(integer(event.get("count"), "invalid_suite_count")?);
            }
            "testStart" => self.start_test(event, timestamp)?,
            "error" | "print" => self.test_event(event, kind, timestamp)?,
            "testDone" => self.finish_test(event)?,
            "done" => {
                let success = event
                    .get("success")
                    .and_then(Value::as_bool)
                    .ok_or(EvidenceError::invalid("incomplete_reporter_done"))?;
                let count = self
                    .all_suites
                    .ok_or(EvidenceError::invalid("missing_all_suites_event"))?;
                require(count == self.suites.len() as u64, "suite_count_mismatch")?;
                require(
                    self.tests.iter().all(|(_, test)| test.done),
                    "unfinished_tests",
                )?;
                self.success = Some(success);
                require(
                    success != (self.counts.failed > 0 || self.counts.loading_failed > 0),
                    "contradictory_done_success",
                )?;
                self.finished = true;
            }
            "debug" => {
                let suite = integer(event.get("suiteID"), "unknown_debug_suite")?;
                require(self.suites.contains_key(&suite), "unknown_debug_suite")?;
            }
            _ => return Err(EvidenceError::invalid("unknown_event_type")),
        }
        Ok(())
    }

    fn start_test(&mut self, event: &Map<String, Value>, timestamp: u64) -> Result<()> {
        let test = object(event.get("test"), "invalid_test_start")?;
        let identifier = self.reserve(test.get("id"))?;
        let suite = integer(test.get("suiteID"), "unknown_test_suite")?;
        let suite_path = self
            .suites
            .get(&suite)
            .ok_or(EvidenceError::invalid("unknown_test_suite"))?;
        let name = string(test.get("name"), "invalid_test_metadata")?;
        let groups = test
            .get("groupIDs")
            .and_then(Value::as_array)
            .ok_or(EvidenceError::invalid("invalid_test_metadata"))?;
        let mut unique_groups = HashSet::with_capacity(groups.len());
        for group in groups {
            let group = integer(Some(group), "unknown_test_group")?;
            require(
                self.groups.get(&group) == Some(&suite) && unique_groups.insert(group),
                "unknown_test_group",
            )?;
        }
        let loading = name.starts_with("loading ") && groups.is_empty();
        require(loading || !groups.is_empty(), "unknown_synthetic_test")?;
        let metadata = object(test.get("metadata"), "invalid_test_metadata")?;
        require(
            metadata.get("skip").is_some_and(Value::is_boolean),
            "invalid_test_metadata",
        )?;
        self.memory.reserve(
            suite_path
                .len()
                .saturating_add(name.len())
                .saturating_add(std::mem::size_of::<(u64, TestState)>() * 2)
                .saturating_add(std::mem::size_of::<(u64, usize)>() * 2),
        )?;
        self.test_indices.insert(identifier, self.tests.len());
        self.tests.push((
            identifier,
            TestState {
                loading,
                done: false,
                suite_path: suite_path.clone(),
                test_name: name.to_owned(),
                result: None,
                timeout_diagnostic_observed: false,
                errors: Vec::new(),
                started_at: timestamp,
            },
        ));
        if !loading {
            self.counts.started += 1;
        }
        Ok(())
    }

    fn test_event(&mut self, event: &Map<String, Value>, kind: &str, timestamp: u64) -> Result<()> {
        let identifier = integer(event.get("testID"), "unknown_event_test")?;
        let index = *self
            .test_indices
            .get(&identifier)
            .ok_or(EvidenceError::invalid("unknown_event_test"))?;
        let test = &mut self.tests[index].1;
        require(!test.done, "event_after_test_done")?;
        if kind == "error" {
            let error = string(event.get("error"), "invalid_error_event")?;
            let stack_trace = string(event.get("stackTrace"), "invalid_error_event")?;
            let is_failure = event
                .get("isFailure")
                .and_then(Value::as_bool)
                .ok_or(EvidenceError::invalid("invalid_error_event"))?;
            self.memory.reserve(
                error
                    .len()
                    .saturating_add(stack_trace.len())
                    .saturating_add(std::mem::size_of::<Diagnostic>() * 2),
            )?;
            let diagnostic = Diagnostic {
                error: error.to_owned(),
                stack_trace: stack_trace.to_owned(),
                is_failure,
            };
            if !test.loading && test_timeout_diagnostic(&diagnostic, test.started_at, timestamp) {
                test.timeout_diagnostic_observed = true;
            }
            test.errors.push(diagnostic);
        } else {
            string(event.get("message"), "invalid_print_event")?;
            string(event.get("messageType"), "invalid_print_event")?;
        }
        Ok(())
    }

    fn finish_test(&mut self, event: &Map<String, Value>) -> Result<()> {
        let identifier = integer(event.get("testID"), "unknown_completed_test")?;
        let index = *self
            .test_indices
            .get(&identifier)
            .ok_or(EvidenceError::invalid("unknown_completed_test"))?;
        let test = &mut self.tests[index].1;
        require(!test.done, "duplicate_test_done")?;
        let result = string(event.get("result"), "invalid_test_done")?;
        require(
            matches!(result, "success" | "failure" | "error"),
            "invalid_test_done",
        )?;
        let skipped = event
            .get("skipped")
            .and_then(Value::as_bool)
            .ok_or(EvidenceError::invalid("invalid_test_done"))?;
        let hidden = event
            .get("hidden")
            .and_then(Value::as_bool)
            .ok_or(EvidenceError::invalid("invalid_test_done"))?;
        require(
            !skipped || result == "success" && test.errors.is_empty(),
            "contradictory_skipped_result",
        )?;
        require(
            result != "success" || test.errors.is_empty(),
            "successful_test_has_errors",
        )?;
        require(
            result == "success" || !test.errors.is_empty(),
            "missing_failure_evidence",
        )?;
        self.memory.reserve(result.len())?;
        test.done = true;
        test.result = Some(result.to_owned());
        if test.loading {
            require(!skipped, "skipped_loading_test")?;
            if result == "success" {
                self.counts.loading_passed += 1;
            } else {
                let duplicated = test
                    .errors
                    .iter()
                    .fold(0usize, |total, error| {
                        total.saturating_add(diagnostic_bytes(error))
                    })
                    .saturating_add(test.suite_path.len())
                    .saturating_add(test.test_name.len())
                    .saturating_add(std::mem::size_of::<LoadFailure>() * 2);
                self.memory.reserve(duplicated)?;
                self.counts.loading_failed += 1;
                let compiler = test.errors.iter().any(|error| {
                    error.error.starts_with("Failed to load ")
                        && compiler_diagnostic().is_match(&error.error)
                });
                self.load_failures.push(LoadFailure {
                    suite_path: test.suite_path.clone(),
                    test_name: test.test_name.clone(),
                    errors: test.errors.clone(),
                    kind: if compiler { "compiler" } else { "unknown" },
                });
                if compiler {
                    self.compile_load_failures += 1;
                } else {
                    self.other_load_failures += 1;
                }
            }
        } else if skipped {
            self.counts.skipped += 1;
        } else if result != "success" {
            self.counts.failed += 1;
        } else if hidden {
            self.counts.hidden += 1;
        } else {
            self.counts.passed += 1;
        }
        Ok(())
    }
}

fn compiler_diagnostic() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"(?:^|\n)[^\n]+\.dart:\d+:\d+: Error: ")
            .expect("constant compiler diagnostic pattern")
    })
}

fn timeout_duration() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"\ATimeoutException after (0|[1-9][0-9]{0,9}):([0-5][0-9]):([0-5][0-9])\.([0-9]{6}): ",
        )
        .expect("constant timeout duration pattern")
    })
}

fn timeout_stack() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\Adart:isolate[ \t]+_RawReceivePort\._handleMessage[ \t]*(?:\r?\n)?\z")
            .expect("constant timeout stack pattern")
    })
}

/// This recognizes a reporter diagnostic's shape and elapsed duration only;
/// user code can reproduce the text, so it does not authenticate timeout origin.
fn test_timeout_diagnostic(error: &Diagnostic, started_at: u64, observed_at: u64) -> bool {
    if error.is_failure || !timeout_stack().is_match(&error.stack_trace) {
        return false;
    }
    let Some(parts) = timeout_duration().captures(&error.error) else {
        return false;
    };
    let numbers: Vec<u64> = parts
        .iter()
        .skip(1)
        .map(|part| {
            part.expect("matched duration group")
                .as_str()
                .parse()
                .expect("bounded ASCII duration integer")
        })
        .collect();
    let duration = ((u128::from(numbers[0]) * 60 + u128::from(numbers[1])) * 60
        + u128::from(numbers[2]))
        * 1_000_000
        + u128::from(numbers[3]);
    if duration == 0
        || duration > i64::MAX as u128
        || observed_at < started_at
        || u128::from(observed_at - started_at) * 1000 < duration
    {
        return false;
    }
    let whole_minutes = duration / 60_000_000;
    let whole_seconds = duration % 60_000_000 / 1_000_000;
    let deciseconds = duration % 1_000_000 / 100_000;
    let mut nice = if whole_minutes == 0 {
        String::new()
    } else {
        format!("{whole_minutes} minutes")
    };
    if whole_minutes == 0 || whole_seconds != 0 {
        if whole_minutes != 0 {
            nice.push_str(", ");
        }
        nice.push_str(&whole_seconds.to_string());
        if deciseconds != 0 {
            nice.push_str(&format!(".{deciseconds}"));
        }
        nice.push_str(" seconds");
    }
    let mut expected = format!("{}Test timed out after {nice}.", &parts[0]);
    if duration == 30_000_000 {
        expected.push_str(" See https://pub.dev/packages/test#timeouts");
    }
    error.error == expected
}

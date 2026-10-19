use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write;
use std::path::{Component, Path};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use super::view::{
    CatalogSnapshot, CatalogView, Diagnostic, GroupDescriptor, GroupKey, Location, SuiteKey, Test,
    TestKey, canonical_metadata, path_uri,
};
use super::{CatalogError, SelectionContext};

#[cfg(test)]
#[path = "comparison_tests.rs"]
mod tests;

const MAX_AUTHOR_MATCHERS: usize = 1024;
const MAX_AUTHOR_PATTERN_BYTES: usize = 32 * 1024;

fn invalid() -> CatalogError {
    CatalogError("invalid catalogue snapshot")
}
fn author_path(path: &str) -> bool {
    !path.contains('\0')
        && path.ends_with(".dart")
        && path.starts_with("test/")
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && !path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}
fn valid_location(location: &Location) -> bool {
    matches!(location, Location(None, None, None))
        || matches!(location,
        Location(Some(line),Some(column),Some(path)) if *line>0 && *column>0 && author_path(path))
}
fn metadata_valid(text: &str) -> bool {
    crate::test_reporter::parse_json(text.as_bytes(), super::MAX_CONTEXT_BYTES)
        .ok()
        .is_some_and(|value| value.is_object() && canonical_metadata(&value) == text)
}

/// Validate descriptors without touching the original workspace, which may have
/// been removed after a passing baseline. Private transport does not excuse
/// contradictory keys, metadata, counters or source relationships.
pub(super) fn validate_snapshot(snapshot: &CatalogSnapshot) -> Result<(), CatalogError> {
    if snapshot.raw_sha256.len() != 64
        || !snapshot
            .raw_sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || snapshot.cwd.contains('\0')
        || !Path::new(&snapshot.cwd).is_absolute()
        || Path::new(&snapshot.cwd)
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        || snapshot.identities.windows(2).any(|pair| pair[0] > pair[1])
    {
        return Err(invalid());
    }
    let Some(view) = &snapshot.view else {
        return Ok(());
    };
    let suites: HashSet<_> = view.suites.iter().collect();
    if suites.len() != view.suites.len() || suites.iter().any(|suite| !author_path(&suite.0)) {
        return Err(invalid());
    }
    let mut groups = HashSet::new();
    // The producer emits parents before children. This also rules out malformed
    // ancestry without repeated scans of the complete group list.
    for group in &view.groups {
        if !suites.contains(&group.key.suite) || group.key.chain.is_empty() {
            return Err(invalid());
        }
        let last = group.key.chain.last().expect("nonempty chain");
        if !valid_location(&last.location) || !metadata_valid(&last.metadata_json) {
            return Err(invalid());
        }
        let parent = (
            &group.key.suite,
            &group.key.chain[..group.key.chain.len() - 1],
        );
        if group.key.chain.len() > 1 && !groups.contains(&parent) {
            return Err(invalid());
        }
        if !groups.insert((&group.key.suite, group.key.chain.as_slice())) {
            return Err(invalid());
        }
    }
    let mut ids = HashSet::new();
    let mut failed = 0usize;
    for test in &view.tests {
        if !ids.insert(test.done.test_id)
            || !suites.contains(&test.suite)
            || test.suite != test.key.suite
            || test.name != test.key.name
            || test.chain != test.key.chain
            || test.location != test.key.location
            || !valid_location(&test.location)
            || test
                .location
                .2
                .as_ref()
                .is_some_and(|path| *path != test.suite.0)
            || !groups.contains(&(&test.suite, test.chain.as_slice()))
            || !test.metadata.is_object()
            || test.metadata.get("skip").and_then(Value::as_bool).is_none()
            || canonical_metadata(&test.metadata) != test.key.metadata_json
            || !matches!(test.done.result.as_str(), "success" | "failure" | "error")
            || (test.done.result == "success") != test.errors.is_empty()
            || test.done.skipped && test.done.result != "success"
        {
            return Err(invalid());
        }
        failed += usize::from(test.done.result != "success");
    }
    if view.failed != failed || view.success != (failed == 0) {
        return Err(invalid());
    }
    Ok(())
}

fn counter<'a, K: Eq + std::hash::Hash + 'a>(keys: impl Iterator<Item = K>) -> HashMap<K, usize> {
    let mut counts = HashMap::new();
    for key in keys {
        *counts.entry(key).or_default() += 1;
    }
    counts
}
fn identities(view: &CatalogView) -> HashMap<(&str, &str), usize> {
    counter(
        view.tests
            .iter()
            .map(|test| (test.suite.0.as_str(), test.name.as_str())),
    )
}
fn pairs(rows: &[[String; 2]]) -> HashMap<(&str, &str), usize> {
    counter(rows.iter().map(|row| (row[0].as_str(), row[1].as_str())))
}
fn hook(test: &Test, name: &str) -> bool {
    test.done.hidden
        && (test.name == format!("({name})") || test.name.ends_with(&format!(" ({name})")))
}
fn same_group(test: &Test, group: &GroupKey) -> bool {
    test.suite == group.suite && test.chain == group.chain
}
fn is_prefix(suite: &SuiteKey, chain: &[GroupDescriptor], group: &GroupKey) -> bool {
    *suite == group.suite && chain.starts_with(&group.chain)
}

/// Disjoint subtrees support logarithmic ancestor lookup. Nested candidates are
/// either rejected (suppression) or reduced to their outer ancestor (witness).
struct GroupIndex {
    groups: Vec<GroupKey>,
}
impl GroupIndex {
    fn new(mut groups: Vec<GroupKey>, reject_nested: bool) -> Option<Self> {
        groups.sort();
        groups.dedup();
        let mut kept = Vec::<GroupKey>::new();
        for group in groups {
            if kept
                .last()
                .is_some_and(|parent| is_prefix(&group.suite, &group.chain, parent))
            {
                if reject_nested {
                    return None;
                } else {
                    continue;
                }
            }
            kept.push(group);
        }
        Some(Self { groups: kept })
    }
    fn containing(&self, suite: &SuiteKey, chain: &[GroupDescriptor]) -> Option<usize> {
        let end = self
            .groups
            .partition_point(|group| (&group.suite, group.chain.as_slice()) <= (suite, chain));
        end.checked_sub(1)
            .filter(|index| is_prefix(suite, chain, &self.groups[*index]))
    }
    fn containing_test(&self, test: &Test) -> Option<usize> {
        self.containing(&test.suite, &test.chain)
    }
}

fn valid_subject(mutant: &Value, alias: &str) -> bool {
    let Some(file) = mutant.get("file").and_then(Value::as_str) else {
        return false;
    };
    let mut chars = alias.bytes();
    matches!(chars.next(), Some(b'a'..=b'z'))
        && alias.len() <= 1024
        && chars.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && file.starts_with("lib/")
        && file.len() > 9
        && file.ends_with(".dart")
        && !file.contains(['\\', '\0'])
        && !file.split('/').any(|part| part == "..")
}

struct ErrorMatcher {
    runtime: Regex,
    subject: Regex,
    authors: HashMap<String, Regex>,
    cwd: String,
}
impl ErrorMatcher {
    fn new(alias: &str, cwd: &str) -> Option<Self> {
        let alias = regex::escape(alias);
        let runtime=Regex::new(&format!(r"^(?:(?:RangeError|StateError|TypeError|NoSuchMethodError|ArgumentError|UnsupportedError|UnimplementedError|AssertionError|FormatException|Exception)(?:\b|\()|Bad state:|Invalid argument(?:s)?[ :(]|Null check operator used on a null value|Error on line [1-9]\d*, column [1-9]\d*:|type '[^'\n]+' is not a subtype of type |'package:{alias}/[^'\n]+': Failed assertion: line \d+ pos \d+:)")).ok()?;
        let subject = Regex::new(&format!(
            r"(?m)(?:^|\s)package:{alias}/[^\s:]+\.dart(?::|\s)(\d+):(\d+)(?:\s|$)"
        ))
        .ok()?;
        Some(Self {
            runtime,
            subject,
            authors: HashMap::new(),
            cwd: cwd.to_owned(),
        })
    }
    fn prepare_author(&mut self, suite: &str) -> Option<()> {
        if self.authors.contains_key(suite) {
            return Some(());
        }
        if self.authors.len() >= MAX_AUTHOR_MATCHERS {
            return None;
        }
        let absolute = Path::new(&self.cwd).join(suite);
        let locations = [
            suite.to_owned(),
            absolute.to_str()?.to_owned(),
            path_uri(&absolute).ok()?,
        ];
        let mut pattern = String::from(r"(?m)(?:^|\s)(?:");
        for (index, path) in locations.iter().enumerate() {
            if index > 0 {
                pattern.push('|');
            }
            pattern.push_str(&regex::escape(path));
            if pattern.len() > MAX_AUTHOR_PATTERN_BYTES {
                return None;
            }
        }
        pattern.push_str(r")(?::|\s)(\d+):(\d+)(?:\s|$)");
        self.authors
            .insert(suite.to_owned(), Regex::new(&pattern).ok()?);
        Some(())
    }
    fn runtime_error(&mut self, test: &Test, errors: &[Diagnostic]) -> Option<bool> {
        if !matches!(test.done.result.as_str(), "failure" | "error")
            || test.done.hidden
            || test.done.skipped
            || errors.is_empty()
            || test.location.0.is_none()
        {
            return Some(false);
        }
        self.prepare_author(&test.suite.0)?;
        let author = &self.authors[&test.suite.0];
        for error in errors {
            let subject = self.subject.find(&error.stack_trace);
            let caller =
                subject.and_then(|frame| author.captures_at(&error.stack_trace, frame.end()));
            if error.is_failure
                || !self.runtime.is_match(&error.error)
                || !caller.is_some_and(|frame| {
                    at_or_after(&frame[1], test.location.0.expect("source line"))
                })
            {
                return Some(false);
            }
        }
        Some(true)
    }
    fn author_frame(&mut self, test: &Test, error: &Diagnostic) -> Option<bool> {
        self.prepare_author(&test.suite.0)?;
        Some(
            self.authors[&test.suite.0]
                .captures(&error.stack_trace)
                .is_some_and(|frame| at_or_after(&frame[1], test.location.0.expect("source line"))),
        )
    }
}

fn at_or_after(number: &str, minimum: u64) -> bool {
    // Dart writes ASCII decimal frame coordinates. Reject textual lookalikes and
    // enormous attacker-supplied coordinates rather than accepting loose text.
    if number.len() > 20 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    number.parse::<u64>().unwrap_or(u64::MAX) >= minimum
}

struct Suppression<'a> {
    report: Value,
    before: &'a CatalogView,
    after: &'a CatalogView,
    before_counts: HashMap<&'a TestKey, usize>,
    after_counts: HashMap<&'a TestKey, usize>,
}

fn suppression<'a>(
    current: &'a CatalogSnapshot,
    context: &'a SelectionContext,
) -> Option<Suppression<'a>> {
    let baseline = context.baseline.as_ref()?;
    let expected = context.expected.as_ref()?;
    if !valid_subject(context.mutant.as_ref()?, &context.alias) {
        return None;
    }
    validate_snapshot(baseline).ok()?;
    validate_snapshot(current).ok()?;
    let (before, after) = (baseline.view.as_ref()?, current.view.as_ref()?);
    if !before.success
        || after.success
        || after.failed == 0
        || before.suites.iter().collect::<BTreeSet<_>>() != after.suites.iter().collect()
        || identities(before) != pairs(expected)
        || identities(after) != pairs(&current.identities)
    {
        return None;
    }
    let before_counts = counter(before.tests.iter().map(|test| &test.key));
    let after_counts = counter(after.tests.iter().map(|test| &test.key));
    if after_counts
        .iter()
        .any(|(key, count)| *count > *before_counts.get(key).unwrap_or(&0))
    {
        return None;
    }
    let missing: HashMap<_, _> = before_counts
        .iter()
        .filter_map(|(key, count)| {
            count
                .checked_sub(*after_counts.get(key).unwrap_or(&0))
                .filter(|remaining| *remaining > 0)
                .map(|remaining| (*key, remaining))
        })
        .collect();
    if missing.is_empty() {
        return None;
    }
    let actual: HashMap<_, _> = after.tests.iter().map(|test| (&test.key, test)).collect();
    let mut matcher = ErrorMatcher::new(&context.alias, &current.cwd)?;
    let mut failed = Vec::new();
    for test in &before.tests {
        if hook(test, "setUpAll")
            && test.done.result == "success"
            && test.metadata["skip"] == false
            && before_counts[&test.key] == 1
        {
            if let Some(current) = actual.get(&test.key) {
                if matcher.runtime_error(current, &current.errors)? {
                    failed.push(GroupKey {
                        suite: test.suite.clone(),
                        chain: test.chain.clone(),
                    });
                }
            }
        }
    }
    if failed.is_empty() {
        return None;
    }
    let index = GroupIndex::new(failed.clone(), true)?;
    let before_groups: HashMap<_, _> = before
        .groups
        .iter()
        .map(|group| (&group.key, group.test_count))
        .collect();
    let after_groups: HashMap<_, _> = after
        .groups
        .iter()
        .map(|group| (&group.key, group.test_count))
        .collect();
    for group in &after.groups {
        if before_groups.get(&group.key) != Some(&group.test_count) {
            return None;
        }
        if let Some(owner) = index.containing(&group.key.suite, &group.key.chain) {
            if group.key != index.groups[owner] {
                return None;
            }
        }
    }
    let mut declared = vec![0u64; index.groups.len()];
    let mut own_hooks = HashSet::new();
    let mut outside_before = HashMap::new();
    for test in &before.tests {
        if let Some(owner) = index.containing_test(test) {
            declared[owner] += u64::from(!test.done.hidden);
            let group = &index.groups[owner];
            if same_group(test, group) {
                if hook(test, "setUpAll") || hook(test, "tearDownAll") {
                    own_hooks.insert(&test.key);
                }
                if hook(test, "tearDownAll") {
                    let observed = actual.get(&test.key)?;
                    if !observed.done.hidden || observed.done.result != "success" {
                        return None;
                    }
                }
            }
            if missing.contains_key(&test.key) && test.done.hidden && same_group(test, group) {
                return None;
            }
        } else {
            *outside_before
                .entry((&test.key, test.done.hidden))
                .or_insert(0usize) += 1;
            if missing.contains_key(&test.key) {
                return None;
            }
        }
    }
    for (owner, group) in index.groups.iter().enumerate() {
        if after_groups.get(group) != Some(&declared[owner]) {
            return None;
        }
    }
    let mut outside_after = HashMap::new();
    for test in &after.tests {
        if index.containing_test(test).is_some() {
            if !own_hooks.contains(&test.key) {
                return None;
            }
        } else {
            *outside_after
                .entry((&test.key, test.done.hidden))
                .or_insert(0usize) += 1;
        }
    }
    if outside_before != outside_after {
        return None;
    }
    for group in &before.groups {
        if !after_groups.contains_key(&group.key) {
            let owner = index.containing(&group.key.suite, &group.key.chain)?;
            if group.key == index.groups[owner] {
                return None;
            }
        }
    }
    let actual_identities = identities(after);
    let missing_identities: BTreeMap<_, _> = identities(before)
        .into_iter()
        .filter_map(|(key, count)| {
            count
                .checked_sub(*actual_identities.get(&key).unwrap_or(&0))
                .filter(|count| *count > 0)
                .map(|count| (key, count))
        })
        .collect();
    let report = json!({"kind":"possible_setup_suppression","catalogueIdentityProven":false,
        "recommendedOutcome":"UNCLASSIFIED","reason":"author_test_catalogue_incomplete_after_setup_failure",
        "expectedEntries":before.tests.len(),"observedEntries":after.tests.len(),"missingEntries":missing.values().sum::<usize>(),
        "missingIdentities":missing_identities.into_iter().map(|((suite,name),count)|json!({"suite":suite,"name":name,"count":count})).collect::<Vec<_>>(),
        "failedSetups":failed.iter().map(|group|json!({"suite":group.suite.0,"group":group.chain.last().expect("validated group").name,"declaredTests":after_groups[group]})).collect::<Vec<_>>(),
        "baselineRawSha256":baseline.raw_sha256,"rawSha256":current.raw_sha256,
        "limitation":"Never-started declaration identities are not reported; this explains suppression without proving the complete catalogue or granting a killed verdict."});
    Some(Suppression {
        report,
        before,
        after,
        before_counts,
        after_counts,
    })
}

#[cfg(test)]
pub(super) fn explain_setup_suppression(
    current: &CatalogSnapshot,
    context: &SelectionContext,
) -> Option<Value> {
    suppression(current, context).map(|proof| proof.report)
}

static COMPILER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:^|\n)[^\n]*\.dart:\d+:\d+: Error:|^(?:Failed to load |Error: Compilation failed)",
    )
    .expect("fixed compiler diagnostic pattern")
});

fn witness(
    proof: &Suppression<'_>,
    current: &CatalogSnapshot,
    context: &SelectionContext,
) -> Option<Value> {
    let baseline: HashMap<_, _> = proof
        .before
        .tests
        .iter()
        .map(|test| (&test.key, test))
        .collect();
    let actual: HashMap<_, _> = proof
        .after
        .tests
        .iter()
        .map(|test| (&test.key, test))
        .collect();
    let groups = proof
        .before
        .tests
        .iter()
        .filter(|test| {
            hook(test, "setUpAll")
                && actual
                    .get(&test.key)
                    .is_some_and(|current| current.done.result != "success")
        })
        .map(|test| GroupKey {
            suite: test.suite.clone(),
            chain: test.chain.clone(),
        })
        .collect();
    let failed = GroupIndex::new(groups, false)?;
    let mut matcher = ErrorMatcher::new(&context.alias, &current.cwd)?;
    for test in &proof.after.tests {
        let Some(original) = baseline.get(&test.key) else {
            continue;
        };
        if proof.before_counts[&test.key] != 1
            || proof.after_counts[&test.key] != 1
            || original.done.result != "success"
            || original.done.hidden
            || original.done.skipped
            || original.metadata["skip"] != false
            || test.done.hidden
            || test.done.skipped
            || test.metadata["skip"] != false
            || !matches!(test.done.result.as_str(), "failure" | "error")
            || test.errors.is_empty()
            || test.location.0.is_none()
            || failed.containing_test(test).is_some()
        {
            continue;
        }
        let mut accepted = true;
        for error in &test.errors {
            if COMPILER.is_match(&error.error) || !matcher.author_frame(test, error)? {
                accepted = false;
                break;
            }
            let assertion = error.is_failure && error.error.starts_with("Expected:");
            if !assertion && !matcher.runtime_error(test, std::slice::from_ref(error))? {
                accepted = false;
                break;
            }
        }
        if !accepted {
            continue;
        }
        let chain:Option<Vec<_>>=test.chain.iter().map(|row|Some(json!({"name":row.name,"line":row.location.0,"column":row.location.1,"url":row.location.2,"metadata":serde_json::from_str::<Value>(&row.metadata_json).ok()?}))).collect();
        return Some(
            json!({"kind":"ordinary_author_failure","witnessIdentityProven":true,"catalogueIdentityProven":false,
            "suite":test.suite.0,"name":test.name,"location":test.location,"metadata":test.metadata,"groupChain":chain?,
            "baselineResult":"success","result":test.done.result,"baselineTestID":original.done.test_id,"rawTestID":test.done.test_id,
            "observedIdentityMultiplicity":1,"diagnosticSha256":crate::sha256_hex(python_diagnostics(&test.errors).as_bytes()),
            "baselineRawSha256":context.baseline.as_ref()?.raw_sha256,"rawSha256":current.raw_sha256}),
        );
    }
    None
}

#[cfg(test)]
pub(super) fn ordinary_failure_witness(
    current: &CatalogSnapshot,
    context: &SelectionContext,
) -> Option<Value> {
    witness(&suppression(current, context)?, current, context)
}

pub(super) fn selection_evidence(
    current: &CatalogSnapshot,
    context: &SelectionContext,
) -> Option<(Value, Option<Value>)> {
    let proof = suppression(current, context)?;
    let ordinary = witness(&proof, current, context);
    Some((proof.report, ordinary))
}

fn python_string(text: &str, output: &mut String) {
    output.push('"');
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if !('\u{20}'..'\u{7f}').contains(&value) => {
                for unit in value.encode_utf16(&mut [0; 2]) {
                    write!(output, "\\u{unit:04x}").expect("write into String");
                }
            }
            value => output.push(value),
        }
    }
    output.push('"');
}

/// Exactly json.dumps(..., sort_keys=True)'s ASCII encoding and separators for
/// the diagnostic schema. This digest is compatible with archived Python runs.
fn python_diagnostics(errors: &[Diagnostic]) -> String {
    let mut output = String::from("[");
    for (index, error) in errors.iter().enumerate() {
        if index > 0 {
            output.push_str(", ");
        }
        output.push_str("{\"error\": ");
        python_string(&error.error, &mut output);
        output.push_str(", \"isFailure\": ");
        output.push_str(if error.is_failure { "true" } else { "false" });
        output.push_str(", \"stackTrace\": ");
        python_string(&error.stack_trace, &mut output);
        output.push('}');
    }
    output.push(']');
    output
}

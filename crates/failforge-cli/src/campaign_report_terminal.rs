//! Plain-text final tables; rendering never changes results or scoring policy.
use failforge_dart::namespace::NamespaceError;
use serde_json::{Map, Value};

use crate::dart_namespace::invalid;

const OUTCOMES: &[&str] = &[
    "KILLED",
    "SURVIVED",
    "INVALID",
    "TIMEOUT",
    "INFRA_ERROR",
    "UNCLASSIFIED",
    "NO_TESTS",
    "CANCELLED",
];
const SCORES: &[(&str, &str, &str)] = &[
    ("classic", "mutationScore", "Mutation score"),
    ("failure_mode", "failureModeScore", "Failure mode score"),
    (
        "language_specific",
        "languageSpecificScore",
        "Language-specific score",
    ),
];

type Result<T> = std::result::Result<T, NamespaceError>;

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| invalid("invalid terminal report object"))
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| invalid("missing terminal report field"))
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    field(value, key)?
        .as_str()
        .ok_or_else(|| invalid("invalid terminal report string"))
}

fn integer(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .ok_or_else(|| invalid("invalid terminal report count"))
}

fn boolean(value: &Value, key: &str) -> Result<bool> {
    field(value, key)?
        .as_bool()
        .ok_or_else(|| invalid("invalid terminal report boolean"))
}

fn number(value: &Value) -> Result<f64> {
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .ok_or_else(|| invalid("invalid terminal report number"))
}

fn expected(value: Option<&Value>) -> Result<String> {
    match value {
        None | Some(Value::Null) => Ok("?".into()),
        Some(value) => Ok(integer(value)?.to_string()),
    }
}

fn count(counts: &Map<String, Value>, outcome: &str) -> Result<u64> {
    counts.get(outcome).map_or(Ok(0), integer)
}

fn score(value: Option<&Value>) -> Result<String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok("N/A (no scored mutants)".into());
    };
    let denominator = integer(field(value, "denominator")?)?;
    let percent = field(value, "percent")?;
    if denominator == 0 || percent.is_null() {
        return Ok("N/A (no scored mutants)".into());
    }
    Ok(format!(
        "{:.2}% ({}/{denominator})",
        number(percent)?,
        integer(field(value, "killed")?)?
    ))
}

fn border(widths: &[usize]) -> String {
    let mut line = String::from("+");
    for width in widths {
        line.push_str(&"-".repeat(width + 2));
        line.push('+');
    }
    line
}

/// Render exactly the public Python terminal schema, excluding its `Report:` line.
pub(crate) fn format_summary(summary: &Value) -> Result<String> {
    object(summary)?;
    if summary.get("kind").and_then(Value::as_str) == Some("profile") {
        return format_profile(summary);
    }
    let family = text(summary, "family")?;
    let (_, score_key, score_label) = SCORES
        .iter()
        .find(|(name, _, _)| *name == family)
        .ok_or_else(|| invalid("unknown terminal mutation family"))?;
    let counts = object(field(summary, "outcomeCounts")?)?;
    let values = OUTCOMES
        .iter()
        .map(|outcome| count(counts, outcome))
        .collect::<Result<Vec<_>>>()?;
    let width = values
        .iter()
        .map(|value| value.to_string().len())
        .max()
        .unwrap_or(0)
        .max(5);
    let border = border(&[12, width, 8]);
    let mut lines = vec![
        "FailForge mutation summary".into(),
        format!("Family: {family}"),
    ];
    if family == "language_specific" {
        lines.push(format!("Language: {}", text(summary, "language")?));
    }
    lines.extend([
        format!("Status: {}", text(summary, "status")?),
        format!(
            "Mutants: {}/{}",
            integer(field(summary, "completedMutants")?)?,
            expected(Some(field(summary, "expectedMutants")?))?
        ),
        String::new(),
        border.clone(),
        format!(
            "| {:<12} | {:>width$} | {:<8} |",
            "Outcome", "Count", "In score"
        ),
        border.clone(),
    ]);
    for (outcome, value) in OUTCOMES.iter().zip(values) {
        let included = if matches!(*outcome, "KILLED" | "SURVIVED") {
            "yes"
        } else {
            "no"
        };
        lines.push(format!(
            "| {outcome:<12} | {value:>width$} | {included:<8} |"
        ));
    }
    let selected = field(summary, score_key)?;
    let score = if boolean(summary, "complete")? {
        score(Some(selected))?
    } else {
        "N/A (campaign incomplete)".into()
    };
    lines.extend([
        border,
        String::new(),
        format!("{score_label}: {score}"),
        format!("Wall time: {:.2}s", number(field(summary, "wallSeconds")?)?),
    ]);
    Ok(lines.join("\n"))
}

fn language(entry: &Value, child: Option<&Value>) -> Result<Option<String>> {
    for value in [
        entry.get("language"),
        child.and_then(|child| child.get("language")),
    ] {
        if let Some(value) = value.filter(|value| !value.is_null()) {
            let language = value
                .as_str()
                .ok_or_else(|| invalid("invalid terminal report language"))?;
            if !language.is_empty() {
                return Ok(Some(language.to_owned()));
            }
        }
    }
    Ok(None)
}

fn format_profile(summary: &Value) -> Result<String> {
    let complete = boolean(summary, "complete")?;
    let families = field(summary, "families")?;
    let mut rows = Vec::with_capacity(SCORES.len());
    for (family, key, _) in SCORES {
        let entry = field(families, family)?;
        object(entry)?;
        let child = entry.get("summary").filter(|value| !value.is_null());
        if let Some(child) = child {
            object(child)?;
        }
        let mut label = (*family).to_owned();
        if *family == "language_specific" {
            if let Some(language) = language(entry, child)? {
                label.push_str(" / ");
                label.push_str(&language);
            }
        }
        let completed = child.map_or(Ok(0), |child| integer(field(child, "completedMutants")?))?;
        let total = match child {
            Some(child) => Some(field(child, "expectedMutants")?),
            None => entry.get("expectedMutants"),
        };
        let mut count = format!("{completed}/{}", expected(total)?);
        let score = if !boolean(entry, "enabled")? {
            count = "-".into();
            "-".into()
        } else if !complete {
            "N/A (profile incomplete)".into()
        } else {
            score(child.and_then(|child| child.get(key)))?
        };
        rows.push([label, text(entry, "status")?.into(), count, score]);
    }
    let headings = ["Family", "Status", "Mutants", "Score"].map(str::to_owned);
    let widths = std::array::from_fn::<_, 4, _>(|index| {
        rows.iter()
            .chain(std::iter::once(&headings))
            .map(|row| row[index].chars().count())
            .max()
            .unwrap_or(0)
    });
    let border = border(&widths);
    let row_text = |row: &[String; 4]| {
        format!(
            "| {} |",
            row.iter()
                .zip(widths)
                .map(|(value, width)| format!("{value:<width$}"))
                .collect::<Vec<_>>()
                .join(" | ")
        )
    };
    let mut lines = vec![
        "FailForge profile summary".into(),
        format!("Status: {}", text(summary, "status")?),
        format!(
            "Mutants: {}/{}",
            integer(field(summary, "completedMutants")?)?,
            expected(Some(field(summary, "expectedMutants")?))?
        ),
        String::new(),
        border.clone(),
        row_text(&headings),
        border.clone(),
    ];
    lines.extend(rows.iter().map(row_text));
    lines.extend([
        border,
        String::new(),
        "Outcomes (counts across selected families):".into(),
    ]);
    let counts = object(field(summary, "outcomeCounts")?)?;
    for outcome in OUTCOMES {
        lines.push(format!("  {outcome}: {}", count(counts, outcome)?));
    }
    lines.extend([
        String::new(),
        format!("Wall time: {:.2}s", number(field(summary, "wallSeconds")?)?),
    ]);
    Ok(lines.join("\n"))
}

#[cfg(test)]
#[path = "campaign_report_terminal_tests.rs"]
mod tests;

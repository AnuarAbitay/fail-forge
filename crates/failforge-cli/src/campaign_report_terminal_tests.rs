use serde_json::json;

use super::*;

fn direct() -> Value {
    json!({
        "family":"classic","status":"completed","complete":true,
        "completedMutants":7,"expectedMutants":7,
        "outcomeCounts":{"KILLED":3,"SURVIVED":1,"INVALID":2,"TIMEOUT":1},
        "mutationScore":{"killed":3,"denominator":4,"percent":75.0},
        "failureModeScore":null,"wallSeconds":12.34
    })
}

fn profile() -> Value {
    let mut families = Map::new();
    for (family, key, killed, total) in [
        ("classic", "mutationScore", 1, 2),
        ("failure_mode", "failureModeScore", 3, 4),
        ("language_specific", "languageSpecificScore", 1, 4),
    ] {
        let mut child = json!({"completedMutants":total,"expectedMutants":total});
        child[key] = json!({"killed":killed,"denominator":total,"percent":100.0 * killed as f64 / total as f64});
        if family == "language_specific" {
            child["language"] = "dart".into();
        }
        families.insert(
            family.to_owned(),
            json!({"enabled":true,"status":"completed","complete":true,"summary":child}),
        );
    }
    json!({
        "kind":"profile","status":"completed","complete":true,
        "completedMutants":10,"expectedMutants":10,"families":families,
        "outcomeCounts":{"KILLED":5,"SURVIVED":5},"wallSeconds":12.345,
        "mutationScore":{"killed":1,"denominator":1,"percent":100.0}
    })
}

#[test]
fn direct_golden_table_matches_existing_public_output() {
    assert_eq!(
        format_summary(&direct()).unwrap(),
        "FailForge mutation summary\n\
Family: classic\n\
Status: completed\n\
Mutants: 7/7\n\
\n\
+--------------+-------+----------+\n\
| Outcome      | Count | In score |\n\
+--------------+-------+----------+\n\
| KILLED       |     3 | yes      |\n\
| SURVIVED     |     1 | yes      |\n\
| INVALID      |     2 | no       |\n\
| TIMEOUT      |     1 | no       |\n\
| INFRA_ERROR  |     0 | no       |\n\
| UNCLASSIFIED |     0 | no       |\n\
| NO_TESTS     |     0 | no       |\n\
| CANCELLED    |     0 | no       |\n\
+--------------+-------+----------+\n\
\n\
Mutation score: 75.00% (3/4)\n\
Wall time: 12.34s"
    );
}

#[test]
fn each_direct_family_uses_its_own_score_and_language_label() {
    for (family, key, label) in SCORES {
        let mut value = direct();
        value["family"] = (*family).into();
        value["language"] = "dart".into();
        value[*key] = json!({"killed":3,"denominator":8,"percent":37.5});
        let rendered = format_summary(&value).unwrap();
        assert!(rendered.contains(&format!("{label}: 37.50% (3/8)")));
        assert_eq!(
            rendered.contains("Language: dart\n"),
            *family == "language_specific"
        );
        assert!(!rendered.contains("75.00%"));
    }
}

#[test]
fn incomplete_campaigns_hide_even_preexisting_scores_for_all_families() {
    for (family, key, _) in SCORES {
        for status in ["failed", "cancelled", "running"] {
            let mut value = direct();
            value["family"] = (*family).into();
            value["language"] = "dart".into();
            value["status"] = status.into();
            value["complete"] = false.into();
            value["expectedMutants"] = Value::Null;
            value[*key] = json!({"killed":1,"denominator":1,"percent":100.0});
            let rendered = format_summary(&value).unwrap();
            assert!(rendered.contains("Mutants: 7/?"));
            assert!(rendered.contains("N/A (campaign incomplete)"));
            assert!(!rendered.contains('%'));
        }
    }
}

#[test]
fn empty_and_excluded_campaigns_have_no_score() {
    for score_value in [
        Value::Null,
        json!({"killed":0,"denominator":0,"percent":null}),
        json!({"killed":0,"denominator":3,"percent":null}),
    ] {
        let mut value = direct();
        value["mutationScore"] = score_value;
        let rendered = format_summary(&value).unwrap();
        assert!(rendered.contains("Mutation score: N/A (no scored mutants)"));
        assert!(!rendered.contains('%'));
    }
}

#[test]
fn percent_and_time_use_two_decimals_without_recalculating_the_score() {
    for (killed, denominator, percent, display) in [
        (0, 3, 0.0, "0.00% (0/3)"),
        (3, 3, 100.0, "100.00% (3/3)"),
        (2, 3, 200.0 / 3.0, "66.67% (2/3)"),
        (1, 8, 12.5, "12.50% (1/8)"),
    ] {
        let mut value = direct();
        value["mutationScore"] =
            json!({"killed":killed,"denominator":denominator,"percent":percent});
        value["wallSeconds"] = 12.345.into();
        let rendered = format_summary(&value).unwrap();
        assert!(rendered.contains(display));
        assert!(rendered.ends_with("Wall time: 12.35s"));
    }
}

#[test]
fn wide_counts_expand_every_direct_border_and_column() {
    let mut value = direct();
    value["outcomeCounts"]["KILLED"] = 1_234_567.into();
    let rendered = format_summary(&value).unwrap();
    assert!(rendered.contains("+--------------+---------+----------+"));
    assert!(rendered.contains("| KILLED       | 1234567 | yes      |"));
    assert!(rendered.contains("| CANCELLED    |       0 | no       |"));
    let widths: Vec<_> = rendered
        .lines()
        .filter(|line| line.starts_with(['+', '|']))
        .map(str::len)
        .collect();
    assert!(widths.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn all_outcomes_keep_order_counts_and_exclusion_labels() {
    let mut value = direct();
    let counts: Map<_, _> = OUTCOMES
        .iter()
        .enumerate()
        .map(|(index, outcome)| ((*outcome).to_owned(), json!(index + 1)))
        .collect();
    value["outcomeCounts"] = counts.into();
    let rendered = format_summary(&value).unwrap();
    let rows: Vec<_> = rendered
        .lines()
        .filter(|line| line.starts_with("| ") && !line.contains("Outcome"))
        .collect();
    assert_eq!(rows.len(), OUTCOMES.len());
    for (index, outcome) in OUTCOMES.iter().enumerate() {
        let included = if index < 2 { "yes" } else { "no" };
        assert_eq!(
            rows[index],
            format!("| {outcome:<12} | {:>5} | {included:<8} |", index + 1)
        );
    }
}

#[test]
fn profile_has_independent_scores_and_every_outcome() {
    let rendered = format_summary(&profile()).unwrap();
    assert!(rendered.starts_with("FailForge profile summary\nStatus: completed\nMutants: 10/10"));
    for text in [
        "50.00% (1/2)",
        "75.00% (3/4)",
        "25.00% (1/4)",
        "language_specific / dart",
        "Wall time: 12.35s",
    ] {
        assert!(rendered.contains(text), "{text}");
    }
    assert!(!rendered.contains("100.00%"));
    assert!(!rendered.contains("Mutation score:"));
    for outcome in OUTCOMES {
        assert!(rendered.contains(&format!("  {outcome}: ")));
    }
    assert!(rendered.contains("  TIMEOUT: 0"));
}

#[test]
fn profile_preserves_disabled_unstarted_unknown_and_cancelled_rows() {
    for status in ["failed", "cancelled"] {
        let mut value = profile();
        value["status"] = status.into();
        value["complete"] = false.into();
        value["expectedMutants"] = Value::Null;
        value["families"]["classic"] = json!({"enabled":false,"status":"disabled","summary":null});
        value["families"]["failure_mode"] =
            json!({"enabled":true,"status":"not_started","summary":null});
        let rendered = format_summary(&value).unwrap();
        assert!(rendered.contains("Mutants: 10/?"));
        assert!(rendered.contains("disabled"));
        assert!(rendered.contains("not_started"));
        assert_eq!(rendered.matches("N/A (profile incomplete)").count(), 2);
        assert!(rendered.contains("0/?"));
        assert!(!rendered.contains('%'));
    }
}

#[test]
fn completed_profiles_without_scored_results_show_na_per_family() {
    let mut value = profile();
    for (family, key, _) in SCORES {
        value["families"][*family]["summary"][*key] =
            json!({"killed":0,"denominator":0,"percent":null});
    }
    assert_eq!(
        format_summary(&value)
            .unwrap()
            .matches("N/A (no scored mutants)")
            .count(),
        3
    );
}

#[test]
fn profile_column_widths_include_long_statuses_and_prefer_entry_language() {
    let mut value = profile();
    value["families"]["language_specific"]["language"] = "another_language".into();
    value["families"]["failure_mode"]["status"] = "a_very_long_status_name".into();
    value["families"]["failure_mode"]["summary"]["completedMutants"] = 1_234_567.into();
    let rendered = format_summary(&value).unwrap();
    assert!(rendered.contains("language_specific / another_language"));
    assert!(!rendered.contains("language_specific / dart"));
    assert!(rendered.contains("1234567/4"));
    let widths: Vec<_> = rendered
        .lines()
        .filter(|line| line.starts_with(['+', '|']))
        .map(str::len)
        .collect();
    assert!(widths.windows(2).all(|pair| pair[0] == pair[1]));
}

#[test]
fn render_is_deterministic_plain_text_and_does_not_mutate_inputs() {
    for value in [direct(), profile()] {
        let before = value.clone();
        let rendered = format_summary(&value).unwrap();
        assert_eq!(rendered, format_summary(&value).unwrap());
        assert_eq!(value, before);
        assert!(rendered.is_ascii());
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("Report:"));
        assert!(!rendered.ends_with('\n'));
    }
}

#[test]
fn malformed_fields_fail_without_panics_or_fabricating_a_table() {
    for (key, invalid_value) in [
        ("family", json!("unknown")),
        ("status", json!(42)),
        ("complete", json!("true")),
        ("completedMutants", json!(-1)),
        ("expectedMutants", json!(true)),
        ("outcomeCounts", json!([])),
        ("wallSeconds", json!(-0.1)),
        ("wallSeconds", Value::Null),
        ("mutationScore", json!({"denominator":1,"percent":"75"})),
    ] {
        let mut value = direct();
        value[key] = invalid_value;
        assert!(format_summary(&value).is_err(), "{key}");
    }
    let mut value = direct();
    value["outcomeCounts"]["KILLED"] = "3".into();
    assert!(format_summary(&value).is_err());
    let mut value = profile();
    value["families"]["classic"]["summary"] = json!([]);
    assert!(format_summary(&value).is_err());
    let mut value = profile();
    value["families"]["classic"]["enabled"] = 1.into();
    assert!(format_summary(&value).is_err());
    assert!(format_summary(&json!(null)).is_err());
}

//! Configuration names and registry order; adapter handshake remains authoritative.

pub const FAMILIES: &[&str] = &["classic", "failure_mode", "language_specific"];

pub const CLASSIC_OPERATORS: &[&str] = &[
    "RELATIONAL_BOUNDARY",
    "COMPARISON_NEGATION",
    "COMPARISON_OPERATOR_REPLACEMENT",
    "LOGICAL_CONNECTOR_SWAP",
    "LOGICAL_OPERAND_REMOVAL",
    "ARITHMETIC_OPERATOR_REPLACEMENT",
    "ARITHMETIC_OPERAND_REMOVAL",
    "NUMERIC_NEGATION",
    "INCREMENT_DECREMENT_REPLACEMENT",
    "COMPOUND_ASSIGNMENT_REPLACEMENT",
    "BOOLEAN_LITERAL_FLIP",
    "BOOLEAN_NEGATION",
    "CONDITIONAL_NEGATION",
    "NUMERIC_LITERAL_REPLACEMENT",
    "STRING_LITERAL_REPLACEMENT",
    "RETURN_VALUE_REPLACEMENT",
    "CONDITION_FORCING",
    "BRANCH_BODY_REMOVAL",
    "CONDITIONAL_EXPRESSION_MUTATION",
    "STATEMENT_DELETION",
    "BREAK_CONTINUE_MUTATION",
    "SWITCH_CASE_MUTATION",
    "FUNCTION_CALL_REMOVAL",
    "ARGUMENT_REPLACEMENT",
    "ARGUMENT_PERMUTATION",
    "VARIABLE_REPLACEMENT",
    "BITWISE_OPERATOR_REPLACEMENT",
    "BITWISE_NEGATION",
    "SHIFT_OPERATOR_REPLACEMENT",
    "THROW_REMOVAL",
    "EXCEPTION_REPLACEMENT",
    "COLLECTION_LITERAL_MUTATION",
];

pub const FAILURE_MODE_OPERATORS: &[&str] = &["EXCEPTION_AT_ENTRY", "METHOD_CHANNEL_ERROR"];
pub const DART_OPERATOR_GROUPS: &[(&str, &[&str])] = &[
    (
        "null_safety",
        &["NSM-001", "NSM-002", "NSM-003", "NSM-004", "NSM-005"],
    ),
    ("parameters", &["DPM-001", "DPM-002", "DPM-003"]),
    ("switch", &["SWM-001", "SWM-002", "SWM-003"]),
    ("patterns", &["PMM-001", "PMM-002", "PMM-003"]),
    ("collections", &["CLM-001", "CLM-002", "CLM-003", "CLM-004"]),
    ("enums", &["ENM-001"]),
];
pub const DART_OPERATORS: &[&str] = &[
    "NSM-001", "NSM-002", "NSM-003", "NSM-004", "NSM-005", "DPM-001", "DPM-002", "DPM-003",
    "SWM-001", "SWM-002", "SWM-003", "PMM-001", "PMM-002", "PMM-003", "CLM-001", "CLM-002",
    "CLM-003", "CLM-004", "ENM-001",
];

pub fn operators(family: &str) -> Option<&'static [&'static str]> {
    match family {
        "classic" => Some(CLASSIC_OPERATORS),
        "failure_mode" => Some(FAILURE_MODE_OPERATORS),
        "language_specific" => Some(DART_OPERATORS),
        _ => None,
    }
}

pub fn default_operators(family: &str, flutter: bool) -> Option<Vec<&'static str>> {
    match family {
        "classic" => Some(
            CLASSIC_OPERATORS
                .iter()
                .copied()
                .filter(|operator| *operator != "CONDITIONAL_NEGATION")
                .collect(),
        ),
        "failure_mode" => Some(
            if flutter {
                FAILURE_MODE_OPERATORS
            } else {
                &FAILURE_MODE_OPERATORS[..1]
            }
            .to_vec(),
        ),
        _ => operators(family).map(<[_]>::to_vec),
    }
}

pub(crate) fn valid_operator_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
    {
        return true;
    }
    value.split_once('-').is_some_and(|(prefix, suffix)| {
        ["NSM", "DPM", "SWM", "PMM", "CLM", "ENM"].contains(&prefix)
            && suffix.len() == 3
            && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn catalogue_preserves_registry_order_and_legacy_default_distinction() {
        assert_eq!(CLASSIC_OPERATORS.len(), 32);
        assert_eq!(default_operators("classic", false).unwrap().len(), 31);
        assert!(
            !default_operators("classic", false)
                .unwrap()
                .contains(&"CONDITIONAL_NEGATION")
        );
        assert!(CLASSIC_OPERATORS.contains(&"CONDITIONAL_NEGATION"));
        assert_eq!(
            default_operators("failure_mode", false).unwrap(),
            ["EXCEPTION_AT_ENTRY"]
        );
        assert_eq!(
            default_operators("failure_mode", true).unwrap(),
            FAILURE_MODE_OPERATORS
        );
        let flattened: Vec<_> = DART_OPERATOR_GROUPS
            .iter()
            .flat_map(|(_, ids)| ids.iter().copied())
            .collect();
        assert_eq!(flattened, DART_OPERATORS);
        assert_eq!(flattened.len(), 19);
        for family in FAMILIES {
            let ids = operators(family).unwrap();
            assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
            assert!(ids.iter().all(|id| valid_operator_id(id)));
        }
        assert!(operators("unknown").is_none());
        assert!(default_operators("unknown", false).is_none());
    }

    #[test]
    fn direct_operator_syntax_does_not_silently_become_a_known_catalogue_check() {
        for valid in ["A", "FUTURE_OPERATOR", "NSM-999", "ENM-001"] {
            assert!(valid_operator_id(valid));
        }
        assert!(valid_operator_id(&"A".repeat(128)));
        for invalid in [
            "",
            "lowercase",
            "BAD-ID",
            "NSM-01",
            "NSM-0001",
            "ABC-001",
            "1A",
            "A\n",
            "А",
        ] {
            assert!(!valid_operator_id(invalid), "{invalid:?}");
        }
        assert!(!valid_operator_id(&"A".repeat(129)));
    }
}

//! A bounded, deliberately restricted YAML loader for mutation configuration.
//!
//! Syntax is handled by `libyaml-safer` events. Scalar resolution retains the
//! existing YAML 1.1 configuration types, rather than silently changing plain
//! `012`, `yes` or `2001-12-15` into strings during the Rust migration.
use std::borrow::Cow;
use std::io::{BufRead, BufReader};
use std::sync::LazyLock;

use libyaml_safer::{Encoding, ErrorKind, EventData, Parser, ScalarStyle, Scanner, TokenData};
use regex::Regex;
use serde_json::{Map, Number, Value};

use crate::{ConfigError, MAX_YAML_BYTES, MAX_YAML_DEPTH, MAX_YAML_SCALAR_CHARACTERS};

const BOOLEAN_ERROR: &str = "boolean settings must use unquoted true or false";
const KEY_ERROR: &str = "YAML mapping keys must be strings";
const TAG_ERROR: &str = "explicit YAML tags are not supported";
const ALIAS_ERROR: &str = "YAML aliases and anchors are not supported";

// These are the scalar categories of the existing PyYAML SafeLoader. In
// particular an exponent without a decimal point is a string, and a float
// exponent requires a sign. Quoted/block scalars never use these resolvers.
static INTEGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)\z",
    )
    .expect("fixed YAML integer resolver")
});
static FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(?:[-+]?[0-9][0-9_]*\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))\z",
    )
    .expect("fixed YAML float resolver")
});
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(?:[0-9]{4}-[0-9]{2}-[0-9]{2}|[0-9]{4}-[0-9]{1,2}-[0-9]{1,2}(?:[Tt]|[ \t]+)[0-9]{1,2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9]{1,2}(?::[0-9]{2})?))?)\z",
    )
    .expect("fixed YAML timestamp resolver")
});

enum Container {
    Sequence(Vec<Value>),
    Mapping {
        values: Map<String, Value>,
        key: Option<String>,
    },
}

fn invalid_yaml() -> ConfigError {
    ConfigError::new("invalid YAML configuration")
}

fn reject_properties(anchored: bool, explicit_tag: bool) -> Result<(), ConfigError> {
    if anchored {
        return Err(ConfigError::new(ALIAS_ERROR));
    }
    if explicit_tag {
        return Err(ConfigError::new(TAG_ERROR));
    }
    Ok(())
}

fn append(
    containers: &mut [Container],
    root: &mut Option<Value>,
    value: Value,
) -> Result<(), ConfigError> {
    match containers.last_mut() {
        Some(Container::Sequence(values)) => values.push(value),
        Some(Container::Mapping { values, key }) => {
            if let Some(key) = key.take() {
                values.insert(key, value);
            } else {
                let Value::String(name) = value else {
                    return Err(ConfigError::new(KEY_ERROR));
                };
                if values.contains_key(&name) {
                    return Err(ConfigError::new(format!(
                        "duplicate YAML mapping key: {name}"
                    )));
                }
                *key = Some(name);
            }
        }
        None => {
            if root.replace(value).is_some() {
                return Err(invalid_yaml());
            }
        }
    }
    Ok(())
}

fn start_container(containers: &mut Vec<Container>, value: Container) -> Result<(), ConfigError> {
    if matches!(
        containers.last(),
        Some(Container::Mapping { key: None, .. })
    ) {
        return Err(ConfigError::new(KEY_ERROR));
    }
    if containers.len() >= MAX_YAML_DEPTH {
        return Err(ConfigError::new("YAML configuration is nested too deeply"));
    }
    containers.push(value);
    Ok(())
}

fn integer(text: &str) -> Result<Value, ConfigError> {
    let without_separators = text.replace('_', "");
    let unsigned = without_separators.trim_start_matches(['-', '+']);
    let bad_integer = || ConfigError::new("YAML integer is outside the supported range");
    let magnitude = if unsigned.contains(':') {
        unsigned.split(':').try_fold(0_u64, |value, part| {
            value
                .checked_mul(60)
                .and_then(|value| value.checked_add(part.parse::<u64>().ok()?))
                .ok_or_else(bad_integer)
        })?
    } else {
        let (digits, radix) = if let Some(digits) = unsigned.strip_prefix("0b") {
            (digits, 2)
        } else if let Some(digits) = unsigned.strip_prefix("0x") {
            (digits, 16)
        } else if unsigned.starts_with('0') && unsigned.len() > 1 {
            (&unsigned[1..], 8)
        } else {
            (unsigned, 10)
        };
        u64::from_str_radix(digits, radix).map_err(|_| bad_integer())?
    };
    if text.starts_with('-') {
        let negative = if magnitude == (i64::MAX as u64) + 1 {
            i64::MIN
        } else {
            -i64::try_from(magnitude).map_err(|_| bad_integer())?
        };
        Ok(Value::Number(Number::from(negative)))
    } else {
        Ok(Value::Number(Number::from(magnitude)))
    }
}

fn float(text: &str) -> Result<Value, ConfigError> {
    let normalized = text.replace('_', "");
    let unsigned = normalized.trim_start_matches(['-', '+']);
    let bad_float = || ConfigError::new("non-finite or unsupported YAML numeric scalar");
    let number = if unsigned.contains(':') {
        let magnitude = unsigned.split(':').try_fold(0.0_f64, |value, part| {
            part.parse::<f64>()
                .map(|part| value * 60.0 + part)
                .map_err(|_| bad_float())
        })?;
        if normalized.starts_with('-') {
            -magnitude
        } else {
            magnitude
        }
    } else {
        normalized.parse::<f64>().map_err(|_| bad_float())?
    };
    Number::from_f64(number)
        .map(Value::Number)
        .ok_or_else(bad_float)
}

fn scalar(text: String, style: ScalarStyle) -> Result<Value, ConfigError> {
    if text.chars().take(MAX_YAML_SCALAR_CHARACTERS + 1).count() > MAX_YAML_SCALAR_CHARACTERS {
        return Err(ConfigError::new("YAML scalar is too large"));
    }
    if style != ScalarStyle::Plain {
        return Ok(Value::String(text));
    }
    match text.as_str() {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "yes" | "Yes" | "YES" | "no" | "No" | "NO" | "True" | "TRUE" | "False" | "FALSE" | "on"
        | "On" | "ON" | "off" | "Off" | "OFF" => {
            return Err(ConfigError::new(BOOLEAN_ERROR));
        }
        "" | "~" | "null" | "Null" | "NULL" => return Ok(Value::Null),
        "<<" | "=" => return Err(ConfigError::new("unsupported implicit YAML tag")),
        _ => {}
    }
    if INTEGER.is_match(&text) {
        return integer(&text);
    }
    if FLOAT.is_match(&text) {
        return float(&text);
    }
    if TIMESTAMP.is_match(&text) {
        // There is no date/datetime configuration field. Treating a YAML 1.1
        // timestamp as a string would accidentally permit it as a path/name.
        return Err(ConfigError::new(
            "YAML timestamps are not configuration strings; quote the value",
        ));
    }
    Ok(Value::String(text))
}

const PREAMBLE_BUFFER_BYTES: usize = 1024;

struct PreambleEdit {
    start: usize,
    end: usize,
    replacement: String,
}

/// Normalize a YAML 1.x preamble for the shared event parser.
///
/// This preserves the existing loader's handling of unknown directives and
/// minor versions. Callers must still parse and validate the document body.
pub fn prepare_preamble(text: &str) -> Result<Cow<'_, str>, ConfigError> {
    prepare_preamble_with(text, false, |input| {
        BufReader::with_capacity(PREAMBLE_BUFFER_BYTES, input)
    })
}

/// Prepare an author-owned YAML preamble, retaining tag-handle directives.
///
/// The caller must validate resolved tags and document values. Mutation-profile
/// parsing deliberately uses [`prepare_preamble`] and keeps its stricter policy.
pub fn prepare_author_preamble(text: &str) -> Result<Cow<'_, str>, ConfigError> {
    prepare_preamble_with(text, true, |input| {
        BufReader::with_capacity(PREAMBLE_BUFFER_BYTES, input)
    })
}

fn prepare_preamble_with<'a, R: BufRead>(
    text: &'a str,
    allow_tag_directives: bool,
    mut reader: impl FnMut(&'a [u8]) -> R,
) -> Result<Cow<'a, str>, ConfigError> {
    // Forced UTF-8 keeps a leading BOM in the stream, and libyaml's BOM skip
    // advances the column: the first block key lands at column 1 and the next
    // key at column 0 becomes a second root node. Drop that one signature here,
    // so callers parse the prepared text rather than the original.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // libyaml rejects unknown directives and versions other than 1.1/1.2;
    // SafeLoader ignores unknown directives and permits other 1.x minors.
    // Only the actual scanner's preamble tokens/errors identify rewrites.
    let mut scanner = Scanner::new();
    scanner.set_input(reader(text.as_bytes()));
    scanner.set_encoding(Encoding::Utf8);
    let mut offset = 0;
    let mut directives = false;
    let mut edits = Vec::new();
    loop {
        match Scanner::scan(&mut scanner) {
            Ok(token) => match token.data {
                TokenData::StreamStart { .. } => {}
                TokenData::VersionDirective { major, minor } => {
                    directives = true;
                    if major != 1 {
                        return Err(invalid_yaml());
                    }
                    if !matches!(minor, 1 | 2) {
                        // Scanner Mark.index counts original UTF-8 bytes, not
                        // Unicode characters. Forcing UTF-8 keeps that index in
                        // bytes, so both token spans map directly to text.
                        let start = offset
                            + usize::try_from(token.start_mark.index)
                                .map_err(|_| invalid_yaml())?;
                        let end = offset
                            + usize::try_from(token.end_mark.index).map_err(|_| invalid_yaml())?;
                        let source = text.get(start..end).ok_or_else(invalid_yaml)?;
                        if !source.starts_with("%YAML") || source.len() < 9 {
                            return Err(invalid_yaml());
                        }
                        edits.push(PreambleEdit {
                            start,
                            end,
                            replacement: format!("%YAML 1.1{}", " ".repeat(source.len() - 9)),
                        });
                    }
                }
                TokenData::TagDirective { .. } if allow_tag_directives => directives = true,
                TokenData::TagDirective { .. } => return Err(ConfigError::new(TAG_ERROR)),
                TokenData::DocumentStart => break,
                _ if directives => return Err(invalid_yaml()),
                _ => break,
            },
            Err(error) => {
                if error.kind() != ErrorKind::Scanner
                    || error.context() != Some("while scanning a directive")
                    || error.problem() != "found unknown directive name"
                {
                    return Err(invalid_yaml());
                }
                let mark = error.context_mark().ok_or_else(invalid_yaml)?;
                let start = offset + usize::try_from(mark.index).map_err(|_| invalid_yaml())?;
                if mark.column != 0 || text.as_bytes().get(start) != Some(&b'%') {
                    return Err(invalid_yaml());
                }
                edits.push(PreambleEdit {
                    start,
                    end: start + 1,
                    replacement: "#".into(),
                });
                directives = true;
                // Resume after this directive's line, never replay its prefix.
                // The bounded BufReader also prevents a reset from eagerly
                // decoding the entire remaining suffix for every directive.
                let remaining = text.get(start..).ok_or_else(invalid_yaml)?;
                offset = remaining
                    .char_indices()
                    .find_map(|(index, character)| {
                        matches!(character, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
                            .then_some(start + index + character.len_utf8())
                    })
                    .unwrap_or(text.len());
                if offset < text.len()
                    && text.as_bytes()[offset - 1] == b'\r'
                    && text.as_bytes()[offset] == b'\n'
                {
                    offset += 1;
                }
                scanner.reset();
                scanner.set_input(reader(&text.as_bytes()[offset..]));
                scanner.set_encoding(Encoding::Utf8);
            }
        }
    }
    if edits.is_empty() {
        return Ok(Cow::Borrowed(text));
    }
    let mut value = String::with_capacity(text.len());
    let mut copied = 0;
    for edit in edits {
        value.push_str(text.get(copied..edit.start).ok_or_else(invalid_yaml)?);
        value.push_str(&edit.replacement);
        copied = edit.end;
    }
    value.push_str(text.get(copied..).ok_or_else(invalid_yaml)?);
    Ok(Cow::Owned(value))
}

/// Parse one restricted YAML document without coercing ambiguous keys to text.
///
/// Empty streams retain YAML's null value; configuration shape validation then
/// rejects them. Containers are built iteratively and nesting is bounded before
/// pushing a frame. Anchors, aliases and explicit tags never enter the result.
pub fn parse_yaml(text: &str) -> Result<Value, ConfigError> {
    if text.len() > MAX_YAML_BYTES {
        return Err(ConfigError::new("YAML configuration exceeds 1 MiB"));
    }
    // Validate the original stream, including comments, so an embedded NUL
    // cannot hide trailing documents behind a parser's virtual end marker.
    // Escaped controls in double-quoted scalars remain the schema's concern.
    if text.chars().any(|character| {
        !matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{7e}' | '\u{85}'
            | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
    }) {
        return Err(invalid_yaml());
    }
    // The parser consumes original UTF-8: it implements the existing NEL/LS/PS
    // line-break folding without a preprocessing lexer or lossy replacement.
    let prepared = prepare_preamble(text)?;
    let mut parser = Parser::new();
    parser.set_input(BufReader::with_capacity(
        PREAMBLE_BUFFER_BYTES,
        prepared.as_bytes(),
    ));
    parser.set_encoding(Encoding::Utf8);
    let mut containers = Vec::new();
    let mut root = None;
    let mut document_count = 0;
    let mut document_open = false;
    loop {
        let event = parser.parse().map_err(|_| invalid_yaml())?;
        match event.data {
            EventData::StreamStart { .. } => {}
            EventData::DocumentStart {
                version_directive,
                tag_directives,
                ..
            } => {
                if !tag_directives.is_empty() {
                    return Err(ConfigError::new(TAG_ERROR));
                }
                if version_directive.is_some_and(|version| version.major != 1) {
                    return Err(invalid_yaml());
                }
                document_count += 1;
                if document_count > 1 {
                    return Err(ConfigError::new(
                        "configuration must contain exactly one YAML document",
                    ));
                }
                document_open = true;
            }
            EventData::DocumentEnd { .. } => {
                if !document_open || !containers.is_empty() || root.is_none() {
                    return Err(invalid_yaml());
                }
                document_open = false;
            }
            EventData::StreamEnd => {
                if document_open || !containers.is_empty() {
                    return Err(invalid_yaml());
                }
                return Ok(root.unwrap_or(Value::Null));
            }
            EventData::Alias { .. } => return Err(ConfigError::new(ALIAS_ERROR)),
            EventData::Scalar {
                value,
                style,
                anchor,
                tag,
                ..
            } if document_open => {
                reject_properties(anchor.is_some(), tag.is_some())?;
                append(&mut containers, &mut root, scalar(value, style)?)?;
            }
            EventData::SequenceStart { anchor, tag, .. } if document_open => {
                reject_properties(anchor.is_some(), tag.is_some())?;
                start_container(&mut containers, Container::Sequence(Vec::new()))?;
            }
            EventData::MappingStart { anchor, tag, .. } if document_open => {
                reject_properties(anchor.is_some(), tag.is_some())?;
                start_container(
                    &mut containers,
                    Container::Mapping {
                        values: Map::new(),
                        key: None,
                    },
                )?;
            }
            EventData::SequenceEnd if document_open => {
                let Some(Container::Sequence(values)) = containers.pop() else {
                    return Err(invalid_yaml());
                };
                append(&mut containers, &mut root, Value::Array(values))?;
            }
            EventData::MappingEnd if document_open => {
                let Some(Container::Mapping { values, key: None }) = containers.pop() else {
                    return Err(invalid_yaml());
                };
                append(&mut containers, &mut root, Value::Object(values))?;
            }
            _ => return Err(invalid_yaml()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn error(text: &str, expected: &str) {
        let error = parse_yaml(text).unwrap_err().to_string();
        let preview = text.chars().take(120).collect::<String>();
        assert!(
            error.contains(expected),
            "{preview:?} ({} input bytes): {error}",
            text.len()
        );
    }

    #[test]
    fn nested_block_flow_and_literal_styles_preserve_configuration_values() {
        let input = "mutations:\n  classic: {enabled: true, operators: {BOOLEAN_LITERAL_FLIP: false}}\nrules:\n  include: ['lib/**.dart']\n  methods: [calculate, 'yes', \"01\"]\noutput: |\n  literal %TAG text\n";
        assert_eq!(
            parse_yaml(input).unwrap(),
            json!({
                "mutations": {"classic": {"enabled": true, "operators": {"BOOLEAN_LITERAL_FLIP": false}}},
                "rules": {"include": ["lib/**.dart"], "methods": ["calculate", "yes", "01"]},
                "output": "literal %TAG text\n"
            })
        );
    }

    #[test]
    fn author_preamble_retains_tag_handles_without_weakening_profile_policy() {
        let input = "%TAG ! tag:yaml.org,2002:\n---\nvalue: !str true\n";
        assert_eq!(prepare_author_preamble(input).unwrap(), input);
        assert!(
            prepare_preamble(input)
                .unwrap_err()
                .to_string()
                .contains(TAG_ERROR)
        );
        assert!(
            parse_yaml(input)
                .unwrap_err()
                .to_string()
                .contains(TAG_ERROR)
        );
        let input =
            "%unknown ignored\n%TAG ! tag:yaml.org,2002:\n%YAML 1.3\n---\nvalue: !str true\n";
        let prepared = prepare_author_preamble(input).unwrap();
        assert!(prepared.contains("%TAG ! tag:yaml.org,2002:"));
        assert!(prepared.contains("%YAML 1.1"));
        assert!(prepared.starts_with("#unknown ignored"));
    }

    #[test]
    fn empty_null_and_lowercase_boolean_types_are_distinct_from_quoted_scalars() {
        assert_eq!(parse_yaml("# comment only\n").unwrap(), Value::Null);
        assert_eq!(parse_yaml("---\n...\n").unwrap(), Value::Null);
        assert_eq!(
            parse_yaml("[null, Null, NULL, ~, true, false, '', 'true', \"null\"]").unwrap(),
            json!([null, null, null, null, true, false, "", "true", "null"])
        );
        assert_eq!(parse_yaml("missing:\n").unwrap(), json!({"missing": null}));
    }

    #[test]
    fn yaml_11_boolean_aliases_are_rejected_even_in_string_values() {
        for value in [
            "yes", "Yes", "YES", "no", "No", "NO", "on", "On", "ON", "off", "Off", "OFF", "True",
            "TRUE", "False", "FALSE",
        ] {
            error(&format!("output: {value}"), "boolean");
            assert_eq!(
                parse_yaml(&format!("output: '{value}'")).unwrap(),
                json!({"output": value})
            );
        }
        for value in ["y", "Y", "n", "N", "TrUe", "falsey"] {
            assert_eq!(
                parse_yaml(&format!("output: {value}")).unwrap(),
                json!({"output": value})
            );
        }
    }

    #[test]
    fn integer_bases_separators_signs_and_sexagesimal_retain_numeric_types() {
        for (text, expected) in [
            ("0", 0),
            ("-0", 0),
            ("+12", 12),
            ("012", 10),
            ("0b1_01", 5),
            ("-0xF_f", -255),
            ("1__2_", 12),
            ("1:20", 80),
            ("-2:03:04", -7384),
        ] {
            assert_eq!(parse_yaml(text).unwrap(), json!(expected), "{text}");
        }
        assert_eq!(parse_yaml("18446744073709551615").unwrap(), json!(u64::MAX));
        assert_eq!(parse_yaml("-9223372036854775808").unwrap(), json!(i64::MIN));
        for text in [
            "08", "0o12", "01:10", "0:10", "1:60", "0x", "0b2", "1e3", "1e+3", "1.0e3",
        ] {
            assert_eq!(parse_yaml(text).unwrap(), json!(text), "{text}");
        }
    }

    #[test]
    fn floats_stay_floats_and_never_become_integer_settings() {
        for (text, expected) in [
            ("1.0", 1.0),
            (".5", 0.5),
            ("-1.", -1.0),
            ("1_2.5_0", 12.5),
            ("1.0e+3", 1000.0),
            ("1:20.5", 80.5),
            ("-2:03.0", -123.0),
        ] {
            let value = parse_yaml(text).unwrap();
            assert!(value.as_number().unwrap().is_f64(), "{text}");
            assert_eq!(value.as_f64(), Some(expected), "{text}");
        }
        for text in [
            ".inf",
            "-.Inf",
            "+.INF",
            ".nan",
            ".NaN",
            ".NAN",
            "1.0e+9999",
        ] {
            assert!(parse_yaml(text).is_err(), "{text}");
        }
    }

    #[test]
    fn numeric_overflow_and_malformed_resolved_numbers_are_errors_without_panics() {
        for text in [
            "18446744073709551616",
            "-9223372036854775809",
            "999999999999999999:59",
            "0b_",
            "0x_",
            "18446744073709551615:59",
        ] {
            assert!(parse_yaml(text).is_err(), "{text}");
        }
        error(&"9".repeat(MAX_YAML_SCALAR_CHARACTERS), "integer");
    }

    #[test]
    fn timestamps_are_not_silently_accepted_as_configuration_paths() {
        for text in [
            "2001-12-15",
            "2001-12-15T02:59:43.1Z",
            "2001-1-2 3:04:05 -07:30",
            "2001-99-99",
        ] {
            error(&format!("output: {text}"), "timestamp");
            assert_eq!(
                parse_yaml(&format!("output: '{text}'")).unwrap(),
                json!({"output": text})
            );
        }
        assert_eq!(
            parse_yaml("output: 2001-1-2").unwrap(),
            json!({"output": "2001-1-2"})
        );
    }

    #[test]
    fn duplicate_keys_are_detected_after_scalar_decoding_at_every_level() {
        for text in [
            "workers: 1\nworkers: 2",
            "rules: {methods: [], methods: [x]}",
            "mutations: {classic: {enabled: false, enabled: true}}",
            "a: 1\n\"\\u0061\": 2",
            "'same': 1\n\"same\": 2",
        ] {
            error(text, "duplicate");
        }
    }

    #[test]
    fn mapping_keys_must_be_strings_without_numeric_null_or_container_coercion() {
        for key in [
            "1", "1.0", "0x10", "012", "true", "false", "null", "~", "[a, b]", "{x: y}",
        ] {
            error(&format!("? {key}\n: value\n"), "keys");
        }
        assert_eq!(
            parse_yaml("'1': value\n'null': value\n'<<': value").unwrap(),
            json!({"1": "value", "null": "value", "<<": "value"})
        );
    }

    #[test]
    fn anchors_aliases_tags_and_unused_tag_directives_are_forbidden() {
        for text in [
            "a: &unused value",
            "a: &unused []",
            "a: &unused {}",
            "[&x value, *x]",
            "a: *missing",
            "a: !!str true",
            "a: !custom value",
            "!!map {}",
            "!!seq []",
            "%TAG !custom! tag:example.com,2026:\n---\na: value\n",
        ] {
            assert!(parse_yaml(text).is_err(), "{text}");
        }
        error(
            "%TAG !custom! tag:example.com,2026:\n---\na: value\n",
            "tags",
        );
    }

    #[test]
    fn tag_directive_text_inside_literals_and_comments_is_plain_content() {
        assert_eq!(
            parse_yaml("a: '%TAG !custom! tag:example.com,2026:'\n# %TAG !x! y\n").unwrap(),
            json!({"a": "%TAG !custom! tag:example.com,2026:"})
        );
        assert_eq!(
            parse_yaml("a: |\n  %TAG !custom! tag:example.com,2026:\n").unwrap(),
            json!({"a": "%TAG !custom! tag:example.com,2026:\n"})
        );
        error("a: <<", "implicit");
        error("a: =", "implicit");
    }

    #[test]
    fn version_and_unknown_directives_do_not_bypass_the_strict_scalar_rules() {
        assert_eq!(
            parse_yaml("%UNKNOWN ignored\n---\na: value\n").unwrap(),
            json!({"a": "value"})
        );
        assert_eq!(
            parse_yaml("%YAML 1.2\n---\na: 012").unwrap(),
            json!({"a": 10})
        );
        error("%YAML 1.2\n---\na: yes", "boolean");
        error("%YAML 2.0\n---\na: value", "invalid YAML");
    }

    #[test]
    fn supported_major_versions_keep_yaml_11_types_for_other_minor_versions() {
        for version in ["1.0", "1.9", "1.123", "01.009"] {
            assert_eq!(
                parse_yaml(&format!("%YAML {version}\n---\na: 012\n")).unwrap(),
                json!({"a": 10})
            );
        }
        for source in [
            "%UNKNOWN ignored\na: 1\n",
            "%UNKNOWN ignored\n# comment only\n",
            "%YAML 1.9\na: 1\n",
            "%YAML 2.0\n---\na: 1\n",
            "%YAML 1.9extra\n---\na: 1\n",
            "%INVALID: directive\n---\na: 1\n",
            "%YAML 1.9\n%YAML 1.1\n---\na: 1\n",
            "%UNKNOWN ignored\n%TAG !x! tag:example.com,2026:\n---\na: 1\n",
        ] {
            assert!(parse_yaml(source).is_err(), "{source:?}");
        }
    }

    #[test]
    fn directive_marks_remain_byte_accurate_after_bom_unicode_comments_and_line_breaks() {
        let source =
            "\u{feff}# 🦀 café\r\n%UNKNOWN ignored\u{2028}%YAML 1.9\u{85}---\u{2029}a: value\n";
        assert_eq!(parse_yaml(source).unwrap(), json!({"a": "value"}));
        assert_eq!(parse_yaml("！: value\n").unwrap(), json!({"！": "value"}));
    }

    #[test]
    fn leading_bom_does_not_shift_the_first_block_mapping_key() {
        // Windows PowerShell 5.1 and several editors write UTF-8 with a BOM.
        for (source, expected) in [
            ("\u{feff}a: 1\nb: 2\n", json!({"a": 1, "b": 2})),
            (
                "\u{feff}a: 1\r\nb:\r\n  c: x\r\n",
                json!({"a": 1, "b": {"c": "x"}}),
            ),
            ("\u{feff}- a\n- b\n", json!(["a", "b"])),
            ("\u{feff}---\na: 1\nb: 2\n", json!({"a": 1, "b": 2})),
            (
                "\u{feff}%YAML 1.9\n---\na: 1\nb: 2\n",
                json!({"a": 1, "b": 2}),
            ),
        ] {
            assert_eq!(parse_yaml(source).unwrap(), expected, "{source:?}");
        }
        // Only one leading byte-order mark is an encoding signature.
        assert!(parse_yaml("\u{feff}\u{feff}a: 1\nb: 2\n").is_err());
        assert!(parse_yaml("a: 1\n\u{feff}b: 2\n").is_err());
    }

    #[test]
    fn preamble_compatibility_never_rewrites_quoted_or_block_body_text() {
        let body =
            "a: |\n  %UNKNOWN literal\nb: \"%YAML 1.9\"\nc: 'first\n  %UNKNOWN quoted\n  last'\n";
        let source = format!("%UNKNOWN ignored\n---\n{body}");
        let prepared = prepare_preamble(&source).unwrap();
        assert!(prepared.ends_with(body));
        assert_eq!(
            parse_yaml(&source).unwrap(),
            json!({
                "a": "%UNKNOWN literal\n", "b": "%YAML 1.9", "c": "first %UNKNOWN quoted last"
            })
        );
        for source in [
            "a: 1\n%UNKNOWN trailing\n",
            "a: 1\n%UNKNOWN between\nb: 2\n",
        ] {
            assert!(parse_yaml(source).is_err());
        }
    }

    #[test]
    fn ten_thousand_ignored_directives_have_linear_bounded_input_reads() {
        use std::cell::Cell;
        use std::io::Read;
        use std::rc::Rc;

        struct Metered<'a> {
            input: &'a [u8],
            bytes: Rc<Cell<usize>>,
        }
        impl Read for Metered<'_> {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = self.input.read(output)?;
                self.bytes.set(self.bytes.get() + count);
                Ok(count)
            }
        }

        let count = 10_000;
        let source = format!("{}---\na: true\n", "%UNKNOWN ignored\n".repeat(count));
        let bytes = Rc::new(Cell::new(0));
        let mut suffix_lengths = Vec::new();
        let prepared = prepare_preamble_with(&source, false, |input| {
            suffix_lengths.push(input.len());
            BufReader::with_capacity(
                PREAMBLE_BUFFER_BYTES,
                Metered {
                    input,
                    bytes: Rc::clone(&bytes),
                },
            )
        })
        .unwrap();
        assert_eq!(suffix_lengths.len(), count + 1);
        assert!(suffix_lengths.windows(2).all(|pair| pair[1] < pair[0]));
        // Count actual bytes read, including bounded lookahead re-read after a
        // reset. Whole-suffix replays would exceed this linear bound greatly.
        assert!(bytes.get() <= source.len() + PREAMBLE_BUFFER_BYTES * (count + 1));
        assert_eq!(prepared.len(), source.len());
        assert_eq!(parse_yaml(&prepared).unwrap(), json!({"a": true}));
    }

    #[test]
    fn additional_documents_and_malformed_syntax_are_never_ignored() {
        for text in [
            "---\na: x\n---\na: y",
            "a: x\n...\n---\n",
            "---\n---\n",
            "a: [",
            "a: {b: c",
            "a: \"unterminated",
            "a: \"\\q\"",
            "a:\n\tb: c",
            "a: x\0",
        ] {
            assert!(parse_yaml(text).is_err(), "{text:?}");
        }
        for text in ["a: \"\\uD800\"", "a: \"\\uDFFF\"", "a: \"\\U00110000\""] {
            assert!(parse_yaml(text).is_err(), "{text}");
        }
    }

    #[test]
    fn raw_nonprintable_characters_cannot_hide_content_or_documents() {
        for character in [
            '\0', '\u{1}', '\u{8}', '\u{b}', '\u{c}', '\u{1f}', '\u{7f}', '\u{84}', '\u{86}',
            '\u{9f}', '\u{fffe}', '\u{ffff}',
        ] {
            error(
                &format!("a: value{character}\n---\nb: value"),
                "invalid YAML",
            );
            error(&format!("a: value\n# ignored{character}"), "invalid YAML");
        }
        assert_eq!(parse_yaml("a: \"\\0\"").unwrap(), json!({"a": "\0"}));
    }

    #[test]
    fn literal_nel_has_yaml_11_line_break_semantics_in_every_scalar_style() {
        assert_eq!(
            parse_yaml("a:\u{85}  b: true\u{85}").unwrap(),
            json!({"a": {"b": true}})
        );
        assert_eq!(
            parse_yaml("# comment\u{85}a: true").unwrap(),
            json!({"a": true})
        );
        for source in [
            "a: \"folder\u{85}  marker\"",
            "a: 'folder\u{85}  marker'",
            "a: folder\u{85}  marker",
            "a: >-\n  folder\u{85}  marker\n",
        ] {
            assert_eq!(parse_yaml(source).unwrap(), json!({"a": "folder marker"}));
        }
        assert_eq!(
            parse_yaml("a: |-\n  folder\u{85}  marker\n").unwrap(),
            json!({"a": "folder\nmarker"})
        );
        assert_eq!(
            parse_yaml("a: \"folder\\Nmarker\"").unwrap(),
            json!({"a": "folder\u{85}marker"})
        );
        assert_eq!(
            parse_yaml("a: \"folder\\u0085marker\"").unwrap(),
            json!({"a": "folder\u{85}marker"})
        );
    }

    #[test]
    fn nel_conversion_cannot_shrink_an_oversized_original_document_past_the_byte_cap() {
        let original = format!("{{}}\n#{}", "\u{85}".repeat(MAX_YAML_BYTES / 2));
        assert!(original.len() > MAX_YAML_BYTES);
        assert!(original.replace('\u{85}', "\n").len() < MAX_YAML_BYTES);
        error(&original, "1 MiB");
    }

    #[test]
    fn escaped_and_unindented_quoted_unicode_separators_keep_their_scalar_values() {
        for character in ['\u{2028}', '\u{2029}'] {
            assert_eq!(
                parse_yaml(&format!("a: \"folder{character}marker\"")).unwrap(),
                json!({"a": format!("folder{character}marker")})
            );
        }
        assert_eq!(
            parse_yaml("a: \"\\u2028\\u2029\"").unwrap(),
            json!({"a": "\u{2028}\u{2029}"})
        );
    }

    #[test]
    fn unicode_ls_ps_are_structural_line_breaks_without_changing_mapping_shape() {
        for separator in ['\u{2028}', '\u{2029}'] {
            let source = format!("mutations:{separator}  classic: {{enabled: true}}{separator}");
            assert_eq!(
                parse_yaml(&source).unwrap(),
                json!({"mutations": {"classic": {"enabled": true}}})
            );
        }
    }

    #[test]
    fn unicode_ls_ps_plain_continuations_strip_indentation_and_retain_the_separator() {
        for separator in ['\u{2028}', '\u{2029}'] {
            let source = format!("output: folder{separator}  marker\n");
            assert_eq!(
                parse_yaml(&source).unwrap(),
                json!({"output": format!("folder{separator}marker")})
            );
        }
    }

    #[test]
    fn unicode_ls_ps_literal_and_folded_blocks_strip_indent_without_changing_separators() {
        for separator in ['\u{2028}', '\u{2029}'] {
            for style in ['|', '>'] {
                let source = format!("output: {style}-\n  folder{separator}  marker\n");
                assert_eq!(
                    parse_yaml(&source).unwrap(),
                    json!({"output": format!("folder{separator}marker")})
                );
            }
        }
    }

    #[test]
    fn unicode_ls_ps_quoted_continuations_strip_indent_without_becoming_ascii_spaces() {
        for separator in ['\u{2028}', '\u{2029}'] {
            for quote in ['\'', '"'] {
                let source = format!("output: {quote}folder{separator}  marker{quote}\n");
                assert_eq!(
                    parse_yaml(&source).unwrap(),
                    json!({"output": format!("folder{separator}marker")})
                );
            }
        }
    }

    #[test]
    fn nesting_limit_accepts_exact_bound_and_stops_deep_input_before_construction() {
        let nested = |depth: usize| format!("{}x{}", "[".repeat(depth), "]".repeat(depth));
        assert!(parse_yaml(&nested(MAX_YAML_DEPTH)).is_ok());
        error(&nested(MAX_YAML_DEPTH + 1), "deeply");
        // A scanner can reject its own flow lookahead limit before delivering
        // the first container event. Either bounded rejection is safe here.
        assert!(parse_yaml(&nested(100_000)).is_err());
        let maps = |depth: usize| format!("{}x{}", "{a: ".repeat(depth), "}".repeat(depth));
        assert!(parse_yaml(&maps(MAX_YAML_DEPTH)).is_ok());
        error(&maps(MAX_YAML_DEPTH + 1), "deeply");
    }

    #[test]
    fn scalar_character_bound_counts_decoded_unicode_including_keys_and_block_styles() {
        let unicode = "🦀".repeat(MAX_YAML_SCALAR_CHARACTERS);
        assert_eq!(parse_yaml(&format!("'{unicode}'")).unwrap(), json!(unicode));
        error(&format!("'{}🦀'", unicode), "scalar");
        error(
            &format!(
                "? '{}'\n: value",
                "a".repeat(MAX_YAML_SCALAR_CHARACTERS + 1)
            ),
            "scalar",
        );
        assert!(parse_yaml(&format!("a: |-\n  {unicode}\n")).is_ok());
        error(&format!("a: |\n  {unicode}\n"), "scalar");
        assert_eq!(
            parse_yaml("a: \"\\U0001f980\\u00e9\"").unwrap(),
            json!({"a": "🦀é"})
        );
    }

    #[test]
    fn byte_limit_includes_comments_and_accepts_exactly_one_mebibyte() {
        let prefix = "{}\n#";
        let exact = format!("{prefix}{}", "x".repeat(MAX_YAML_BYTES - prefix.len()));
        assert_eq!(parse_yaml(&exact).unwrap(), json!({}));
        error(&(exact + "x"), "1 MiB");
        error(&"🦀".repeat(MAX_YAML_BYTES / 4 + 1), "1 MiB");
    }
}

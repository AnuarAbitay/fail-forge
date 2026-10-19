use super::*;

const OWN: &[u8] = b"package:subject/";
const ALIAS: &[u8] = b"package:failforge_subject/";

fn scan(raw: impl AsRef<[u8]>) -> Scanner {
    Scanner::new(raw.as_ref(), &Cancellation::default()).unwrap()
}

fn headers(scanner: &Scanner) -> Vec<&[u8]> {
    scanner
        .header_uris(&Cancellation::default())
        .unwrap()
        .iter()
        .map(|token| scanner.value(token))
        .collect()
}

fn allowed_headers(scanner: &Scanner) -> BTreeSet<usize> {
    scanner
        .header_uris(&Cancellation::default())
        .unwrap()
        .iter()
        .filter(|token| scanner.value(token).starts_with(OWN))
        .map(|token| token.content_start)
        .collect()
}

fn validate(scanner: &Scanner) -> Result<(), NamespaceError> {
    check_uris(
        scanner,
        OWN,
        ALIAS,
        &allowed_headers(scanner),
        &Cancellation::default(),
    )
}

fn error_contains<T: std::fmt::Debug>(result: Result<T, NamespaceError>, expected: &str) {
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(expected),
        "expected {expected:?}, got {error:?}"
    );
}

#[test]
fn ranges_preserve_bom_crlf_unicode_and_distinguish_quotes_from_uri_bytes() {
    let raw = "\u{feff}// 😀 unchanged\r\nimport r'package:subject/пример.dart';\r\n".as_bytes();
    let scanner = scan(raw);
    assert_eq!(scanner.raw(), raw);
    let uris = scanner.header_uris(&Cancellation::default()).unwrap();
    let uri = uris[0];
    assert_eq!(uri.kind, TokenKind::String);
    assert_eq!(
        uri.start,
        raw.windows(2).position(|bytes| bytes == b"r'").unwrap()
    );
    assert_eq!(uri.content_start, uri.start + 2);
    assert_eq!(uri.content_end, uri.end - 1);
    assert!(uri.raw_string);
    assert!(!uri.interpolated);
    assert_eq!(scanner.value(uri), "package:subject/пример.dart".as_bytes());
    assert_eq!(
        scanner.references(OWN, &Cancellation::default()).unwrap(),
        [uri.content_start]
    );
    validate(&scanner).unwrap();
}

#[test]
fn annotations_library_and_conditional_directives_select_only_uri_tokens() {
    let scanner = scan(
        b"@A.named<List<int>>(')')\nlibrary author;\n\
        import 'one.dart' if (dart.library.io == 'true') 'two.dart' as impl;\n\
        export r'three.dart' hide X;\npart 'four.dart';\n",
    );
    assert_eq!(
        headers(&scanner),
        [
            b"one.dart".as_slice(),
            b"two.dart",
            b"three.dart",
            b"four.dart"
        ]
    );
    let scanner = scan(b"@TestOn('vm')\r\nlibrary;\r\n\
        import /* keep */ 'package:subject/api.dart' as impl show read;\r\n\
        import 'package:subject/types.dart' if (dart.library.io == 'true')\r\n\
        'package:subject/io.dart' if (dart.library.html) 'package:subject/web.dart' deferred as platform;\r\n");
    assert_eq!(headers(&scanner).len(), 4);
    validate(&scanner).unwrap();
}

#[test]
fn named_and_uri_part_of_and_annotated_declarations_have_separate_boundaries() {
    for raw in [
        b"library names.here; part of names.here;".as_slice(),
        b"library; @A() class C {}",
    ] {
        assert!(headers(&scan(raw)).is_empty());
    }
    assert_eq!(
        headers(&scan(b"part of 'package:subject/a.dart';")),
        [b"package:subject/a.dart".as_slice()]
    );
    let scanner = scan(b"@A.named<List<Map<int, String>>>(f('(', ')')) import 'a.dart'; @B() void main(){} import 'b.dart';");
    assert_eq!(headers(&scanner), [b"a.dart".as_slice()]);
}

#[test]
fn all_six_ascii_whitespace_bytes_preserve_header_detection() {
    for byte in *b" \t\n\r\x0b\x0c" {
        let raw = [
            vec![byte],
            b"import".to_vec(),
            vec![byte],
            b"'package:subject/a';".to_vec(),
        ]
        .concat();
        let scanner = scan(raw);
        assert_eq!(headers(&scanner), [b"package:subject/a".as_slice()]);
        validate(&scanner).unwrap();
    }
}

#[test]
fn declaration_ends_header_and_late_import_is_rejected_as_unproven_reference() {
    let scanner = scan(
        b"import 'package:subject/first.dart'; void main(){} import 'package:subject/late.dart';",
    );
    assert_eq!(
        headers(&scanner),
        [b"package:subject/first.dart".as_slice()]
    );
    error_contains(
        validate(&scanner),
        "unsupported author test package reference",
    );
}

#[test]
fn comments_and_shebangs_hide_only_their_actual_ranges() {
    for prefix in [b"".as_slice(), b"\xef\xbb\xbf"] {
        let raw = [prefix, b"#!/bin/dart package:subject/hidden\r\n/* package:subject/outer /* nested */ end */\nimport 'package:subject/a.dart';//package:failforge_subject/hidden"].concat();
        let scanner = scan(&raw);
        assert_eq!(headers(&scanner), [b"package:subject/a.dart".as_slice()]);
        assert_eq!(
            scanner
                .references(OWN, &Cancellation::default())
                .unwrap()
                .len(),
            1
        );
        assert!(
            scanner
                .references(ALIAS, &Cancellation::default())
                .unwrap()
                .is_empty()
        );
        validate(&scanner).unwrap();
    }
    let scanner = scan(b"void main() {} #! package:subject/visible\n");
    assert_eq!(
        scanner
            .references(OWN, &Cancellation::default())
            .unwrap()
            .len(),
        1
    );
    error_contains(
        validate(&scanner),
        "unsupported author test package reference",
    );
}

#[test]
fn nested_interpolation_exposes_inner_literals_identifiers_and_comments() {
    let raw = br#"void main(){print('${/* package:subject/comment */ Isolate.resolvePackageUriSync(Uri.parse("package:subject/a.dart"))}');}"#;
    let scanner = scan(raw);
    assert_eq!(scanner.strings().len(), 2);
    let inner = &scanner.strings()[0];
    let outer = &scanner.strings()[1];
    assert_eq!(scanner.value(inner), b"package:subject/a.dart");
    assert!(!inner.interpolated);
    assert!(outer.interpolated);
    assert!(outer.content_start < inner.content_start && inner.content_end < outer.content_end);
    assert!(
        scanner
            .code_identifiers()
            .contains(b"resolvePackageUriSync".as_slice())
    );
    assert!(headers(&scanner).is_empty());
    assert_eq!(
        scanner.references(OWN, &Cancellation::default()).unwrap(),
        [inner.content_start]
    );
    check_uris(
        &scanner,
        OWN,
        ALIAS,
        &BTreeSet::from([inner.content_start]),
        &Cancellation::default(),
    )
    .unwrap();
}

#[test]
fn resolver_text_in_comments_literals_or_short_interpolation_is_not_a_code_identifier() {
    let scanner = scan(
        br#"// resolvePackageUri('x')
        const s='resolvePackageUriSync'; const t='$resolvePackageUri';
        const r=r'${resolvePackageUriSync}';"#,
    );
    assert!(
        !scanner
            .code_identifiers()
            .contains(b"resolvePackageUri".as_slice())
    );
    assert!(
        !scanner
            .code_identifiers()
            .contains(b"resolvePackageUriSync".as_slice())
    );
    let scanner = scan(br#"const x='${{'k': '${resolvePackageUri(Uri())}'}}';"#);
    assert!(
        scanner
            .code_identifiers()
            .contains(b"resolvePackageUri".as_slice())
    );
    assert_eq!(
        scanner
            .strings()
            .iter()
            .filter(|token| token.interpolated)
            .count(),
        2
    );
}

#[test]
fn raw_triple_and_escaped_quotes_preserve_string_boundaries() {
    let scanner = scan(br#"r'''${resolvePackageUri} \''' ; """a'"b\"c ${fn("inner")}""";"#);
    assert_eq!(scanner.strings().len(), 3);
    let raw = &scanner.strings()[0];
    assert!(raw.raw_string && !raw.interpolated);
    assert_eq!(scanner.value(raw), br#"${resolvePackageUri} \"#);
    assert_eq!(scanner.value(&scanner.strings()[1]), b"inner");
    assert!(scanner.strings()[2].interpolated);
    assert!(
        !scanner
            .code_identifiers()
            .contains(b"resolvePackageUri".as_slice())
    );
    assert!(scanner.code_identifiers().contains(b"fn".as_slice()));
}

#[test]
fn import_looking_text_inside_literal_cannot_be_promoted_to_a_header() {
    let scanner = scan(
        br#"import 'package:subject/a.dart'; void main(){ const s='''
        import 'package:subject/b.dart';
        ''';}"#,
    );
    assert_eq!(headers(&scanner).len(), 1);
    error_contains(
        validate(&scanner),
        "unsupported author test package reference",
    );
}

#[test]
fn every_author_reference_requires_its_exact_allowed_byte_offset() {
    let scanner = scan(b"import 'package:subject/a.dart'; const s='package:subject/b.dart';");
    let references = scanner.references(OWN, &Cancellation::default()).unwrap();
    assert_eq!(references.len(), 2);
    error_contains(
        validate(&scanner),
        "unsupported author test package reference",
    );
    check_uris(
        &scanner,
        OWN,
        ALIAS,
        &references.iter().copied().collect(),
        &Cancellation::default(),
    )
    .unwrap();
    let wrong = references.iter().map(|value| value + 1).collect();
    error_contains(
        check_uris(&scanner, OWN, ALIAS, &wrong, &Cancellation::default()),
        "unsupported author test package reference",
    );
}

#[test]
fn encoded_noncanonical_and_interpolated_header_uris_fail_closed() {
    for uri in [
        r"package:%73ubject/a.dart",
        r"package:sub\u006aect/a.dart",
        r"PACKAGE:subject/a.dart",
        r"package:subject/$file.dart",
        r"other:%20file.dart",
        r"other:\x61.dart",
    ] {
        let scanner = scan(format!("import '{uri}';"));
        assert!(validate(&scanner).is_err(), "accepted {uri:?}");
    }
}

#[test]
fn hidden_runtime_references_fail_even_when_no_raw_prefix_exists() {
    for value in [
        r"package:%73ubject/a.dart",
        r"%70ackage:subject/a.dart",
        r"package:subject%2Fa.dart",
        r"package:sub\u006aect/a.dart",
        r"\x70ackage:subject/a.dart",
        r"\u{70}ackage:subject/a.dart",
        r"PACKAGE:subject/a.dart",
        "pacKage:subject/a.dart",
        r"package:subject/a.dart%20",
        r"package:failforge_%73ubject/a.dart",
        r"PACKAGE:failforge_subject/a.dart",
    ] {
        let scanner = scan(format!("const s='{value}';"));
        error_contains(
            validate(&scanner),
            if value.contains("failforge") {
                "already uses subject alias"
            } else {
                "unsupported author test package reference"
            },
        );
    }
}

#[test]
fn existing_aliases_are_rejected_in_code_or_literal_but_not_comments() {
    for raw in [
        b"import 'package:failforge_subject/a.dart';".as_slice(),
        b"const s='package:failforge_subject/a.dart';",
        b"package:failforge_subject/a",
    ] {
        error_contains(validate(&scan(raw)), "already uses subject alias");
    }
    validate(&scan(
        b"/* package:failforge_subject/a */ // package:subject/b\nconst s='other';",
    ))
    .unwrap();
}

#[test]
fn literal_decoder_matches_dart_escape_shapes_without_changing_source_bytes() {
    let scanner = scan(br#"'\n\r\t\b\f\v\\\'\"\$\u0061\u{1F600}\x62\q'; r'\u0061'; '$value';"#);
    let original = scanner.raw().to_vec();
    assert_eq!(
        literal_value(&scanner, &scanner.strings()[0], &Cancellation::default())
            .unwrap()
            .unwrap(),
        "\n\r\t\u{8}\u{c}\u{b}\\'\"$a😀bq"
    );
    assert_eq!(
        literal_value(&scanner, &scanner.strings()[1], &Cancellation::default())
            .unwrap()
            .unwrap(),
        r"\u0061"
    );
    assert_eq!(
        literal_value(&scanner, &scanner.strings()[2], &Cancellation::default()).unwrap(),
        None
    );
    assert_eq!(scanner.raw(), original);
}

#[test]
fn malformed_unicode_escapes_are_rejected_and_surrogate_escapes_do_not_panic() {
    for source in [
        br"'\u{}'".as_slice(),
        br"'\u{1234567}'",
        br"'\uZZZZ'",
        br"'\xzz'",
        br"'\u'",
        br"'\x'",
    ] {
        error_contains(validate(&scan(source)), "invalid Dart Unicode escape");
    }
    let scanner = scan(br"'\uD800 \u{DFFF}';");
    assert_eq!(
        literal_value(&scanner, &scanner.strings()[0], &Cancellation::default())
            .unwrap()
            .unwrap(),
        "\u{fffd} \u{fffd}"
    );
    validate(&scanner).unwrap();
    let scanner = scan(br"'\u{110000}';");
    error_contains(validate(&scanner), "Unicode range");
}

#[test]
fn percent_decoding_matches_utf8_lossy_rules_without_treating_plus_as_space() {
    for (source, expected) in [
        ("%70ackage:%73ubject%2Fa.dart", "package:subject/a.dart"),
        ("пример%20%F0%9F%98%80+%ZZ%2", "пример 😀+%ZZ%2"),
        ("%FF%C3%28", "\u{fffd}\u{fffd}("),
        ("%e2%82", "\u{fffd}"),
    ] {
        assert_eq!(
            percent_decode(source, &Cancellation::default()).unwrap(),
            expected
        );
    }
}

#[test]
fn malformed_source_is_rejected_without_panics() {
    error_contains(Scanner::new(b"\xff", &Cancellation::default()), "not UTF-8");
    for (source, error) in [
        ("/* never closed", "unterminated header comment"),
        ("'never closed", "unterminated Dart string literal"),
        ("r'''never closed", "unterminated Dart string literal"),
        ("'ends with \\", "unterminated Dart string literal"),
        ("'${value", "unterminated Dart interpolation"),
        ("'${{'a': 1}", "unterminated Dart interpolation"),
    ] {
        error_contains(
            Scanner::new(source.as_bytes(), &Cancellation::default()),
            error,
        );
    }
}

#[test]
fn malformed_header_directives_fail_before_any_range_can_be_applied() {
    for (source, error) in [
        ("@", "unsupported header annotation"),
        ("@A.", "unsupported header annotation"),
        ("@A(1", "unterminated header annotation or condition"),
        ("@A<T", "unterminated header annotation or condition"),
        (
            "@A '<' > import 'a';",
            "unterminated header annotation or condition",
        ),
        (
            "import package:subject/a.dart;",
            "unsupported header URI expression",
        ),
        ("import 'a' 'b';", "unsupported header directive string"),
        ("import 'a' if x 'b';", "unsupported conditional URI"),
        ("import 'a' if (x) value;", "unsupported conditional URI"),
        (
            "import 'a' if (x",
            "unterminated header annotation or condition",
        ),
        ("import 'a'", "unterminated header directive"),
        ("part", "unterminated header directive"),
    ] {
        error_contains(scan(source).header_uris(&Cancellation::default()), error);
    }
}

#[test]
fn nested_comments_and_header_brackets_are_iterative() {
    let raw = format!(
        "{} ignored {} import 'a';",
        "/*".repeat(10_000),
        "*/".repeat(10_000)
    );
    assert_eq!(headers(&scan(raw)), [b"a".as_slice()]);
    let raw = format!(
        "@A{}x{} import 'a';",
        "<".repeat(10_000),
        ">".repeat(10_000)
    );
    assert_eq!(headers(&scan(raw)), [b"a".as_slice()]);
}

#[test]
fn recursive_interpolation_is_bounded_before_stack_exhaustion() {
    fn nested(count: usize) -> String {
        format!("{}value{}", "'${".repeat(count), "}'".repeat(count))
    }
    let scanner = scan(nested(MAX_NESTING));
    assert_eq!(scanner.strings().len(), MAX_NESTING);
    error_contains(
        Scanner::new(nested(MAX_NESTING + 1).as_bytes(), &Cancellation::default()),
        "nesting limit",
    );
}

#[test]
fn source_and_item_budgets_reject_oversized_inputs() {
    error_contains(
        Scanner::new(&vec![b' '; MAX_SOURCE_BYTES + 1], &Cancellation::default()),
        "exceeds eight MiB",
    );
    error_contains(
        Scanner::new(&vec![b';'; MAX_ITEMS + 1], &Cancellation::default()),
        "item limit",
    );
}

#[test]
fn item_count_includes_nested_tokens_and_comments_for_the_campaign_budget() {
    assert_eq!(scan(b"  \r\n ").item_count(), 0);
    assert_eq!(scan(b"first second; /* comment */").item_count(), 4);
    // The outer literal, nested identifier, and closing interpolation token all
    // consume the same budget even though only the outer token is retained in
    // the top-level directive stream.
    assert_eq!(scan(b"'${inner}' // comment").item_count(), 4);
}

#[test]
fn cancellation_is_observed_by_every_public_scanner_entry_point() {
    let scanner = scan(b"import 'package:subject/a.dart';");
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    for result in [
        Scanner::new(scanner.raw(), &cancel).map(|_| ()),
        scanner.header_uris(&cancel).map(|_| ()),
        scanner.references(OWN, &cancel).map(|_| ()),
        check_uris(&scanner, OWN, ALIAS, &BTreeSet::new(), &cancel),
    ] {
        assert!(matches!(
            result,
            Err(NamespaceError::Cancelled(libc::SIGTERM))
        ));
    }
}

#[test]
fn empty_or_non_utf8_search_prefix_is_rejected_without_looping() {
    let scanner = scan(b"'content';");
    error_contains(
        scanner.references(b"", &Cancellation::default()),
        "empty namespace prefix",
    );
    error_contains(
        scanner.references(b"\xff", &Cancellation::default()),
        "prefix is not UTF-8",
    );
}

/// Frozen against scripts/dart_namespace.py at e6e0c49c. The JSON records the
/// exact baseline commit/hash and callable sequence; it has no Python runtime
/// dependency. Sources cover accepted and rejected scanner shapes independently
/// of the filesystem transaction and the Dart analyzer's resolved-AST plan.
#[test]
fn committed_python_oracle_matches_ranges_identifiers_references_and_decisions() {
    use serde_json::{Value, json};

    let oracle: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/namespace_scanner.json")).unwrap();
    assert_eq!(
        oracle["provenance"]["sha256"],
        "64bdbfcb9aa3dce98a4ee19f5191e5fc0f95819a22313f760c1d66240d56e52c"
    );
    assert_eq!(oracle["cases"].as_array().unwrap().len(), 72);
    fn token(scanner: &Scanner, token: &Token) -> Value {
        json!({
            "kind": match token.kind { TokenKind::Identifier => "identifier", TokenKind::Symbol => "symbol", TokenKind::String => "string" },
            "start": token.start, "end": token.end,
            "contentStart": token.content_start, "contentEnd": token.content_end,
            "value": std::str::from_utf8(scanner.value(token)).unwrap(),
            "rawString": token.raw_string, "interpolated": token.interpolated,
        })
    }
    let cancel = Cancellation::default();
    for row in oracle["cases"].as_array().unwrap() {
        let name = row["name"].as_str().unwrap();
        let source = row["source"].as_str().unwrap();
        let scanned = Scanner::new(source.as_bytes(), &cancel);
        if let Some(expected) = row.get("scanError") {
            assert_eq!(
                scanned.unwrap_err().to_string(),
                expected.as_str().unwrap(),
                "{name}"
            );
            continue;
        }
        let scanner = scanned.unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_eq!(
            json!(
                scanner
                    .tokens
                    .iter()
                    .map(|value| token(&scanner, value))
                    .collect::<Vec<_>>()
            ),
            row["tokens"],
            "tokens: {name}"
        );
        assert_eq!(
            json!(
                scanner
                    .strings()
                    .iter()
                    .map(|value| token(&scanner, value))
                    .collect::<Vec<_>>()
            ),
            row["strings"],
            "strings: {name}"
        );
        assert_eq!(
            json!(
                scanner
                    .code_identifiers()
                    .iter()
                    .map(|value| std::str::from_utf8(value).unwrap())
                    .collect::<Vec<_>>()
            ),
            row["identifiers"],
            "identifiers: {name}"
        );
        assert_eq!(
            json!(scanner.references(OWN, &cancel).unwrap()),
            row["ownReferences"],
            "own references: {name}"
        );
        assert_eq!(
            json!(scanner.references(ALIAS, &cancel).unwrap()),
            row["aliasReferences"],
            "alias references: {name}"
        );
        let headers = scanner.header_uris(&cancel);
        if let Some(expected) = row.get("headerError") {
            assert_eq!(
                headers.unwrap_err().to_string(),
                expected.as_str().unwrap(),
                "header error: {name}"
            );
            continue;
        }
        assert_eq!(
            json!(
                headers
                    .unwrap()
                    .iter()
                    .map(|value| token(&scanner, value))
                    .collect::<Vec<_>>()
            ),
            row["headers"],
            "headers: {name}"
        );
        // Malformed Unicode escapes remain rejected; replace the baseline's
        // incidental Python int() exception text with a Dart-specific reason.
        let expected = if row["checkError"] == "invalid literal for int() with base 16: ''" {
            json!("invalid Dart Unicode escape")
        } else {
            row["checkError"].clone()
        };
        assert_eq!(
            json!(validate(&scanner).err().map(|error| error.to_string())),
            expected,
            "decision: {name}"
        );
    }
}

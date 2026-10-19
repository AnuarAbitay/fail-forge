use super::*;
use std::ffi::CString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    options: GenerateOptions,
    context: Context,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "failforge preparation {} {}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("package/lib")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let project = root.join("package");
        fs::write(project.join("pubspec.yaml"), "name: example\n").unwrap();
        let source = project.join("lib/calculator.dart");
        fs::write(&source, "bool enabled() => true;\n").unwrap();
        let options = GenerateOptions {
            source,
            output: project.join("profiles/calculator.yml"),
            families: vec!["classic".into()],
            language: None,
            operators: Vec::new(),
            force: false,
            dart_sdk: None,
            flutter_sdk: None,
            adapter_root: None,
        };
        let context = Context::new(&project, Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        Self {
            root,
            project,
            options,
            context,
        }
    }

    fn prepare(&self) -> Result<ProfileDraft, ProfileError> {
        prepare_profile(&self.options, &self.context)
    }
    fn error(&self, text: &str) {
        let error = self.prepare().unwrap_err();
        assert!(
            error.to_string().contains(text),
            "expected {text:?}, got {error}"
        );
        assert!(
            !self
                .options
                .output
                .parent()
                .unwrap()
                .join(".temporary")
                .exists()
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn classic_defaults_roundtrip_through_native_loader_without_creating_directories() {
    let fixture = Fixture::new();
    let draft = fixture.prepare().unwrap();
    assert!(!fixture.options.output.parent().unwrap().exists());
    assert_eq!(
        draft.values["rules"],
        json!({"include": ["lib/calculator.dart"]})
    );
    assert_eq!(
        draft.values["timeouts"],
        json!({"test": "30s", "command": "120s"})
    );
    assert_eq!(draft.values["limits"], json!({"max_mutants": 1000}));
    let switches = draft.values["mutations"]["classic"]["operators"]
        .as_object()
        .unwrap();
    assert_eq!(switches.len(), 32);
    assert_eq!(switches.values().filter(|v| **v == true).count(), 31);
    assert_eq!(switches["CONDITIONAL_NEGATION"], false);
    fs::create_dir(draft.output.parent().unwrap()).unwrap();
    fs::write(&draft.output, &draft.text).unwrap();
    let failforge_config::RunConfig::Profile(profile) =
        failforge_config::load_yaml(&draft.output, &fixture.context).unwrap()
    else {
        panic!("expected profile");
    };
    assert_eq!(profile.campaigns.len(), 1);
    assert_eq!(profile.campaigns[0].project, fixture.project);
    assert_eq!(
        profile.campaigns[0].rules.include.as_ref().unwrap(),
        &["lib/calculator.dart"]
    );
    assert_eq!(
        profile.campaigns[0].rules.operators.as_ref().unwrap().len(),
        31
    );
}

#[test]
fn every_registered_operator_can_be_selected_without_implicitly_enabling_others() {
    let mut fixture = Fixture::new();
    fs::write(
        fixture.project.join("pubspec.yaml"),
        "name: example\ndev_dependencies:\n  flutter_test:\n    sdk: flutter\n",
    )
    .unwrap();
    for family in catalogue::FAMILIES {
        fixture.options.families = vec![(*family).into()];
        fixture.options.language = (*family == "language_specific").then(|| "dart".into());
        for operator in catalogue::operators(family).unwrap() {
            fixture.options.operators = vec![(*operator).into()];
            let draft = fixture.prepare().unwrap();
            let switches = draft.values["mutations"][*family]["operators"]
                .as_object()
                .unwrap();
            assert_eq!(switches.values().filter(|v| **v == true).count(), 1);
            assert_eq!(switches[*operator], true);
        }
    }
}

#[test]
fn output_family_and_operator_order_is_canonical() {
    let mut fixture = Fixture::new();
    fixture.options.families = vec![
        "language_specific".into(),
        "failure_mode".into(),
        "classic".into(),
    ];
    fixture.options.language = Some("dart".into());
    let first = fixture.prepare().unwrap();
    fixture.options.families.reverse();
    let second = fixture.prepare().unwrap();
    assert_eq!(first.text, second.text);
    let mut position = 0;
    for family in catalogue::FAMILIES {
        let next = first.text.find(&format!("  {family}:\n")).unwrap();
        assert!(next >= position);
        position = next;
        for operator in catalogue::operators(family).unwrap() {
            let next = first.text.find(&format!("      {operator}: ")).unwrap();
            assert!(next > position);
            position = next;
        }
    }
}

#[test]
fn explicit_global_selection_leaves_other_requested_families_disabled() {
    let mut fixture = Fixture::new();
    fixture.options.families.push("failure_mode".into());
    fixture.options.operators.push("BOOLEAN_NEGATION".into());
    let draft = fixture.prepare().unwrap();
    assert_eq!(draft.values["mutations"]["failure_mode"]["enabled"], true);
    assert!(
        draft.values["mutations"]["failure_mode"]["operators"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v == false)
    );
    assert!(draft.values["mutations"].get("language_specific").is_none());
}

#[test]
fn selection_rejects_unknown_duplicate_empty_wrong_family_and_language_values() {
    let mut fixture = Fixture::new();
    for families in [
        vec![],
        vec!["unknown"],
        vec!["classic", "classic"],
        vec![""],
    ] {
        fixture.options.families = families.into_iter().map(String::from).collect();
        assert!(fixture.prepare().is_err());
    }
    fixture.options.families = vec!["classic".into()];
    for operators in [
        vec!["NSM-001"],
        vec!["not_uppercase"],
        vec!["UNKNOWN"],
        vec!["BOOLEAN_NEGATION", "BOOLEAN_NEGATION"],
        vec![""],
    ] {
        fixture.options.operators = operators.into_iter().map(String::from).collect();
        assert!(fixture.prepare().is_err());
    }
    fixture.options.operators.clear();
    fixture.options.language = Some("dart".into());
    fixture.error("--language requires");
    fixture.options.families = vec!["language_specific".into()];
    fixture.options.language = None;
    fixture.error("explicit --language dart");
    fixture.options.language = Some("rust".into());
    fixture.error("explicit --language dart");
    assert!(!fixture.options.output.parent().unwrap().exists());
}

#[test]
fn method_channel_requires_test_framework_not_merely_flutter_sdk_dependency() {
    let mut fixture = Fixture::new();
    fixture.options.families = vec!["failure_mode".into()];
    for pubspec in [
        "name: example\n",
        "name: example\ndependencies:\n  flutter:\n    sdk: flutter\n",
    ] {
        fs::write(fixture.project.join("pubspec.yaml"), pubspec).unwrap();
        assert_eq!(fixture.prepare().unwrap().framework_flutter, Some(false));
    }
    fixture.options.operators = vec!["METHOD_CHANNEL_ERROR".into()];
    fixture.error("requires a Flutter package");
    fs::write(
        fixture.project.join("pubspec.yaml"),
        "name: example\ndev_dependencies:\n  flutter_test:\n    sdk: flutter\n",
    )
    .unwrap();
    assert_eq!(fixture.prepare().unwrap().framework_flutter, Some(true));
}

#[test]
fn relative_inputs_use_invocation_directory_and_nearest_package() {
    let mut fixture = Fixture::new();
    fixture.context.cwd = fixture.project.join("lib");
    fixture.options.source = PathBuf::from("../lib/calculator.dart");
    fixture.options.output = PathBuf::from("../profiles/calculator.yaml");
    let draft = fixture.prepare().unwrap();
    assert_eq!(draft.project, fixture.project);
    assert_eq!(
        draft.output,
        fixture.project.join("profiles/calculator.yaml")
    );
    assert_eq!(
        draft.values["rules"]["include"],
        json!(["lib/calculator.dart"])
    );
}

#[test]
fn unicode_quotes_controls_and_yaml_sensitive_file_names_preserve_literal_scope() {
    let mut fixture = Fixture::new();
    for name in [
        "калькулятор.dart",
        "calculator's value.dart",
        "yes.dart",
        "colon: name.dart",
        "a\nline.dart",
        "a\u{85}line.dart",
        "a\u{2028}line.dart",
        "a\u{2029}line.dart",
    ] {
        fixture.options.source = fixture.project.join("lib").join(name);
        fs::write(&fixture.options.source, "bool enabled() => true;").unwrap();
        let draft = fixture.prepare().unwrap();
        assert_eq!(
            draft.values["rules"]["include"],
            json!([format!("lib/{name}")])
        );
        assert_eq!(
            failforge_config::yaml::parse_yaml(&draft.text).unwrap(),
            draft.values
        );
    }
}

#[test]
fn source_requires_library_scope_and_rejects_tool_directories_and_globs() {
    let mut fixture = Fixture::new();
    for path in [
        "test/source.dart",
        "lib/BUILD/source.dart",
        "lib/.DART_TOOL/source.dart",
        "lib/file*.dart",
        "lib/file?.dart",
        "lib/file[.dart",
        "lib/file].dart",
        "lib/file{.dart",
        "lib/file}.dart",
        "lib/file\\.dart",
    ] {
        fixture.options.source = fixture.project.join(path);
        fs::create_dir_all(fixture.options.source.parent().unwrap()).unwrap();
        fs::write(&fixture.options.source, "bool enabled() => true;").unwrap();
        assert!(fixture.prepare().is_err(), "accepted {path}");
    }
}

#[test]
fn missing_directory_and_fifo_sources_fail_without_blocking() {
    let mut fixture = Fixture::new();
    for path in ["lib/missing.dart", "lib", "pubspec.yaml"] {
        fixture.options.source = fixture.project.join(path);
        assert!(fixture.prepare().is_err());
    }
    fixture.options.source = fixture.project.join("lib/fifo.dart");
    let fifo = CString::new(fixture.options.source.to_str().unwrap()).unwrap();
    // SAFETY: fifo is a live, NUL-terminated path; the mode contains only permission bits.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    fixture.error("readable regular");
}

#[test]
fn package_local_source_aliases_and_symlink_dotdot_are_rejected() {
    let mut fixture = Fixture::new();
    symlink(
        &fixture.options.source,
        fixture.project.join("lib/alias.dart"),
    )
    .unwrap();
    symlink(fixture.project.join("lib"), fixture.project.join("linked")).unwrap();
    for path in [
        "lib/alias.dart",
        "linked/calculator.dart",
        "linked/../lib/calculator.dart",
    ] {
        fixture.options.source = fixture.project.join(path);
        fixture.error("must not contain symlinks");
    }
}

#[test]
fn whole_package_alias_is_allowed_but_alias_to_package_interior_is_rejected() {
    let mut fixture = Fixture::new();
    let alias = fixture.root.join("alias");
    symlink(&fixture.project, &alias).unwrap();
    fixture.options.source = alias.join("lib/calculator.dart");
    fixture.options.output = alias.join("profiles/calculator.yml");
    assert_eq!(fixture.prepare().unwrap().project, fixture.project);
    let interior = fixture.root.join("interior");
    symlink(fixture.project.join("lib"), &interior).unwrap();
    fixture.options.source = interior.join("calculator.dart");
    fixture.error("must not contain symlinks");
}

#[test]
fn output_aliases_are_rejected_even_when_dangling_or_forced() {
    let mut fixture = Fixture::new();
    fs::create_dir(fixture.options.output.parent().unwrap()).unwrap();
    symlink(fixture.project.join("absent.yml"), &fixture.options.output).unwrap();
    for force in [false, true] {
        fixture.options.force = force;
        fixture.error("symlink");
    }
}

#[test]
fn output_cannot_cross_nearest_package_boundary() {
    let mut fixture = Fixture::new();
    fs::create_dir(fixture.project.join("nested")).unwrap();
    fs::write(
        fixture.project.join("nested/pubspec.yaml"),
        "name: nested\n",
    )
    .unwrap();
    for path in [
        fixture.root.join("outside.yml"),
        fixture.project.join("nested/profiles/source.yml"),
    ] {
        fixture.options.output = path;
        fixture.error("same package");
    }
}

#[test]
fn reserved_names_and_tool_directories_are_protected_in_every_case() {
    let mut fixture = Fixture::new();
    fixture.options.force = true;
    for name in RESERVED_FILES {
        for spelling in [
            (*name).to_string(),
            name.to_uppercase().replace(".YAML", ".yaml"),
        ] {
            fixture.options.output = fixture.project.join(spelling);
            fixture.error("configuration or tool");
        }
    }
    for directory in TOOL_DIRECTORIES {
        fixture.options.output = fixture
            .project
            .join(directory.to_uppercase())
            .join("profile.yml");
        fixture.error("configuration or tool");
    }
    for name in ["pubſpec.yaml", "analyſis_options.yaml", "dart_teﬆ.yaml"] {
        fixture.options.output = fixture.project.join(name);
        fixture.error("configuration or tool");
    }
    for name in ["profile", "profile.YML", "profile.dart"] {
        fixture.options.output = fixture.project.join(name);
        fixture.error(".yml or .yaml");
    }
}

#[test]
fn existing_output_is_untouched_and_requires_force() {
    let mut fixture = Fixture::new();
    fs::create_dir(fixture.options.output.parent().unwrap()).unwrap();
    fs::write(&fixture.options.output, "original").unwrap();
    fixture.error("--force");
    fixture.options.force = true;
    fixture.prepare().unwrap();
    assert_eq!(
        fs::read_to_string(&fixture.options.output).unwrap(),
        "original"
    );
}

#[test]
fn invalid_unicode_nul_and_empty_paths_are_configuration_errors() {
    let mut fixture = Fixture::new();
    for source in [
        PathBuf::new(),
        PathBuf::from("bad\0.dart"),
        PathBuf::from(std::ffi::OsString::from_vec(b"bad\xff.dart".to_vec())),
    ] {
        fixture.options.source = source;
        assert!(matches!(fixture.prepare(), Err(ProfileError::Config(_))));
    }
}

#[test]
fn sdk_and_adapter_overrides_become_absolute_and_conflicting_sdks_fail() {
    let mut fixture = Fixture::new();
    fixture.options.dart_sdk = Some("sdk/bin/dart".into());
    fixture.options.adapter_root = Some("custom-adapter".into());
    let draft = fixture.prepare().unwrap();
    assert_eq!(
        draft.values["dart_sdk"],
        fixture.project.join("sdk/bin/dart").to_str().unwrap()
    );
    assert_eq!(
        draft.values["adapter_root"],
        fixture.project.join("custom-adapter").to_str().unwrap()
    );
    fixture.options.flutter_sdk = Some("flutter/bin/flutter".into());
    assert!(fixture.prepare().is_err());
}

#[test]
fn pubspec_symlink_or_oversize_fails_before_profile_generation() {
    let fixture = Fixture::new();
    fs::write(
        fixture.project.join("pubspec.yaml"),
        vec![b'#'; MAX_YAML_BYTES + 1],
    )
    .unwrap();
    fixture.error("exceeds 1 MiB");
    fs::remove_file(fixture.project.join("pubspec.yaml")).unwrap();
    fs::write(fixture.project.join("manifest.yaml"), "name: example\n").unwrap();
    symlink(
        fixture.project.join("manifest.yaml"),
        fixture.project.join("pubspec.yaml"),
    )
    .unwrap();
    fixture.error("readable regular");
}

#[test]
fn actual_entry_spelling_is_used_on_case_insensitive_filesystems() {
    let mut fixture = Fixture::new();
    let requested = fixture.project.join("LIB/CALCULATOR.dart");
    if !requested.exists() {
        return;
    }
    fixture.options.source = requested;
    assert_eq!(
        fixture.prepare().unwrap().values["rules"]["include"],
        json!(["lib/calculator.dart"])
    );
    fs::hard_link(
        fixture.project.join("lib/calculator.dart"),
        fixture.project.join("lib/unrelated.dart"),
    )
    .unwrap();
    assert_eq!(
        fixture.prepare().unwrap().values["rules"]["include"],
        json!(["lib/calculator.dart"])
    );
}

#[test]
fn composed_unicode_alias_uses_discovered_spelling_even_with_unrelated_hardlink() {
    let mut fixture = Fixture::new();
    let actual = fixture.project.join("lib/cafe\u{301}.dart");
    fs::write(&actual, "bool enabled() => true;").unwrap();
    let requested = fixture.project.join("lib/café.dart");
    if !requested.exists() {
        return;
    }
    fs::hard_link(&actual, fixture.project.join("lib/unrelated.dart")).unwrap();
    fixture.options.source = requested;
    let actual_name = fs::read_dir(fixture.project.join("lib"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .find(|name| normalized_folded(name.to_str().unwrap()) == normalized_folded("café.dart"))
        .unwrap();
    assert_eq!(
        fixture.prepare().unwrap().values["rules"]["include"],
        json!([format!("lib/{}", actual_name.to_str().unwrap())])
    );
}

fn applicability(families: &[(&str, &[&str])]) -> ApplicabilityResult {
    ApplicabilityResult::for_test(
        families
            .iter()
            .map(|(family, operators)| crate::analysis::FamilyApplicability {
                family: (*family).into(),
                active_operators: operators
                    .iter()
                    .map(|operator| (*operator).into())
                    .collect(),
                ..Default::default()
            })
            .collect(),
    )
}

#[test]
fn applicability_intersects_requested_switches_and_preserves_disabled_families() {
    let mut fixture = Fixture::new();
    fixture.options.families = vec!["classic".into(), "failure_mode".into()];
    let mut draft = fixture.prepare().unwrap();
    let result = applicability(&[
        ("failure_mode", &[]),
        ("classic", &["BOOLEAN_LITERAL_FLIP"]),
    ]);
    apply_applicability(&mut draft, &result, &fixture.context).unwrap();
    let classic = draft.values["mutations"]["classic"]["operators"]
        .as_object()
        .unwrap();
    assert_eq!(classic.values().filter(|value| **value == true).count(), 1);
    assert_eq!(classic["BOOLEAN_LITERAL_FLIP"], true);
    assert!(
        draft.values["mutations"]["failure_mode"]["operators"]
            .as_object()
            .unwrap()
            .values()
            .all(|value| value == false)
    );
    assert_eq!(
        failforge_config::yaml::parse_yaml(&draft.text).unwrap(),
        draft.values
    );
    assert!(!draft.output.parent().unwrap().exists());
}

#[test]
fn malformed_or_empty_applicability_does_not_modify_the_draft() {
    let fixture = Fixture::new();
    let mut draft = fixture.prepare().unwrap();
    let original = draft.text.clone();
    let values = draft.values.clone();
    for result in [
        applicability(&[]),
        applicability(&[("failure_mode", &["EXCEPTION_AT_ENTRY"])]),
        applicability(&[
            ("classic", &["BOOLEAN_LITERAL_FLIP"]),
            ("classic", &["BOOLEAN_NEGATION"]),
        ]),
        applicability(&[("classic", &["CONDITIONAL_NEGATION"])]),
        applicability(&[("classic", &["UNKNOWN"])]),
        applicability(&[("classic", &[])]),
    ] {
        assert!(apply_applicability(&mut draft, &result, &fixture.context).is_err());
        assert_eq!(draft.text, original);
        assert_eq!(draft.values, values);
    }
}

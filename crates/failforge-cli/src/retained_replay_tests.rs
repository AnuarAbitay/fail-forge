use super::*;
use crate::test_support::symlink;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    project: PreparedProject,
    paths: ReplayPaths,
}
fn put(path: &Path, raw: impl AsRef<[u8]>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, raw).unwrap();
}
fn write_json(path: &Path, value: &Value) {
    put(path, serde_json::to_vec(value).unwrap());
}
fn read(path: &Path) -> Value {
    parse_json(&fs::read(path).unwrap()).unwrap()
}
fn jsonl(rows: &[Value]) -> Vec<u8> {
    rows.iter()
        .flat_map(|row| {
            let mut bytes = serde_json::to_vec(row).unwrap();
            bytes.push(b'\n');
            bytes
        })
        .collect()
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-retained-replay-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let project = root.join("project");
        put(&project.join("lib/subject.dart"), b"final value = true;\n");
        put(&project.join("test/subject_test.dart"), b"void main() {}\n");
        put(&project.join("pubspec.yaml"), b"name: subject\n");
        put(&project.join("pubspec.lock"), b"packages: {}\n");
        put(
            &project.join(".dart_tool/package_config.json"),
            b"{\"configVersion\":2,\"packages\":[]}",
        );
        put(&project.join(".dart_tool/package_graph.json"), b"{}");
        let framework = root.join("framework");
        put(
            &framework.join("lib/subject.dart"),
            b"final value = true;\n",
        );
        put(&framework.join("assets/value.txt"), b"pristine");
        let paths = ReplayPaths {
            transcript: root.join("archive/transcript.jsonl"),
            baseline: root.join("archive/baseline-result.json"),
            framework,
        };
        let rows = vec![
            json!({"type":"start","protocolVersion":"0.1.1","runnerVersion":"1.33.0","pid":1,"time":0}),
            json!({"type":"suite","suite":{"id":0,"platform":"vm","path":project.join("test/subject_test.dart")},"time":0}),
            json!({"type":"testStart","test":{"id":1,"name":"loading test/subject_test.dart","suiteID":0,"groupIDs":[],"metadata":{"skip":false}},"time":0}),
            json!({"type":"allSuites","count":1,"time":0}),
            json!({"type":"testDone","testID":1,"result":"success","skipped":false,"hidden":true,"time":0}),
            json!({"type":"group","group":{"id":2,"suiteID":0,"parentID":null,"name":"","testCount":1},"time":0}),
            json!({"type":"testStart","test":{"id":3,"name":"assertion","suiteID":0,"groupIDs":[2],"metadata":{"skip":false}},"time":0}),
            json!({"type":"testDone","testID":3,"result":"success","skipped":false,"hidden":false,"time":0}),
            json!({"type":"done","success":true,"time":0}),
        ];
        let raw = jsonl(&rows);
        put(&root.join("archive/capture/stdout.bin"), &raw);
        write_json(
            &paths.baseline,
            &json!({
                "runnerExitCode":0,"runnerSummary":format!("failforge: cause=ProcessCompleted status=exit:0 execution_ms=1 stdout_bytes={} stderr_bytes=0 cleanup=complete display_dropped_bytes=0 display_unconfirmed_bytes=0 stdout_issue=none stderr_issue=none stdout_tail_truncated=false stderr_tail_truncated=false\n",raw.len()),
                "outputSource":"capture/stdout.bin","completedTestIdentities":[["test/subject_test.dart","assertion"]]
            }),
        );
        let hashes = source_hashes(&project, &Cancellation::default()).unwrap();
        write_json(
            &root.join("archive/provenance.json"),
            &json!({"sourceFileHashes":hashes}),
        );
        write_json(
            &root.join("archive/dependency-result.json"),
            &json!({
                "generatedLockSha256":hashes[Path::new("pubspec.lock")],
                "generatedPackageConfigSha256":hashes[Path::new(".dart_tool/package_config.json")]
            }),
        );
        put(
            &paths.transcript,
            jsonl(&[
                json!({"step":"handshake","payload":{"protocolVersion":1,"operators":[]}}),
                json!({"step":"discover","payload":{"truncated":false,"returnedCandidates":0,"totalCandidates":0,"mutants":[]}}),
                json!({"step":"cleanup","payload":{"complete":true,"campaignSucceeded":true,"adapterCloseSucceeded":true,
                "runnerShutdown":true,"activeRuns":0,"exitCode":0,"writesPerformed":false,"validatedCandidates":0,"staticallyValidCandidates":0}}),
            ]),
        );
        Self {
            root,
            paths,
            project: PreparedProject {
                root: project,
                name: "subject".into(),
                packages: BTreeMap::new(),
                graph: json!({}),
                flutter: false,
                framework_collision: false,
            },
        }
    }
    fn load(&self) -> Result<LoadedReplay, NamespaceError> {
        LoadedReplay::load(&self.paths, &self.project, &Cancellation::default())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn passing_archive_binds_sources_identities_framework_and_every_evidence_file() {
    let fixture = Fixture::new();
    let replay = fixture.load().unwrap();
    assert_eq!(
        replay.expected,
        vec![["test/subject_test.dart".to_owned(), "assertion".into()]]
    );
    assert_eq!(replay.discovery["candidates"], json!([]));
    assert_eq!(replay.framework(), fixture.paths.framework);
    assert_eq!(replay.binding()["staticValidationReused"], true);
    assert_eq!(replay.protected_paths().len(), 6);
    replay.verify().unwrap();
}

#[test]
fn baseline_receipt_raw_identities_provenance_and_dependency_bindings_are_mandatory() {
    for case in [
        "exit",
        "summary",
        "raw",
        "identity",
        "provenance",
        "omitted-test",
        "dependency",
        "capture-path",
        "cwd",
    ] {
        let fixture = Fixture::new();
        let mut baseline = read(&fixture.paths.baseline);
        match case {
            "exit" => baseline["runnerExitCode"] = json!(1),
            "summary" => baseline["runnerSummary"] = json!("untrusted"),
            "raw" => put(
                &fixture.root.join("archive/capture/stdout.bin"),
                b"invalid\n",
            ),
            "identity" => {
                baseline["completedTestIdentities"] =
                    json!([["test/subject_test.dart", "different"]])
            }
            "provenance" | "omitted-test" => {
                let path = fixture.root.join("archive/provenance.json");
                let mut value = read(&path);
                if case == "provenance" {
                    value["sourceFileHashes"]["lib/subject.dart"] = json!("0".repeat(64));
                } else {
                    value["sourceFileHashes"]
                        .as_object_mut()
                        .unwrap()
                        .remove("test/subject_test.dart");
                }
                write_json(&path, &value);
            }
            "dependency" => write_json(
                &fixture.root.join("archive/dependency-result.json"),
                &json!({}),
            ),
            "capture-path" => baseline["outputSource"] = json!("../outside"),
            "cwd" => baseline["workingDirectory"] = json!("relative"),
            _ => unreachable!(),
        }
        write_json(&fixture.paths.baseline, &baseline);
        assert!(fixture.load().is_err(), "{case}");
    }
}

#[test]
fn namespace_framework_must_be_exact_and_unchanged_including_nonlibrary_resources() {
    let fixture = Fixture::new();
    put(
        &fixture.paths.framework.join("lib/subject.dart"),
        b"changed",
    );
    assert!(fixture.load().is_err());
    put(
        &fixture.paths.framework.join("lib/subject.dart"),
        b"final value = true;\n",
    );
    let replay = fixture.load().unwrap();
    put(
        &fixture.paths.framework.join("assets/value.txt"),
        b"replaced",
    );
    assert!(replay.verify().is_err());
}

#[test]
fn every_retained_file_is_verified_even_after_successful_loading() {
    for relative in [
        "transcript.jsonl",
        "baseline-result.json",
        "provenance.json",
        "dependency-result.json",
        "capture/stdout.bin",
    ] {
        let fixture = Fixture::new();
        let replay = fixture.load().unwrap();
        let path = fixture.root.join("archive").join(relative);
        let bytes = fs::read(&path).unwrap();
        let saved = path.with_extension("saved");
        fs::rename(&path, &saved).unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(
            replay.verify().is_err(),
            "same bytes at foreign inode: {relative}"
        );
    }
}

#[test]
fn same_inode_edits_source_additions_and_directory_replacements_fail_closed() {
    for case in ["capture", "added-test", "framework-root", "project-root"] {
        let fixture = Fixture::new();
        let replay = fixture.load().unwrap();
        match case {
            "capture" => {
                let path = fixture.root.join("archive/capture/stdout.bin");
                let mut bytes = fs::read(&path).unwrap();
                bytes[0] = b' ';
                fs::write(path, bytes).unwrap();
            }
            "added-test" => put(
                &fixture.project.root.join("test/new.dart"),
                b"void main() {}",
            ),
            "framework-root" | "project-root" => {
                let path = if case == "framework-root" {
                    &fixture.paths.framework
                } else {
                    &fixture.project.root
                };
                fs::rename(path, path.with_extension("saved")).unwrap();
                fs::create_dir(path).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(replay.verify().is_err(), "{case}");
    }
}

#[test]
fn late_bound_file_edit_is_rejected_after_remaining_inventory_checks() {
    let fixture = Fixture::new();
    let replay = fixture.load().unwrap();
    let result = replay.verify_before_final_check(|| {
        let path = fixture.root.join("archive/capture/stdout.bin");
        let mut raw = fs::read(&path).unwrap();
        raw[0] = b' ';
        fs::write(path, raw).unwrap();
    });
    assert!(result.is_err());
}

#[test]
fn bound_reader_rejects_symlinks_nonregular_files_limits_and_relative_paths() {
    let fixture = Fixture::new();
    let source = fixture.root.join("value");
    put(&source, b"12345");
    let link = fixture.root.join("link");
    symlink(&source, &link).unwrap();
    assert!(BoundFile::read(&link, 10).is_err());
    assert!(BoundFile::read(&fixture.root, 10).is_err());
    assert!(BoundFile::read(&source, 4).is_err());
    assert!(BoundFile::read(Path::new("relative"), 10).is_err());
    let (bound, raw) = BoundFile::read(&source, 5).unwrap();
    assert_eq!(raw, b"12345");
    bound.verify().unwrap();
}

#[test]
fn cancellation_stops_admission_and_relative_archive_paths_are_rejected() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    cancel.request(libc::SIGTERM);
    assert!(matches!(
        LoadedReplay::load(&fixture.paths, &fixture.project, &cancel),
        Err(NamespaceError::Cancelled(libc::SIGTERM))
    ));
    for path in ["", "../x", "/absolute", "a//b", "a/./b", "a\\b", "a/"] {
        assert!(relative(path).is_err(), "{path}");
    }
    assert_eq!(
        relative("capture/stdout.bin").unwrap(),
        Path::new("capture/stdout.bin")
    );
}

#[cfg(windows)]
#[test]
fn windows_bound_evidence_detects_replacement_and_native_content_changes() {
    let fixture = Fixture::new();
    let path = fixture.root.join("native-evidence");
    fs::write(&path, b"first").unwrap();
    let (bound, bytes) = BoundFile::read(&path, 5).unwrap();
    assert_eq!(bytes, b"first");
    fs::write(&path, b"other").unwrap();
    assert!(bound.verify().is_err());
    drop(bound);
    let (bound, _) = BoundFile::read(&path, 5).unwrap();
    fs::rename(&path, fixture.root.join("retained-old")).unwrap();
    fs::write(&path, b"other").unwrap();
    assert!(bound.verify().is_err());
}

use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ff-workspace-inputs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn file(&self, relative: &str, bytes: &[u8]) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    fn prepared(&self) -> PreparedProject {
        self.file("project/pubspec.yaml", b"name: subject\n");
        self.file("project/lib/subject.dart", b"const value = 1;\n");
        self.file("test/lib/test.dart", b"void main() {}\n");
        let config = json!({"configVersion":2,"packages":[
            {"name":"subject","rootUri":"../"},
            {"name":"test","rootUri":format!("file://{}",self.0.join("test").display())}
        ]});
        self.file(
            "project/.dart_tool/package_config.json",
            &serde_json::to_vec(&config).unwrap(),
        );
        self.file("project/.dart_tool/package_graph.json", br#"{"roots":["subject"],"packages":[{"name":"subject","dependencies":[]},{"name":"test","dependencies":[]}]}"#);
        inputs::inspect_project(&self.0.join("project"), &Cancellation::default()).unwrap()
    }
    fn runtime(&self) -> RuntimeInputs {
        for path in [
            "installation/bin/failforge",
            "installation/bin/failforge-adapter-campaign",
            "dart/bin/dart",
            "dart/lib/core.dart",
            "adapter/lib/adapter.dart",
            "installation/python/failforge_campaign/controller.py",
            "installation/scripts/shared.py",
            "flutter/bin/flutter",
            "flutter/bin/cache/flutter_tools.snapshot",
            "flutter/bin/cache/artifacts/engine/kernel.bin",
        ] {
            self.file(path, b"input");
        }
        self.file(
            "adapter/.dart_tool/package_config.json",
            br#"{"configVersion":2,"packages":[]}"#,
        );
        RuntimeInputs {
            installation: self.0.join("installation"),
            runner: self.0.join("installation/bin/failforge"),
            dart_sdk: self.0.join("dart"),
            flutter_sdk: Some(self.0.join("flutter")),
            adapter_root: self.0.join("adapter"),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn request_rejects_ambiguous_unknown_and_relative_fields_before_operations() {
    for request in [
        br#"{"operation":"inspect","root":"relative"}"#.as_slice(),
        br#"{"operation":"inspect","root":"/a","root":"/b"}"#,
        br#"{"operation":"inspect","root":"/a","extra":true}"#,
        br#"{"operation":"copy-project","source":"/a","destination":"../b"}"#,
        br#"{"operation":"delete","root":"/a"}"#,
        br#"{"operation":"bound-tree","root":"/a\u0000"}"#,
        br#"{"operation":"dependency-roots","path":false}"#,
    ] {
        assert!(decode(request).is_err(), "{:?}", request);
    }
    assert!(decode(br#"{"operation":"source-hashes","root":"/safe"}"#).is_ok());
}

#[test]
fn environment_fingerprint_matches_sorted_python_ascii_json() {
    let environment = BTreeMap::from([
        ("z".into(), "é😀\u{7f}".into()),
        ("a".into(), "\"\\\t\n\r\u{8}\u{c}\u{1}".into()),
    ]);
    let expected = r#"[["a", "\"\\\t\n\r\b\f\u0001"], ["z", "\u00e9\ud83d\ude00\u007f"]]"#;
    assert_eq!(environment_json(&environment), expected);
    assert_eq!(
        environment_hash(&environment),
        format!("{:x}", Sha256::digest(expected.as_bytes()))
    );
    assert_eq!(environment_json(&BTreeMap::new()), "[]");
}

#[test]
fn binding_covers_project_dependencies_sdk_tools_controller_and_environment() {
    let fixture = Fixture::new();
    let project = fixture.prepared();
    let runtime = fixture.runtime();
    let environment = BTreeMap::from([("CI".into(), "true".into())]);
    let cancel = Cancellation::default();
    let first = bind_inputs(&runtime, &project, &environment, &cancel).unwrap();
    assert_eq!(
        first["dependencies"]["test"]["root"],
        value(fixture.0.join("test")).unwrap()
    );
    for (path, section) in [
        ("project/lib/subject.dart", "project"),
        ("test/lib/test.dart", "dependencies"),
        ("dart/lib/core.dart", "sdk"),
        ("installation/bin/failforge", "tools"),
        (
            "installation/python/failforge_campaign/controller.py",
            "controller",
        ),
        ("adapter/lib/adapter.dart", "adapter"),
        ("flutter/bin/cache/artifacts/engine/kernel.bin", "sdk"),
    ] {
        let before = bind_inputs(&runtime, &project, &environment, &cancel).unwrap();
        fixture.file(path, b"changed");
        let after = bind_inputs(&runtime, &project, &environment, &cancel).unwrap();
        assert_ne!(before[section], after[section], "{path}");
    }
    let changed = bind_inputs(&runtime, &project, &BTreeMap::new(), &cancel).unwrap();
    assert_ne!(first["environmentSha256"], changed["environmentSha256"]);
    assert!(first.get("environment").is_none());
    let mut project = project;
    project.packages.remove("test");
    assert!(bind_inputs(&runtime, &project, &environment, &cancel).is_err());
}

#[test]
fn all_filesystem_requests_use_shared_native_inputs_and_cancellation() {
    let fixture = Fixture::new();
    let project = fixture.prepared();
    let cancellation = Cancellation::default();
    let observed = dispatch(
        Request::Inspect {
            root: project.root.clone(),
        },
        &cancellation,
    )
    .unwrap();
    assert_eq!(observed["name"], "subject");
    assert_eq!(observed["frameworkCollision"], false);
    assert!(observed["graph"].is_object());
    assert!(
        dispatch(
            Request::SourceHashes {
                root: project.root.clone()
            },
            &cancellation
        )
        .unwrap()["lib/subject.dart"]
            .is_string()
    );
    assert!(
        dispatch(
            Request::DependencyRoots {
                path: project.root.join(".dart_tool/package_config.json")
            },
            &cancellation
        )
        .unwrap()["test"]
            .is_string()
    );
    assert!(
        dispatch(
            Request::BoundTree {
                root: project.root.clone()
            },
            &cancellation
        )
        .unwrap()["files"]
            .is_object()
    );
    for (index, snapshot) in [false, true].into_iter().enumerate() {
        let destination = fixture.0.join(format!("copy-{index}"));
        let request = if snapshot {
            Request::CopySnapshot {
                source: project.root.clone(),
                destination: destination.clone(),
            }
        } else {
            Request::CopyProject {
                source: project.root.clone(),
                destination: destination.clone(),
            }
        };
        let copied = dispatch(request, &cancellation).unwrap();
        assert_eq!(copied["root"], value(destination).unwrap());
    }
    cancellation.request(libc::SIGINT);
    assert!(matches!(
        dispatch(Request::Inspect { root: project.root }, &cancellation),
        Err(InputError::Cancelled(libc::SIGINT))
    ));
}

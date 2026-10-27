//! Finite SDK surrogate for native campaign process tests.
//! It implements adapter JSONL and package:test JSONL without another runtime.
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Read, Write};
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const FAMILIES: [&str; 3] = ["classic", "failure_mode", "language_specific"];
const OPERATORS: [&str; 3] = [
    "NUMERIC_LITERAL_REPLACEMENT",
    "EXCEPTION_AT_ENTRY",
    "NSM-001",
];
const ORIGINAL: &str = "const value = 1;\n";

fn mark(config: &Value, kind: &str, fields: Value) {
    let mut row = fields.as_object().unwrap().clone();
    row.insert("kind".into(), kind.into());
    row.insert("pid".into(), std::process::id().into());
    // SAFETY: getppid has no pointer arguments or side effects.
    row.insert("parent".into(), parent_id().into());
    row.insert(
        "root".into(),
        std::env::current_dir().unwrap().to_str().unwrap().into(),
    );
    let mut stream = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(config["trace"].as_str().unwrap())
        .unwrap();
    lock_trace(&stream, true);
    serde_json::to_writer(&mut stream, &row).unwrap();
    writeln!(stream).unwrap();
    stream.flush().unwrap();
}

#[cfg(unix)]
fn parent_id() -> u32 {
    // SAFETY: getppid only reads this fixture's parent identifier.
    unsafe { libc::getppid() as u32 }
}
#[cfg(windows)]
fn parent_id() -> u32 {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: snapshot is an owned native handle; entry has its required ABI size.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        assert!(snapshot != INVALID_HANDLE_VALUE);
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = Process32FirstW(snapshot, &mut entry);
        while found != 0 {
            if entry.th32ProcessID == std::process::id() {
                CloseHandle(snapshot);
                return entry.th32ParentProcessID;
            }
            found = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
        panic!("current fixture process missing from process snapshot");
    }
}
fn lock_trace(file: &fs::File, exclusive: bool) {
    #[cfg(unix)]
    assert_eq!(
        unsafe {
            libc::flock(
                file.as_raw_fd(),
                if exclusive {
                    libc::LOCK_EX
                } else {
                    libc::LOCK_SH
                },
            )
        },
        0
    );
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
        use windows_sys::Win32::System::IO::OVERLAPPED;
        let mut offset = OVERLAPPED::default();
        // A synchronous file handle keeps this blocking fixture-only lock local
        // to the call. Closing the file releases the complete-record lock.
        assert_ne!(
            unsafe {
                LockFileEx(
                    file.as_raw_handle(),
                    if exclusive {
                        LOCKFILE_EXCLUSIVE_LOCK
                    } else {
                        0
                    },
                    0,
                    u32::MAX,
                    u32::MAX,
                    &mut offset,
                )
            },
            0
        );
    }
}
fn rows(config: &Value) -> Vec<Value> {
    let mut file = fs::File::open(config["trace"].as_str().unwrap()).unwrap();
    lock_trace(&file, false);
    let mut text = String::new();
    file.read_to_string(&mut text).unwrap();
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn wait_for_workers(config: &Value, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while rows(config)
        .iter()
        .filter(|row| row["kind"] == "test_start" && row["patched"] == true)
        .count()
        < count
    {
        assert!(
            Instant::now() < deadline,
            "second fixture worker did not start"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn adapter(config: &Value) {
    let mode = config["mode"].as_str().unwrap();
    let mut family = None;
    mark(config, "adapter_start", json!({}));
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let command = request["command"].as_str().unwrap();
        let payload = match command {
            "handshake" => json!({"protocolVersion":1,"operators":
                FAMILIES.iter().zip(OPERATORS).map(|(family,operator)|
                    json!({"id":operator,"version":1,"family":family})).collect::<Vec<_>>()}),
            "discover" => {
                let selected = request["payload"]["rules"]["operators"][0]
                    .as_str()
                    .unwrap_or(OPERATORS[0]);
                let index = OPERATORS.iter().position(|name| *name == selected).unwrap();
                let current = FAMILIES[index];
                family = Some(current);
                let root = Path::new(request["payload"]["workspaceRoot"].as_str().unwrap());
                let source = fs::read(root.join("lib/subject.dart")).unwrap();
                let offset = source.iter().position(|byte| *byte == b'1').unwrap();
                let source_hash = digest(&source);
                let mutants = config["cases"][current]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|replacement| {
                        let replacement = replacement.as_str().unwrap();
                        let id = digest(
                            &serde_json::to_vec(&json!([
                                1,
                                "lib/subject.dart",
                                source_hash,
                                selected,
                                1,
                                current,
                                offset,
                                1,
                                "1",
                                replacement
                            ]))
                            .unwrap(),
                        );
                        json!({"id":id,"operatorId":selected,"operatorVersion":1,
                            "family":current,"file":"lib/subject.dart","sourceHash":source_hash,
                            "components":[],"symbol":{},"validation":"unvalidated",
                            "edit":{"offsetBytes":offset,"lengthBytes":1,"expected":"1",
                                "replacement":replacement}})
                    })
                    .collect::<Vec<_>>();
                mark(
                    config,
                    "discovery",
                    json!({"family":current,"count":mutants.len(),
                        "limit":request["payload"]["limits"]["maxMutants"]}),
                );
                json!({"truncated":mode == "truncated","totalCandidates":mutants.len(),
                    "returnedCandidates":mutants.len(),"mutants":mutants})
            }
            "validate" => {
                let mutant = &request["payload"]["mutant"];
                let valid = mutant["edit"]["replacement"] != "missing";
                mark(config, "validation", json!({"family":family,"valid":valid}));
                json!({"mutantId":if mode == "invalid_validation" {
                    json!("forged")
                } else { mutant["id"].clone() }, "valid":valid,
                    "validation":if valid {"valid"} else {"invalid"},"diagnostics":[]})
            }
            _ => panic!("unexpected adapter command: {command}"),
        };
        println!(
            "{}",
            json!({"protocolVersion":1,"requestId":request["requestId"],
                "kind":command,"payload":payload})
        );
        io::stdout().flush().unwrap();
    }
    mark(config, "adapter_end", json!({"family":family}));
}

fn test_events(root: &Path, failed: bool) -> Vec<Value> {
    let path = root.join("test/subject_test.dart");
    let mut events = vec![
        json!({"type":"start","protocolVersion":"0.1.1","runnerVersion":"1.33.0",
            "pid":std::process::id()}),
        json!({"type":"suite","suite":{"id":0,"platform":"vm","path":path}}),
        json!({"type":"testStart","test":{"id":1,"name":format!("loading {}",path.display()),
            "suiteID":0,"groupIDs":[],"metadata":{"skip":false}}}),
        json!({"type":"allSuites","count":1}),
        json!({"type":"testDone","testID":1,"result":"success","skipped":false,"hidden":true}),
        json!({"type":"group","group":{"id":2,"suiteID":0,"parentID":null,"name":"","testCount":1}}),
        json!({"type":"testStart","test":{"id":3,"name":"assertion","suiteID":0,
            "groupIDs":[2],"metadata":{"skip":false}}}),
    ];
    if failed {
        events.push(
            json!({"type":"error","testID":3,"error":"expected different value",
            "stackTrace":"stack","isFailure":true}),
        );
    }
    events.push(json!({"type":"testDone","testID":3,
        "result":if failed {"failure"} else {"success"},"skipped":false,"hidden":false}));
    events.push(json!({"type":"done","success":!failed}));
    for event in &mut events {
        event["time"] = 0.into();
    }
    events
}

fn test(config: &Value) -> i32 {
    let root = std::env::current_dir().unwrap();
    let source = fs::read_to_string(root.join("lib/subject.dart")).unwrap();
    let patched = source != ORIGINAL;
    let mode = config["mode"].as_str().unwrap();
    mark(
        config,
        "test_start",
        json!({"patched":patched,"source":source}),
    );
    if patched && matches!(mode, "hold" | "progress_failure") {
        if source == "const value = 2;\n" {
            wait_for_workers(config, 2);
            if mode == "progress_failure" {
                let target = Path::new(config["output"].as_str().unwrap()).join("progress.json");
                fs::remove_file(&target).unwrap();
                fs::create_dir(&target).unwrap();
                fs::write(target.join("sentinel"), "foreign").unwrap();
            } else {
                // Both workers remain active until the test signals their
                // owner. A completed first worker cannot race a third launch.
                std::thread::sleep(Duration::from_secs(12));
                panic!("owner did not cancel the held worker");
            }
        } else {
            // Bounded independently of the owner; normally its signal stops us.
            std::thread::sleep(Duration::from_secs(12));
            panic!("owner did not cancel the held worker");
        }
    }
    if patched && mode == "source_drift" {
        fs::write(config["original"].as_str().unwrap(), "const value = 99;\n").unwrap();
    }
    if patched && mode == "sdk_drift" {
        fs::write(config["sdk_input"].as_str().unwrap(), "changed SDK input\n").unwrap();
    }
    if patched && mode == "garbage" {
        println!("not a package:test reporter event");
        return 1;
    }
    let original_attempts = rows(config)
        .iter()
        .filter(|row| row["kind"] == "test_start" && row["patched"] == false)
        .count();
    let failed = (patched && mode != "survived")
        || mode == "baseline_failure"
        || (!patched && mode == "final_original_failure" && original_attempts == 2)
        || (!patched && mode == "later_baseline_failure" && original_attempts == 3);
    for event in test_events(&root, failed) {
        println!("{event}");
    }
    mark(
        config,
        "test_end",
        json!({"patched":patched,"source":source}),
    );
    i32::from(failed)
}

fn main() {
    for key in [
        "FAILFORGE_WORKSPACE_REQUEST_FD",
        "FAILFORGE_WORKSPACE_CAPS_FDS",
        "FAILFORGE_WORKSPACE_LEASE_FD",
        "FAILFORGE_WORKSPACE_HANDOFF_FD",
        "FAILFORGE_TEST_CONTEXT_FD",
        "FAILFORGE_TEST_EVIDENCE_FD",
        "FAILFORGE_RUNNER_READY_FD",
        "FAILFORGE_ADAPTER_CANCEL_FD",
        "FAILFORGE_ADAPTER_CANCEL_PIPE",
        "FAILFORGE_CLI_CANCEL_PIPE",
    ] {
        assert!(std::env::var_os(key).is_none(), "private descriptor: {key}");
    }
    let path = std::env::var_os("FF_CAMPAIGN_FIXTURE").expect("fixture configuration");
    let config: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    if std::env::args_os().skip(1).any(|arg| {
        PathBuf::from(arg).file_name() == Some(std::ffi::OsStr::new("failforge_dart_adapter.dart"))
    }) {
        adapter(&config);
    } else {
        std::process::exit(test(&config));
    }
}

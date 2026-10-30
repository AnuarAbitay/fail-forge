//! Real controller death while a Dart author test owns a live process tree.
//!
//! Windows has no separate Unix-style launcher/watchdog: the public controller
//! owns Runner's kill-on-close jobs. Its forced death must leave honest partial
//! evidence; the next public campaign, not a fabricated lease, recovers it.
use super::*;
use serde::Deserialize;
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};

const AUTHOR_TEST: &str = "controller crash tree";
const READY_LIMIT: usize = 4096;
const PROCESS_SYNCHRONIZE: u32 = 0x0010_0000;

// Every descendant waits on an owned pipe, rather than sleeping for an assumed
// startup interval. The author only announces readiness after both generations
// and a separate background watcher have announced their own PIDs.
const TREE_HELPER: &str = r#"import 'dart:convert';
import 'dart:io';

Future<Map<String, dynamic>> ready(Process child) async =>
    jsonDecode(await child.stdout.transform(utf8.decoder)
        .transform(const LineSplitter()).first) as Map<String, dynamic>;

Future<void> main(List<String> arguments) async {
  final role = arguments.single;
  if (role == 'child') {
    final grandchild = await Process.start(Platform.resolvedExecutable,
        ['--disable-dart-dev', Platform.script.toString(), 'grandchild']);
    final observed = await ready(grandchild);
    if (observed['role'] != 'grandchild' || observed['pid'] != grandchild.pid) {
      throw StateError('grandchild readiness mismatch');
    }
    stdout.writeln(jsonEncode({'role': role, 'pid': pid,
        'grandchildPid': grandchild.pid}));
    await stdout.flush();
    await stdin.drain<void>();
    // Keep the Process and its stdin capability live across the readiness wait.
    await grandchild.stdin.close();
    await grandchild.exitCode;
    return;
  }
  if (role != 'grandchild' && role != 'watcher') {
    throw StateError('unknown fixture role');
  }
  stdout.writeln(jsonEncode({'role': role, 'pid': pid}));
  await stdout.flush();
  await stdin.drain<void>();
}
"#;

fn author_source(port: u16, nonce: &str) -> Result<String> {
    let nonce = serde_json::to_string(nonce)?;
    Ok(format!(
        r#"import 'dart:convert';
import 'dart:io';
import 'package:campaign_sample/value.dart';
import 'package:test/test.dart';

Future<Map<String, dynamic>> ready(Process child) async =>
    jsonDecode(await child.stdout.transform(utf8.decoder)
        .transform(const LineSplitter()).first) as Map<String, dynamic>;

void main() {{
  test('{AUTHOR_TEST}', () async {{
    if (!keepGoing()) return;
    final helper = File('tool/crash_tree.dart').absolute.uri.toString();
    final child = await Process.start(Platform.resolvedExecutable,
        ['--disable-dart-dev', helper, 'child']);
    final tree = await ready(child);
    final watcher = await Process.start(Platform.resolvedExecutable,
        ['--disable-dart-dev', helper, 'watcher']);
    final background = await ready(watcher);
    if (tree['role'] != 'child' || tree['pid'] != child.pid ||
        background['role'] != 'watcher' || background['pid'] != watcher.pid) {{
      throw StateError('descendant readiness mismatch');
    }}
    final barrier = await Socket.connect(InternetAddress.loopbackIPv4, {port});
    barrier.writeln(jsonEncode({{'nonce': {nonce}, 'authorPid': pid,
        'childPid': child.pid, 'grandchildPid': tree['grandchildPid'],
        'watcherPid': watcher.pid}}));
    await barrier.flush();
    // The harness retains its peer until the controller has actually died.
    await barrier.drain<void>();
    // Retain both Process objects and their pipe capabilities while at barrier.
    await child.stdin.close();
    await watcher.stdin.close();
    await Future.wait([child.exitCode, watcher.exitCode]);
  }});
}}
"#
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Ready {
    nonce: String,
    author_pid: u32,
    child_pid: u32,
    grandchild_pid: u32,
    watcher_pid: u32,
}

struct Process {
    role: &'static str,
    pid: u32,
    image: PathBuf,
    handle: OwnedHandle,
}
impl Process {
    fn open(role: &'static str, pid: u32) -> Result<Self> {
        ensure(pid != 0, "fixture announced an invalid process ID")?;
        // SAFETY: these PIDs come from our nonce-bound fixture barrier or the
        // matching live reporter capture; each process is still held at ready.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful OpenProcess transferred a unique owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
        let mut name = vec![0_u16; 32768];
        let mut length = name.len() as u32;
        // SAFETY: handle is retained and name is writable native path storage.
        if unsafe {
            QueryFullProcessImageNameW(
                handle.as_raw_handle().cast(),
                0,
                name.as_mut_ptr(),
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let process = Self {
            role,
            pid,
            image: std::ffi::OsString::from_wide(&name[..length as usize]).into(),
            handle,
        };
        ensure(
            process.alive()?,
            format!("{role} exited before the crash barrier"),
        )?;
        Ok(process)
    }

    fn alive(&self) -> Result<bool> {
        // SAFETY: retained process handle prevents PID reuse from affecting proof.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle().cast(), 0) } {
            WAIT_TIMEOUT => Ok(true),
            WAIT_OBJECT_0 => Ok(false),
            _ => Err(io::Error::last_os_error().into()),
        }
    }

    fn wait_until(&self, deadline: Instant) -> Result<u32> {
        let milliseconds = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: only this exact retained process is observed, never a PID lookup.
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle().cast(), milliseconds) } {
            WAIT_OBJECT_0 => {
                let mut code = 0;
                // SAFETY: the retained handle is signalled and code is writable.
                if unsafe { GetExitCodeProcess(self.handle.as_raw_handle().cast(), &mut code) } == 0
                {
                    return Err(io::Error::last_os_error().into());
                }
                Ok(code)
            }
            WAIT_TIMEOUT => Err(format!(
                "{} (PID {}) survived the controller crash deadline",
                self.role, self.pid
            )
            .into()),
            _ => Err(io::Error::last_os_error().into()),
        }
    }

    fn observation(&self) -> Value {
        json!({"role":self.role,"pid":self.pid,"image":self.image})
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // Failure-path containment only. Successful cleanup evidence is recorded
        // before any destructor runs; fallback termination cannot make a pass.
        if self.alive().unwrap_or(false) {
            unsafe {
                TerminateProcess(self.handle.as_raw_handle().cast(), 125);
                WaitForSingleObject(self.handle.as_raw_handle().cast(), 5000);
            }
        }
    }
}

fn ready(
    listener: &TcpListener,
    owner: &mut common::OwnedChild,
    nonce: &str,
    budget: Duration,
) -> Result<(TcpStream, Ready)> {
    let deadline = Instant::now() + budget;
    listener.set_nonblocking(true)?;
    let mut stream = loop {
        common::check_interrupt()?;
        ensure(
            owner.0.try_wait()?.is_none(),
            "controller exited before descendant readiness",
        )?;
        ensure(
            Instant::now() < deadline,
            "Dart process tree never reached its crash barrier",
        )?;
        match listener.accept() {
            Ok((stream, address)) => {
                ensure(address.ip().is_loopback(), "nonlocal crash barrier peer")?;
                break stream;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Polling an explicit barrier is not a startup-time assumption.
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    };
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    let mut bytes = Vec::new();
    while bytes.last() != Some(&b'\n') {
        common::check_interrupt()?;
        ensure(
            owner.0.try_wait()?.is_none(),
            "controller exited during descendant readiness",
        )?;
        ensure(
            Instant::now() < deadline,
            "incomplete Dart process tree readiness",
        )?;
        ensure(
            bytes.len() < READY_LIMIT,
            "oversized crash barrier response",
        )?;
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte) {
            Ok(0) => return Err("Dart process tree closed the crash barrier".into()),
            Ok(_) => bytes.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let observed: Ready = serde_json::from_slice(&bytes)?;
    ensure(observed.nonce == nonce, "foreign crash barrier response")?;
    let pids = [
        observed.author_pid,
        observed.child_pid,
        observed.grandchild_pid,
        observed.watcher_pid,
    ];
    ensure(
        pids.into_iter().all(|pid| pid != 0 && pid != owner.0.id())
            && pids.into_iter().collect::<BTreeSet<_>>().len() == pids.len(),
        "crash fixture process identities overlap",
    )?;
    Ok((stream, observed))
}

fn live_capture(root: &Path) -> Result<Option<(PathBuf, u32)>> {
    if !root.is_dir() {
        return Ok(None);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(observed) = live_capture(&entry.path())? {
                return Ok(Some(observed));
            }
        } else if entry.file_name() == "stdout.bin" {
            let mut runner = None;
            let mut author = false;
            for line in fs::read(entry.path())?.split(|byte| *byte == b'\n') {
                if let Ok(row) = serde_json::from_slice::<Value>(line) {
                    if row["type"] == "start" {
                        runner = row["pid"].as_u64().and_then(|pid| u32::try_from(pid).ok());
                    }
                    author |= row["type"] == "testStart" && row["test"]["name"] == AUTHOR_TEST;
                }
            }
            if author {
                if let Some(pid) = runner {
                    return Ok(Some((entry.path(), pid)));
                }
            }
        }
    }
    Ok(None)
}

fn incomplete(summary: &Value) -> Result<()> {
    ensure(
        matches!(
            summary["status"].as_str(),
            Some("running" | "cancelled" | "failed")
        ) && summary["complete"] == false
            && summary["completedMutants"] == 0
            && summary["records"].as_array().is_some_and(Vec::is_empty),
        "crashed controller published a completed or fabricated mutation result",
    )?;
    for name in ["mutationScore", "failureModeScore", "languageSpecificScore"] {
        ensure(
            summary[name].is_null(),
            "crashed controller exposed a score",
        )?;
    }
    Ok(())
}

fn lease_busy(path: &Path) -> Result<bool> {
    let lease = failforge_workspace::windows_fs::regular(path, false)?;
    // A successfully acquired probe lock is released by closing this independent
    // file. The retained observer never owns the campaign's shared lease.
    Ok(!failforge_workspace::windows_fs::try_lock_at(
        &lease,
        false,
        failforge_runner::WORKSPACE_LEASE_LOCK_OFFSET,
    )?)
}

impl Harness {
    pub(super) fn controller_crash(&mut self) -> Result<()> {
        let temporary = common::Temporary::new("ff-crash")?;
        let temp_root = temporary.0.clone();
        // On failure preserve the real abandoned workspace and its evidence.
        std::mem::forget(temporary);
        let previous = self.temp_root.replace(temp_root.clone());
        let result = self.controller_crash_in(&temp_root);
        self.temp_root = previous;
        if result.is_ok() {
            fs::remove_dir_all(temp_root)?;
        }
        result
    }

    fn controller_crash_in(&mut self, temp_root: &Path) -> Result<()> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let nonce = failforge_workspace::windows_fs::random_token()?;
        let project = fixture(
            &self.root,
            &self.output.join("windows-controller-crash-subject"),
            "bool keepGoing() => false;\n",
            &author_source(listener.local_addr()?.port(), &nonce)?,
        )?;
        fs::create_dir(project.join("tool"))?;
        fs::write(project.join("tool/crash_tree.dart"), TREE_HELPER)?;
        let mut empty = profile();
        empty["mutations"]
            .as_object_mut()
            .unwrap()
            .retain(|key, _| key == "classic");
        empty["rules"]["methods"] = json!(["noSuchMethod"]);
        for name in [
            "windows-crash-active-owner-preserved",
            "windows-crash-stale-recovered",
        ] {
            let mut settings = empty.clone();
            settings["output"] = serde_json::to_value(self.output.join(name))?;
            settings["dart_sdk"] = serde_json::to_value(&self.sdk)?;
            common::write_json(
                &project
                    .join("profiles with spaces")
                    .join(format!("{name}.yml")),
                &settings,
            )?;
        }
        let original = common::tree(&project)?;
        let destination = self.output.join("windows-controller-crash");
        let logs = self.output.join("windows-controller-crash-command");
        fs::create_dir(&logs)?;
        let mut command = self.command();
        command
            .arg("mutate")
            .arg("--installation")
            .arg(&self.root)
            .arg("--project")
            .arg(&project)
            .arg("--dart-sdk")
            .arg(&self.sdk)
            .args([
                "--family",
                "classic",
                "--operator",
                "BOOLEAN_LITERAL_FLIP",
                "--method",
                "keepGoing",
                "--include",
                "lib/value.dart",
                "--test-timeout",
                "3m",
                "--command-timeout",
                "4m",
            ])
            .arg("--output")
            .arg(&destination)
            .stdout(File::create(logs.join("stdout.log"))?)
            .stderr(File::create(logs.join("stderr.log"))?);
        println!("regression windows-controller-crash");
        let mut owner = common::OwnedChild::spawn(&mut command)?;
        let (barrier, announced) = ready(&listener, &mut owner, &nonce, Duration::from_secs(120))?;
        let capture_deadline = Instant::now() + Duration::from_secs(10);
        let (capture, runner_pid) = loop {
            if let Some(observed) = live_capture(&destination.join("mutants"))? {
                break observed;
            }
            common::check_interrupt()?;
            ensure(
                owner.0.try_wait()?.is_none() && Instant::now() < capture_deadline,
                "ready Dart author lacks matching native mutant reporter capture",
            )?;
            std::thread::sleep(Duration::from_millis(10));
        };
        ensure(
            runner_pid != owner.0.id(),
            "Dart reporter incorrectly identified the native controller as its process",
        )?;
        let processes = [
            Process::open("controller-and-native-owner", owner.0.id())?,
            Process::open("dart-test-runner", runner_pid)?,
            Process::open("author-test-vm", announced.author_pid)?,
            Process::open("author-child", announced.child_pid)?,
            Process::open("author-grandchild", announced.grandchild_pid)?,
            Process::open("background-watcher", announced.watcher_pid)?,
        ];
        let registry = temp_root.join(failforge_workspace::windows_fs::campaign_registry_name()?);
        let campaigns = fs::read_dir(&registry)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("campaign-"))
            })
            .collect::<Vec<_>>();
        ensure(
            campaigns.len() == 1,
            "expected one real live crash campaign",
        )?;
        let campaign = &campaigns[0];
        let campaign_name = campaign.file_name().unwrap().to_string_lossy().into_owned();
        let lease_path = campaign.join("lease");
        let lease = failforge_workspace::windows_fs::regular(&lease_path, false)?;
        let lease_identity = failforge_workspace::windows_fs::identity(&lease)?;
        ensure(
            lease_busy(&lease_path)?,
            "live Dart tree has no active campaign lease",
        )?;
        incomplete(&common::read_json(&destination.join("summary.json"))?)?;
        let active = self.configured(
            "windows-crash-active-owner-preserved",
            &project,
            empty.clone(),
            0,
        )?;
        ensure(
            active["workspaceRecovery"]["skipped"]
                .as_array()
                .is_some_and(|rows| {
                    rows.iter()
                        .any(|row| row["name"] == campaign_name && row["reason"] == "active_lease")
                }),
            "concurrent public campaign did not preserve the live crash owner's lease",
        )?;
        ensure(
            campaign.is_dir()
                && lease_busy(&lease_path)?
                && failforge_workspace::windows_fs::identity(
                    &failforge_workspace::windows_fs::regular(&lease_path, false)?,
                )? == lease_identity,
            "active-lease probe removed or replaced the live workspace",
        )?;
        for process in &processes {
            ensure(
                process.alive()?,
                format!("{} exited before forced controller death", process.role),
            )?;
        }
        let evidence_path = self.output.join("windows-controller-crash-evidence.json");
        let mut evidence = json!({"name":"windows-controller-crash", "phase":"ready",
            "nativeOwnerIsController":true,"activeNativeOwnerProtected":true,
            "campaign":campaign,"report":destination,"mutantCapture":capture,
            "retainedProcessHandles":processes.iter().map(Process::observation).collect::<Vec<_>>(),
            "cleanupEvidenceComplete":false});
        common::write_json(&evidence_path, &evidence)?;
        let crash_started = Instant::now();
        let deadline = crash_started + Duration::from_secs(15);
        owner.signal(9)?;
        let exit = owner.wait(Duration::from_secs(10))?;
        ensure(exit != 0, "forced controller death returned success")?;
        evidence["phase"] = "owner-killed-waiting-descendants".into();
        evidence["exitCode"] = exit.into();
        common::write_json(&evidence_path, &evidence)?;
        let exits = processes
            .iter()
            .map(|process| {
                let code = process.wait_until(deadline)?;
                Ok(
                    json!({"role":process.role,"pid":process.pid,"exitCode":code,
                "retainedHandleSignalled":true}),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        evidence["processDrainWallSeconds"] = crash_started.elapsed().as_secs_f64().into();
        evidence["processDrainBudgetSeconds"] = 15.into();
        drop(barrier);
        let summary = common::read_json(&destination.join("summary.json"))?;
        incomplete(&summary)?;
        ensure(
            common::tree(&project)? == original,
            "controller crash changed original inputs",
        )?;
        ensure(
            campaign.is_dir() && !lease_busy(&lease_path)?,
            "dead native owner did not leave the real campaign recoverable",
        )?;
        drop(lease);
        let artifacts = common::tree(&destination)?;
        ensure(
            destination.join("manifest.json").is_file()
                && capture.is_file()
                && destination.join("progress.json").is_file(),
            "controller crash lost report artifacts",
        )?;
        evidence["phase"] = "processes-exited-before-recovery".into();
        evidence["processExits"] = json!(exits);
        evidence["observedCampaignStatus"] = summary["status"].clone();
        evidence["campaignComplete"] = summary["complete"].clone();
        evidence["scoresWithheld"] = true.into();
        common::write_json(&evidence_path, &evidence)?;
        let recovered = self.configured("windows-crash-stale-recovered", &project, empty, 0)?;
        ensure(
            recovered["workspaceRecovery"]["recovered"]
                .as_array()
                .is_some_and(|rows| rows.contains(&json!(campaign_name))),
            "next public campaign did not recover the actual crashed workspace",
        )?;
        ensure(
            !campaign.exists(),
            "actual crashed workspace remains after public recovery",
        )?;
        for entry in fs::read_dir(&registry)? {
            let name = entry?.file_name();
            ensure(
                name == ".global.lock" || name == ".owner",
                "crash registry still has workspaces",
            )?;
        }
        ensure(
            common::tree(&project)? == original && common::tree(&destination)? == artifacts,
            "public recovery changed original inputs or removed crash evidence",
        )?;
        evidence["phase"] = "verified".into();
        evidence["cleanupEvidenceComplete"] = true.into();
        evidence["originalProjectFilesUnchanged"] = true.into();
        evidence["allObservedProcessesExited"] = true.into();
        evidence["staleLeaseRecovered"] = true.into();
        evidence["registryClean"] = true.into();
        evidence["reportsPreserved"] = true.into();
        common::write_json(&evidence_path, &evidence)?;
        self.reports.push(
            json!({"name":"windows-controller-crash", "evidence":evidence_path,
            "nativeOwnerIsController":true,"forcedExitCode":exit,"campaignComplete":false,
            "observedCampaignStatus":summary["status"],"scoresWithheld":true,
            "activeNativeOwnerProtected":true,"allObservedProcessesExited":true,
            "staleLeaseRecovered":true,"registryClean":true,"reportsPreserved":true,
            "originalProjectFilesUnchanged":true}),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_checkpoint_withholds_completion_records_and_every_score() {
        let honest = json!({"status":"running","complete":false,"completedMutants":0,
            "records":[],"mutationScore":null,"failureModeScore":null,
            "languageSpecificScore":null});
        assert!(incomplete(&honest).is_ok());
        for (field, value) in [
            ("status", json!("completed")),
            ("status", json!("no_mutants")),
            ("complete", json!(true)),
            ("completedMutants", json!(1)),
            ("records", json!([{"outcome":"KILLED"}])),
            ("records", Value::Null),
            ("mutationScore", json!({"denominator":1,"percent":100})),
            ("failureModeScore", json!({"denominator":0})),
            ("languageSpecificScore", json!({"percent":0})),
        ] {
            let mut corrupted = honest.clone();
            corrupted[field] = value;
            assert!(
                incomplete(&corrupted).is_err(),
                "accepted fabricated {field}"
            );
        }
    }
}

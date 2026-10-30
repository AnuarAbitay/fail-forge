//! Real controller death while a Dart author test owns a live process tree.
//!
//! Windows has no separate Unix-style launcher/watchdog: the public controller
//! owns Runner's kill-on-close jobs. Its forced death must leave honest partial
//! evidence; the next public campaign, not a fabricated lease, recovers it.
use super::*;
use serde::Deserialize;
use std::collections::BTreeMap;
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

// Shared by the author and its child: an early grandchild failure must reach
// the outer reporter with its stderr, rather than becoming an opaque EOF.
const DART_READINESS: &str = r#"const stderrLimit = 8192;
const readinessLimit = 4096;

class StderrCapture {
  final bytes = BytesBuilder(copy: false);
  final done = Completer<void>();
  int total = 0;
  String? error;

  StderrCapture(Stream<List<int>> stream) {
    stream.listen((chunk) {
      total += chunk.length;
      final remaining = stderrLimit - bytes.length;
      if (remaining > 0) {
        bytes.add(chunk.sublist(0,
            chunk.length < remaining ? chunk.length : remaining));
      }
    }, onError: (Object failure) {
      final text = failure.toString();
      error = text.substring(0, text.length < 512 ? text.length : 512);
    }, onDone: () => done.complete());
  }

  Map<String, dynamic> snapshot() => {
    'bytesRead': total,
    'retainedBytes': bytes.length,
    'limitBytes': stderrLimit,
    'truncated': total > bytes.length,
    'complete': done.isCompleted,
    'error': error,
    'text': utf8.decode(bytes.toBytes(), allowMalformed: true),
  };
}

Future<String> readyLine(Process child) async {
  final bytes = BytesBuilder(copy: false);
  await for (final chunk in child.stdout) {
    for (final byte in chunk) {
      if (byte == 10) return utf8.decode(bytes.toBytes());
      if (bytes.length == readinessLimit) {
        throw StateError('readiness stdout exceeds its byte limit');
      }
      bytes.addByte(byte);
    }
  }
  throw StateError('readiness stdout closed without a complete line');
}

Future<Map<String, dynamic>> ready(Process child, {
  required String role,
  required String program,
  required List<String> arguments,
  required String helperUri,
}) async {
  // Drain immediately and keep draining after the retained prefix is full.
  // Success does not wait for stderr EOF: the process must stay at its barrier.
  final stderr = StderrCapture(child.stderr);
  Object? observedRole;
  Object? observedPid;
  try {
    final line = await readyLine(child).timeout(const Duration(seconds: 45));
    final value = jsonDecode(line);
    if (value is! Map<String, dynamic>) {
      throw StateError('readiness JSON is not an object');
    }
    observedRole = value['role'];
    observedPid = value['pid'];
    if (observedRole != role) {
      throw StateError('readiness role mismatch');
    }
    if (observedPid is! int || observedPid <= 0 || observedPid > 0xffffffff) {
      throw StateError('readiness VM pid is not a positive Windows process ID');
    }
    return value;
  } catch (failure) {
    Object exitCode;
    try {
      exitCode = await child.exitCode.timeout(const Duration(seconds: 2));
    } on TimeoutException {
      exitCode = 'still running after exit observation deadline';
    }
    try {
      await stderr.done.future.timeout(const Duration(seconds: 2));
    } on TimeoutException {
      // Preserve the available prefix and explicitly mark the drain incomplete.
    }
    Object helperExists;
    try {
      helperExists = File.fromUri(Uri.parse(helperUri)).existsSync();
    } catch (_) {
      helperExists = 'cannot inspect helper URI';
    }
    final text = failure.toString();
    throw StateError('crash helper readiness failed: ${jsonEncode({
      'role': role,
      'program': program,
      'arguments': arguments,
      'directory': Directory.current.path,
      'helperUri': helperUri,
      'helperExists': helperExists,
      'resolvedExecutable': Platform.resolvedExecutable,
      'pid': child.pid,
      'launcherPid': child.pid,
      'observedRole': observedRole,
      'observedPid': observedPid,
      'exitCode': exitCode,
      'failure': text.substring(0, text.length < 512 ? text.length : 512),
      'stderr': stderr.snapshot(),
    })}');
  }
}
"#;

// Every descendant waits on an owned pipe, rather than sleeping for an assumed
// startup interval. The author only announces readiness after both generations
// and a separate background watcher have announced their own PIDs.
const TREE_HELPER: &str = r#"Future<void> main(List<String> arguments) async {
  final role = arguments.single;
  if (role == 'child') {
    final program = Platform.resolvedExecutable;
    final helper = Platform.script.toString();
    final arguments = ['--disable-dart-dev', helper, 'grandchild'];
    final grandchild = await Process.start(program, arguments);
    final observed = await ready(grandchild, role: 'grandchild',
        program: program, arguments: arguments, helperUri: helper);
    stdout.writeln(jsonEncode({'role': role, 'pid': pid,
        'grandchildLauncherPid': grandchild.pid,
        'grandchildPid': observed['pid']}));
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

fn tree_helper_source() -> String {
    format!(
        "import 'dart:async';\nimport 'dart:convert';\nimport 'dart:io';\nimport 'dart:typed_data';\n\n{DART_READINESS}\n{TREE_HELPER}"
    )
}

fn author_source(port: u16, nonce: &str) -> Result<String> {
    let nonce = serde_json::to_string(nonce)?;
    Ok(format!(
        r#"import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';
import 'package:campaign_sample/value.dart';
import 'package:test/test.dart';

{DART_READINESS}

void main() {{
  test('{AUTHOR_TEST}', () async {{
    if (!keepGoing()) return;
    final helper = File('tool/crash_tree.dart').absolute.uri.toString();
    final program = Platform.resolvedExecutable;
    final childArguments = ['--disable-dart-dev', helper, 'child'];
    final child = await Process.start(program, childArguments);
    final tree = await ready(child, role: 'child', program: program,
        arguments: childArguments, helperUri: helper);
    final watcherArguments = ['--disable-dart-dev', helper, 'watcher'];
    final watcher = await Process.start(program, watcherArguments);
    final background = await ready(watcher, role: 'watcher', program: program,
        arguments: watcherArguments, helperUri: helper);
    final barrier = await Socket.connect(InternetAddress.loopbackIPv4, {port});
    barrier.writeln(jsonEncode({{'nonce': {nonce}, 'authorPid': pid,
        'childLauncherPid': child.pid, 'childPid': tree['pid'],
        'grandchildLauncherPid': tree['grandchildLauncherPid'],
        'grandchildPid': tree['grandchildPid'],
        'watcherLauncherPid': watcher.pid, 'watcherPid': background['pid']}}));
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
    child_launcher_pid: u32,
    child_pid: u32,
    grandchild_launcher_pid: u32,
    grandchild_pid: u32,
    watcher_launcher_pid: u32,
    watcher_pid: u32,
}

impl Ready {
    fn process_roles(
        &self,
        controller: u32,
        reporter: Option<u32>,
    ) -> Result<BTreeMap<u32, Vec<&'static str>>> {
        let vms = [
            self.author_pid,
            self.child_pid,
            self.grandchild_pid,
            self.watcher_pid,
        ];
        ensure(
            controller != 0
                && vms.iter().all(|pid| *pid != 0 && *pid != controller)
                && vms.into_iter().collect::<BTreeSet<_>>().len() == vms.len(),
            format!("crash fixture VM identities overlap: controller={controller}, VMs={vms:?}"),
        )?;
        let bindings = [
            ("author-child", self.child_launcher_pid, self.child_pid),
            (
                "author-grandchild",
                self.grandchild_launcher_pid,
                self.grandchild_pid,
            ),
            (
                "background-watcher",
                self.watcher_launcher_pid,
                self.watcher_pid,
            ),
        ];
        let mut launchers = BTreeSet::new();
        for (role, launcher, vm) in bindings {
            ensure(
                launcher != 0
                    && launcher != controller
                    && launchers.insert(launcher)
                    && (!vms.contains(&launcher) || launcher == vm),
                format!(
                    "invalid crash fixture identity: role={role}, pid={vm}, launcherPid={launcher}, controller={controller}"
                ),
            )?;
        }
        let mut roles: BTreeMap<u32, Vec<&'static str>> = BTreeMap::new();
        for (role, pid) in [
            ("controller-and-native-owner", controller),
            ("author-test-vm", self.author_pid),
            ("author-child-launcher", self.child_launcher_pid),
            ("author-child-vm", self.child_pid),
            ("author-grandchild-launcher", self.grandchild_launcher_pid),
            ("author-grandchild-vm", self.grandchild_pid),
            ("background-watcher-launcher", self.watcher_launcher_pid),
            ("background-watcher-vm", self.watcher_pid),
        ] {
            roles.entry(pid).or_default().push(role);
        }
        if let Some(pid) = reporter {
            ensure(
                pid != 0
                    && pid != controller
                    && (!roles.contains_key(&pid) || pid == self.author_pid),
                format!(
                    "invalid Dart reporter identity: role=dart-test-runner, pid={pid}, authorPid={}",
                    self.author_pid
                ),
            )?;
            roles.entry(pid).or_default().push("dart-test-runner");
        }
        Ok(roles)
    }
}

struct Process {
    roles: Vec<&'static str>,
    pid: u32,
    image: PathBuf,
    handle: OwnedHandle,
}
impl Process {
    fn open(roles: Vec<&'static str>, pid: u32) -> Result<Self> {
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
            roles,
            pid,
            image: std::ffi::OsString::from_wide(&name[..length as usize]).into(),
            handle,
        };
        ensure(
            process.alive()?,
            format!(
                "{} exited before the crash barrier",
                process.roles.join("/")
            ),
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
                self.roles.join("/"),
                self.pid
            )
            .into()),
            _ => Err(io::Error::last_os_error().into()),
        }
    }

    fn observation(&self) -> Value {
        json!({"roles":self.roles,"pid":self.pid,"image":self.image})
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
    observed.process_roles(owner.0.id(), None)?;
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
        fs::write(project.join("tool/crash_tree.dart"), tree_helper_source())?;
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
        // Windows dart.exe can wait for a separate dartvm.exe. Retain every
        // launcher and VM once, preserving role aliases on exec-style hosts.
        let processes = announced
            .process_roles(owner.0.id(), Some(runner_pid))?
            .into_iter()
            .map(|(pid, roles)| Process::open(roles, pid))
            .collect::<Result<Vec<_>>>()?;
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
                format!(
                    "{} exited before forced controller death",
                    process.roles.join("/")
                ),
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
                    json!({"roles":process.roles,"pid":process.pid,"exitCode":code,
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
            "retainedProcessHandles":evidence["retainedProcessHandles"].clone(),
            "processExits":evidence["processExits"].clone(),
            "processDrainWallSeconds":evidence["processDrainWallSeconds"].clone(),
            "processDrainBudgetSeconds":evidence["processDrainBudgetSeconds"].clone(),
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

    fn identities() -> Value {
        json!({"nonce":"fixture","authorPid":10,
            "childLauncherPid":21,"childPid":20,
            "grandchildLauncherPid":31,"grandchildPid":30,
            "watcherLauncherPid":41,"watcherPid":40})
    }

    #[test]
    fn launchers_and_vms_are_distinct_processes_with_only_known_aliases() {
        let split: Ready = serde_json::from_value(identities()).unwrap();
        let roles = split.process_roles(1, Some(10)).unwrap();
        assert_eq!(roles.len(), 8);
        assert_eq!(roles[&10], ["author-test-vm", "dart-test-runner"]);
        for pid in [1, 10, 20, 21, 30, 31, 40, 41] {
            assert!(roles.contains_key(&pid), "lost process {pid}");
        }
        assert_eq!(split.process_roles(1, Some(9)).unwrap().len(), 9);

        let mut same = identities();
        for (launcher, vm) in [
            ("childLauncherPid", "childPid"),
            ("grandchildLauncherPid", "grandchildPid"),
            ("watcherLauncherPid", "watcherPid"),
        ] {
            same[launcher] = same[vm].clone();
        }
        let same: Ready = serde_json::from_value(same).unwrap();
        let roles = same.process_roles(1, Some(10)).unwrap();
        assert_eq!(roles.len(), 5);
        assert_eq!(roles[&20], ["author-child-launcher", "author-child-vm"]);
        assert_eq!(roles[&30].len(), 2);
        assert_eq!(roles[&40].len(), 2);
    }

    #[test]
    fn crash_identities_reject_zero_controller_and_cross_role_aliases() {
        for (field, pid) in [
            ("authorPid", 0),
            ("authorPid", 1),
            ("childPid", 10),
            ("grandchildPid", 20),
            ("watcherPid", 30),
            ("childLauncherPid", 0),
            ("childLauncherPid", 1),
            ("childLauncherPid", 10),
            ("childLauncherPid", 30),
            ("childLauncherPid", 31),
            ("grandchildLauncherPid", 20),
            ("watcherLauncherPid", 21),
        ] {
            let mut value = identities();
            value[field] = json!(pid);
            let ready: Ready = serde_json::from_value(value).unwrap();
            assert!(ready.process_roles(1, None).is_err(), "{field}={pid}");
        }
        let ready: Ready = serde_json::from_value(identities()).unwrap();
        assert!(ready.process_roles(0, None).is_err());
        for reporter in [0, 1, 20, 21, 30, 31, 40, 41] {
            assert!(ready.process_roles(1, Some(reporter)).is_err());
        }
        for pid in [json!(-1), json!(4294967296_u64), json!(1.5), json!(true)] {
            let mut value = identities();
            value["grandchildPid"] = pid;
            assert!(serde_json::from_value::<Ready>(value).is_err());
        }
        let mut missing = identities();
        missing.as_object_mut().unwrap().remove("childLauncherPid");
        assert!(serde_json::from_value::<Ready>(missing).is_err());
    }

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

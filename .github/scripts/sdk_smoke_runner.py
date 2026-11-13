#!/usr/bin/env python3
"""Disposable, offline native-archive and real Dart quality-gate checks.

Run only on a prepared Linux/Windows host. This never builds, downloads, changes
the source checkout/SDK/old Pub cache, or sends credentials anywhere. The supplied
extracted archive is disposable: package-smoke prepares its adapter in place.

Use --real-gates-only --only-case all-admitted-completed for a focused two-worker
repeat without a developer-tool build. --expected-source-sha binds the checkout.
An explicit --product-source-sha permits reusing an older archive only when all
tracked product inputs still match; both revisions are retained in the receipt.
"""

import argparse
import ctypes
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import stat
import subprocess
import sys
import time
import traceback


EXE = ".exe" if os.name == "nt" else ""
CREDENTIAL_NAMES = {"credentials", "credentials.json", "pub-credentials.json"}
PRODUCT_INPUTS = ["Cargo.toml", "Cargo.lock", ".cargo", "crates", "adapters", "protocol", "packages", "third-party"]


def require(condition, message):
    if not condition:
        raise RuntimeError(str(message))


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, ensure_ascii=True) + "\n", encoding="utf-8")


def load(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def within(path, root):
    return path == root or root in path.parents


def comparison_path(path):
    value = str(path)
    if os.name == "nt":
        if value.startswith("\\\\?\\UNC\\"):
            value = "\\\\" + value[8:]
        elif value.startswith("\\\\?\\"):
            value = value[4:]
    return Path(value).resolve()


def link_or_reparse(path):
    metadata = path.lstat()
    return stat.S_ISLNK(metadata.st_mode) or bool(getattr(metadata, "st_file_attributes", 0) & 0x400)


def link_description(path):
    try:
        return os.readlink(path)
    except OSError:
        return "unfollowed reparse point"


def tree(root, skip_credentials=False):
    result = {}
    for directory, names, files in os.walk(root, followlinks=False):
        parent = Path(directory)
        for name in list(names):
            path = parent / name
            if link_or_reparse(path):
                names.remove(name)
                result[path.relative_to(root).as_posix()] = {"link": link_description(path)}
        for name in files:
            path = parent / name
            relative = path.relative_to(root).as_posix()
            if skip_credentials and name.lower() in CREDENTIAL_NAMES:
                # Do not even read the bytes of a credential-named old file.
                result[relative] = {"credentialNameSkipped": True}
            elif link_or_reparse(path):
                result[relative] = {"link": link_description(path)}
            else:
                metadata = path.stat()
                require(stat.S_ISREG(metadata.st_mode), "Special input file: " + str(path))
                result[relative] = {"bytes": metadata.st_size, "sha256": digest(path)}
    return result


def git(source, *arguments):
    return subprocess.check_output(["git", "-c", "core.fsmonitor=false", "-C", str(source), *arguments], timeout=30)


def source_boundary(source):
    paths = git(source, "ls-files", "-z").decode("utf-8").rstrip("\0").split("\0")
    require(paths and paths != [""], "Empty source inventory")
    return {"revision": git(source, "rev-parse", "HEAD").decode().strip(),
            "status": git(source, "status", "--porcelain=v1", "--untracked-files=all").decode(),
            "files": {path: digest(source / path) for path in paths}}


def public_cache_roots(cache):
    roots = []
    for category in ("hosted", "hosted-hashes"):
        category_path = cache / category
        if category_path.exists():
            require(category_path.is_dir() and not link_or_reparse(category_path), "Public cache category is a link/reparse point")
        for server in ("pub.dev", "pub.dartlang.org"):
            candidate = cache / category / server
            if candidate.exists():
                require(candidate.is_dir() and not link_or_reparse(candidate), "Public cache root is not a real directory")
                roots.append((category + "/" + server, candidate))
    require(any(name.startswith("hosted/") for name, _ in roots), "No public hosted Pub cache was found")
    return roots


def old_cache_boundary(roots):
    return {name: tree(path, skip_credentials=True) for name, path in roots}


def copy_public_cache(roots, destination):
    destination.mkdir()
    copied = 0
    skipped = []
    for prefix, source in roots:
        target = destination / prefix
        target.mkdir(parents=True)
        for directory, names, files in os.walk(source, followlinks=False):
            parent = Path(directory)
            relative = parent.relative_to(source)
            for name in list(names):
                path = parent / name
                if link_or_reparse(path) or name.lower() in CREDENTIAL_NAMES:
                    names.remove(name)
                    skipped.append(prefix + "/" + (relative / name).as_posix())
                else:
                    (target / relative / name).mkdir(parents=True, exist_ok=True)
            for name in files:
                path = parent / name
                if link_or_reparse(path) or name.lower() in CREDENTIAL_NAMES:
                    skipped.append(prefix + "/" + (relative / name).as_posix())
                    continue
                require(path.is_file(), "Non-file in public cache")
                copied_path = target / relative / name
                copied_path.parent.mkdir(parents=True, exist_ok=True)
                # copy2 produces independent files; no hardlinks to old cache.
                shutil.copy2(path, copied_path)
                # Pub may refresh metadata in this private copy. A public cache
                # file's read-only attribute must not make that copy unusable.
                copied_path.chmod(copied_path.stat().st_mode | stat.S_IWUSR)
                copied += 1
    return {"publicRoots": [name for name, _ in roots], "copiedFiles": copied,
            "skippedLinksOrCredentialNames": skipped, "credentialFilesCopied": False}


class WindowsJob:
    """Owned Job Object; closing/terminating it affects only this command tree."""
    def __init__(self):
        from ctypes import wintypes as w

        class BasicLimit(ctypes.Structure):
            _fields_ = [("PerProcessUserTimeLimit", ctypes.c_longlong), ("PerJobUserTimeLimit", ctypes.c_longlong),
                        ("LimitFlags", w.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
                        ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", w.DWORD),
                        ("Affinity", ctypes.c_size_t), ("PriorityClass", w.DWORD), ("SchedulingClass", w.DWORD)]

        class IOCounters(ctypes.Structure):
            _fields_ = [(name, ctypes.c_ulonglong) for name in ("ReadOperationCount", "WriteOperationCount", "OtherOperationCount", "ReadTransferCount", "WriteTransferCount", "OtherTransferCount")]

        class ExtendedLimit(ctypes.Structure):
            _fields_ = [("BasicLimitInformation", BasicLimit), ("IoInfo", IOCounters),
                        ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
                        ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t)]

        class Accounting(ctypes.Structure):
            _fields_ = [(name, ctypes.c_longlong) for name in ("TotalUserTime", "TotalKernelTime", "ThisPeriodTotalUserTime", "ThisPeriodTotalKernelTime")] + [(name, w.DWORD) for name in ("TotalPageFaultCount", "TotalProcesses", "ActiveProcesses", "TotalTerminatedProcesses")]

        self.accounting_type = Accounting
        self.kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        self.kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, w.LPCWSTR]
        self.kernel.CreateJobObjectW.restype = w.HANDLE
        self.kernel.SetInformationJobObject.argtypes = [w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD]
        self.kernel.SetInformationJobObject.restype = w.BOOL
        self.kernel.AssignProcessToJobObject.argtypes = [w.HANDLE, w.HANDLE]
        self.kernel.AssignProcessToJobObject.restype = w.BOOL
        self.kernel.QueryInformationJobObject.argtypes = [w.HANDLE, ctypes.c_int, ctypes.c_void_p, w.DWORD, ctypes.c_void_p]
        self.kernel.QueryInformationJobObject.restype = w.BOOL
        self.kernel.TerminateJobObject.argtypes = [w.HANDLE, w.UINT]
        self.kernel.TerminateJobObject.restype = w.BOOL
        self.kernel.CloseHandle.argtypes = [w.HANDLE]
        self.kernel.CloseHandle.restype = w.BOOL
        self.handle = self.kernel.CreateJobObjectW(None, None)
        require(self.handle, "CreateJobObjectW failed: " + str(ctypes.get_last_error()))
        limits = ExtendedLimit()
        limits.BasicLimitInformation.LimitFlags = 0x2000  # KILL_ON_JOB_CLOSE
        if not self.kernel.SetInformationJobObject(self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            error = ctypes.get_last_error()
            self.close()
            raise OSError(error, "SetInformationJobObject failed")

    def assign(self, process):
        require(self.kernel.AssignProcessToJobObject(self.handle, int(process._handle)), "AssignProcessToJobObject failed: " + str(ctypes.get_last_error()))

    def resume(self, process):
        # Popen closed the primary thread handle, but the process is still
        # CREATE_SUSPENDED. Find and resume that one owned thread only after job
        # assignment, so no child can escape through a startup/assignment race.
        from ctypes import wintypes as w

        class ThreadEntry(ctypes.Structure):
            _fields_ = [("dwSize", w.DWORD), ("cntUsage", w.DWORD), ("th32ThreadID", w.DWORD),
                        ("th32OwnerProcessID", w.DWORD), ("tpBasePri", w.LONG),
                        ("tpDeltaPri", w.LONG), ("dwFlags", w.DWORD)]

        kernel = self.kernel
        kernel.CreateToolhelp32Snapshot.argtypes = [w.DWORD, w.DWORD]
        kernel.CreateToolhelp32Snapshot.restype = w.HANDLE
        kernel.Thread32First.argtypes = [w.HANDLE, ctypes.POINTER(ThreadEntry)]
        kernel.Thread32First.restype = w.BOOL
        kernel.Thread32Next.argtypes = [w.HANDLE, ctypes.POINTER(ThreadEntry)]
        kernel.Thread32Next.restype = w.BOOL
        kernel.OpenThread.argtypes = [w.DWORD, w.BOOL, w.DWORD]
        kernel.OpenThread.restype = w.HANDLE
        kernel.ResumeThread.argtypes = [w.HANDLE]
        kernel.ResumeThread.restype = w.DWORD
        snapshot = kernel.CreateToolhelp32Snapshot(0x4, 0)  # SNAPTHREAD
        require(snapshot not in (None, ctypes.c_void_p(-1).value), "Thread snapshot failed")
        threads = []
        try:
            entry = ThreadEntry()
            entry.dwSize = ctypes.sizeof(entry)
            more = kernel.Thread32First(snapshot, ctypes.byref(entry))
            while more:
                if entry.th32OwnerProcessID == process.pid:
                    threads.append(entry.th32ThreadID)
                more = kernel.Thread32Next(snapshot, ctypes.byref(entry))
        finally:
            kernel.CloseHandle(snapshot)
        require(len(threads) == 1, "Suspended child did not retain exactly one primary thread")
        thread = kernel.OpenThread(0x2, False, threads[0])  # SUSPEND_RESUME
        require(thread, "Opening the owned primary thread failed")
        try:
            require(kernel.ResumeThread(thread) == 1, "Resuming the owned primary thread failed")
        finally:
            kernel.CloseHandle(thread)

    def active(self):
        counters = self.accounting_type()
        require(self.kernel.QueryInformationJobObject(self.handle, 1, ctypes.byref(counters), ctypes.sizeof(counters), None), "QueryInformationJobObject failed")
        return counters.ActiveProcesses

    def terminate(self):
        require(self.kernel.TerminateJobObject(self.handle, 124), "TerminateJobObject failed")

    def close(self):
        if self.handle:
            self.kernel.CloseHandle(self.handle)
            self.handle = None


def prepare_supervisor():
    if os.name == "nt":
        from ctypes import wintypes as w
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.GetCurrentProcess.argtypes = []
        kernel.GetCurrentProcess.restype = w.HANDLE
        kernel.SetPriorityClass.argtypes = [w.HANDLE, w.DWORD]
        kernel.SetPriorityClass.restype = w.BOOL
        kernel.GetPriorityClass.argtypes = [w.HANDLE]
        kernel.GetPriorityClass.restype = w.DWORD
        handle = kernel.GetCurrentProcess()
        require(kernel.SetPriorityClass(handle, 0x4000), "SetPriorityClass BELOW_NORMAL failed")
        require(kernel.GetPriorityClass(handle) == 0x4000, "BELOW_NORMAL priority was not retained")
        return {"priority": "BELOW_NORMAL", "cleanup": "owned Windows Job Object"}
    require(sys.platform.startswith("linux"), "This runner is intended for Linux or Windows")
    os.nice(15)
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
    libc.prctl.restype = ctypes.c_int
    require(libc.prctl(36, 1, 0, 0, 0) == 0, "PR_SET_CHILD_SUBREAPER failed")
    return {"priority": os.getpriority(os.PRIO_PROCESS, 0), "cleanup": "own Linux subreaper descendants"}


def linux_descendants():
    processes = {}
    for directory in Path("/proc").iterdir():
        if not directory.name.isdigit():
            continue
        try:
            data = (directory / "stat").read_text()
            fields = data[data.rfind(")") + 2:].split()
            processes[int(directory.name)] = {"parent": int(fields[1]), "start": fields[19], "state": fields[0]}
        except (OSError, ValueError, IndexError):
            continue
    owned = {os.getpid()}
    while True:
        added = {pid for pid, row in processes.items() if row["parent"] in owned and pid not in owned}
        if not added:
            break
        owned.update(added)
    return {pid: processes[pid] for pid in owned if pid != os.getpid() and pid in processes}


def reap_orphans():
    if os.name != "nt":
        while True:
            try:
                if os.waitpid(-1, os.WNOHANG)[0] == 0:
                    break
            except ChildProcessError:
                break


def active_descendants(job):
    if job is not None:
        return job.active()
    return sum(row["state"] not in ("Z", "X", "x") for row in linux_descendants().values())


def force_owned_tree(process, job):
    if job is not None:
        job.terminate()
        # Handles assignment/resume failure too: an unassigned root is still
        # suspended and therefore has no children. Popen.kill uses its handle.
        if process.poll() is None:
            process.kill()
    else:
        for action, grace in ((signal.SIGTERM, 5), (signal.SIGKILL, 5)):
            snapshot = linux_descendants()
            for pid, row in sorted(snapshot.items(), reverse=True):
                current = linux_descendants().get(pid)
                if current and current["start"] == row["start"] and current["state"] not in ("Z", "X", "x"):
                    try:
                        os.kill(pid, action)
                    except ProcessLookupError:
                        pass
            until = time.monotonic() + grace
            while time.monotonic() < until and active_descendants(None):
                time.sleep(0.1)
            if not active_descendants(None):
                break
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        raise RuntimeError("Owned root process could not be reaped after termination")
    until = time.monotonic() + 15
    while time.monotonic() < until and active_descendants(job):
        time.sleep(0.1)
    require(active_descendants(job) == 0, "Owned command tree cleanup is unconfirmed")
    reap_orphans()


class Harness:
    def __init__(self, root, environment):
        self.root = root
        self.environment = environment
        self.commands = []

    def run(self, name, command, cwd, expected=0, deadline=600, extra_env=None):
        directory = self.root / "commands" / name
        directory.mkdir(parents=True)
        arguments = [str(argument) for argument in command]
        environment = dict(self.environment)
        environment.update(extra_env or {})
        receipt = {"name": name, "command": arguments, "cwd": str(cwd), "startedAt": now(),
                   "deadlineSeconds": deadline, "expectedExit": expected, "timedOut": False,
                   "pubCache": environment["PUB_CACHE"], "temporaryRoot": environment["TMPDIR"]}
        self.commands.append(receipt)
        write_json(self.root / "commands.json", self.commands)
        job = None
        process = None
        began = time.monotonic()
        try:
            job = WindowsJob() if os.name == "nt" else None
            with (directory / "stdout.log").open("wb") as stdout, (directory / "stderr.log").open("wb") as stderr:
                options = {"creationflags": subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.BELOW_NORMAL_PRIORITY_CLASS | 0x4} if os.name == "nt" else {"start_new_session": True}
                process = subprocess.Popen(arguments, cwd=cwd, env=environment, stdin=subprocess.DEVNULL,
                                           stdout=stdout, stderr=stderr, **options)
                receipt["pid"] = process.pid
                if job is not None:
                    job.assign(process)
                    job.resume(process)
                while process.poll() is None:
                    if time.monotonic() - began > deadline:
                        receipt["timedOut"] = True
                        try:
                            process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                        except OSError:
                            pass
                        grace = time.monotonic() + 20
                        while process.poll() is None and time.monotonic() < grace:
                            time.sleep(0.1)
                        force_owned_tree(process, job)
                        raise RuntimeError("Command exceeded its external deadline: " + name)
                    time.sleep(0.2)
                receipt["exit"] = process.wait()
                grace = time.monotonic() + 20
                while active_descendants(job) and time.monotonic() < grace:
                    time.sleep(0.1)
                if active_descendants(job):
                    force_owned_tree(process, job)
                    raise RuntimeError("Command root exited with live owned descendants: " + name)
                reap_orphans()
                receipt["processTreeCleanupConfirmed"] = True
                require(receipt["exit"] == expected, {"step": name, "exit": receipt["exit"], "expected": expected,
                    "stderrTail": (directory / "stderr.log").read_text(encoding="utf-8", errors="replace")[-6000:]})
                return receipt
        except BaseException as error:
            receipt["error"] = repr(error)
            if process is not None:
                try:
                    force_owned_tree(process, job)
                    receipt["processTreeCleanupConfirmed"] = True
                except BaseException as cleanup_error:
                    receipt["processTreeCleanupConfirmed"] = False
                    receipt["cleanupError"] = repr(cleanup_error)
            raise
        finally:
            if job is not None:
                job.close()
            receipt["finishedAt"] = now()
            receipt["wallSeconds"] = round(time.monotonic() - began, 3)
            if process is not None:
                receipt["exit"] = process.poll()
            write_json(directory / "command.json", receipt)
            write_json(self.root / "commands.json", self.commands)
            print(name + ": exit=" + str(receipt.get("exit")) + ", seconds=" + str(receipt["wallSeconds"]), flush=True)


def cleanup_evidence(summary, temporary):
    for key in ("originalProjectFilesUnchanged", "temporarySnapshotCleaned"):
        require(summary.get(key) is True, "Missing clean evidence: " + key)
    require(summary.get("workspaceCleanupDeferred") is False, "Workspace cleanup was deferred")
    require(summary.get("error") is None, "Campaign retained an execution error")
    paths = []
    if summary.get("workspaceDirectory"):
        paths.append(Path(summary["workspaceDirectory"]))
    attempts = [summary.get("baseline")] + summary.get("records", []) + summary.get("originalControls", [])
    for attempt in attempts:
        if not attempt:
            continue
        require(attempt.get("outcome") in ("KILLED", "SURVIVED"), "Unexpected execution outcome")
        require(attempt.get("temporaryCopyCleaned") is True, "Attempt copy cleanup unconfirmed")
        require(attempt.get("runner", {}).get("cleanup") == "complete", "Runner cleanup unconfirmed")
        if attempt.get("isolatedWorkingDirectory"):
            paths.append(Path(attempt["isolatedWorkingDirectory"]))
    for path in paths:
        require(within(comparison_path(path), comparison_path(temporary)), "Attempt escaped private temporary root: " + str(path))
        require(not path.exists(), "Temporary execution directory remains: " + str(path))
    require(summary.get("baseline", {}).get("outcome") == "SURVIVED", "Original baseline did not pass")
    require(len(summary.get("originalControls", [])) == 1 and summary["originalControls"][0].get("outcome") == "SURVIVED", "Final original did not pass")


def validate_all_admitted_capture(stdout, stderr, expected_marker):
    """Require the actual assertion after this campaign's barrier observation."""
    require(not stderr.strip(), "Unexpected stderr in the barrier mutant")
    try:
        events = [json.loads(line) for line in stdout.decode("utf-8").splitlines() if line.strip()]
    except (UnicodeError, ValueError) as error:
        raise RuntimeError("Incomplete Dart machine capture") from error
    require(events and all(isinstance(event, dict) for event in events), "Empty or invalid machine capture")
    require(events[0].get("type") == "start", "Missing Dart reporter start")
    terminal = [(index, event) for index, event in enumerate(events) if event.get("type") == "done"]
    require(len(terminal) == 1 and terminal[0][0] == len(events) - 1 and terminal[0][1].get("success") is False,
            "Missing or unexpected terminal Dart result")
    starts = [(index, event["test"]) for index, event in enumerate(events)
              if event.get("type") == "testStart" and isinstance(event.get("test"), dict)
              and event["test"].get("name") == "enabled"]
    require(len(starts) == 1, "Expected exactly one enabled test")
    start_index, test = starts[0]
    test_id = test.get("id")
    require(type(test_id) is int and test.get("metadata", {}).get("skip") is False,
            "Enabled test was invalid or skipped")
    markers = []
    for index, event in enumerate(events):
        if event.get("type") != "print":
            continue
        try:
            message = json.loads(event.get("message", ""))
        except (ValueError, TypeError):
            continue
        if isinstance(message, dict) and message.get("kind") == "failforge_gate_barrier_observed":
            markers.append((index, event, message))
    require(len(markers) == 1, "Missing or duplicate barrier observation")
    marker_index, marker_event, marker = markers[0]
    require(marker == expected_marker and all(type(marker.get(key)) is not bool for key in
            ["sequence", "killed", "survived", "remaining", "minScore", "maxPossiblePercent"]),
            "Barrier observation differs from the retained journal proof")
    require(marker_event.get("testID") == test_id, "Barrier belongs to another test")
    errors = [(index, event) for index, event in enumerate(events) if event.get("type") == "error"]
    require(len(errors) == 1, "Expected exactly one assertion failure")
    error_index, error = errors[0]
    require(error.get("testID") == test_id and error.get("isFailure") is True,
            "Mutant failed from an unexpected test or runtime error")
    require(re.sub(r"\s+", " ", error.get("error", "")).strip() == "Expected: true Actual: <false>",
            "Mutant did not fail the intended boolean assertion")
    require("expect" in error.get("stackTrace", "") and "value_test.dart" in error.get("stackTrace", ""),
            "Assertion stack does not identify the generated test")
    completions = [(index, event) for index, event in enumerate(events)
                   if event.get("type") == "testDone" and event.get("testID") == test_id]
    require(len(completions) == 1, "Missing or duplicate enabled test completion")
    end_index, end = completions[0]
    require(end.get("result") == "failure" and end.get("skipped") is False and end.get("hidden") is False,
            "Enabled test did not finish with the intended visible failure")
    require(start_index < marker_index < error_index < end_index < terminal[0][0],
            "Barrier, assertion and completion order is invalid")
    require(all(event.get("result") == "success" for event in events
                if event.get("type") == "testDone" and event.get("testID") != test_id),
            "Another test did not complete successfully")
    return {"barrierObserved": True, "assertionFailureConfirmed": True,
            "testId": test_id, "barrierMarker": marker}


def validate_all_admitted_report(summary, report, nonce):
    records = summary.get("records", [])
    require(len(records) == 2 and sorted(row.get("outcome", "") for row in records) == ["KILLED", "SURVIVED"],
            "Expected exactly one killed and one surviving admitted mutant")
    try:
        events = [json.loads(line) for line in (report / "events.jsonl").read_text(encoding="utf-8").splitlines() if line.strip()]
    except (UnicodeError, ValueError) as error:
        raise RuntimeError("Invalid retained gate journal") from error
    require(all(isinstance(event, dict) for event in events), "Invalid gate journal event")
    stops = [event for event in events if event.get("kind") == "quality_gate_stopped"]
    require(len(stops) == 1, "Missing or duplicate retained gate proof")
    stop = stops[0]
    expected = {"killed": 0, "survived": 1, "remaining": 1, "minScore": 80, "maxPossiblePercent": 50}
    summary_proof = summary.get("qualityGate", {}).get("proof")
    require(summary_proof == expected and all(type(summary_proof[key]) is not bool for key in expected),
            "Unexpected summary gate proof")
    require(all(type(summary_proof[key]) is int for key in ("killed", "survived", "remaining")),
            "Summary proof counts must be integers")
    require(stop.get("version") == 1 and type(stop.get("version")) is int
            and stop.get("runId") == summary.get("runId") and isinstance(stop.get("runId"), str)
            and stop.get("family") == "classic" and type(stop.get("sequence")) is int
            and stop["sequence"] > 0, "Gate journal identity differs from the campaign")
    require(all(stop.get(key) == value and type(stop.get(key)) is not bool for key, value in expected.items()),
            "Retained journal proof differs from the summary")
    require(all(type(stop.get(key)) is int for key in ("killed", "survived", "remaining")),
            "Journal proof counts must be integers")
    marker = {"kind": "failforge_gate_barrier_observed", "nonce": nonce, "family": "classic",
              "runId": stop["runId"], "sequence": stop["sequence"], **expected}
    killed = next(row for row in records if row["outcome"] == "KILLED")
    mutant_id = killed.get("mutantId", "")
    require(isinstance(mutant_id, str) and re.fullmatch(r"[a-f0-9]{64}", mutant_id), "Invalid killed mutant identity")
    capture = report / "mutants" / mutant_id / "capture"
    stdout, stderr = (capture / "stdout.bin").read_bytes(), (capture / "stderr.bin").read_bytes()
    require(hashlib.sha256(stdout).hexdigest() == killed.get("stdoutSha256"), "Killed mutant capture hash mismatch")
    proof = validate_all_admitted_capture(stdout, stderr, marker)
    return {**proof, "mutantId": mutant_id, "journalSequence": stop["sequence"]}


def make_project(root, adapter, unused, barrier):
    root.mkdir()
    (root / "lib").mkdir()
    (root / "test").mkdir()
    raw = (adapter / "pubspec.yaml").read_text(encoding="utf-8")
    prepared, replacements = re.subn(r"(?m)^name:\s*failforge_dart_adapter\s*$", "name: failforge_platform_sdk_gate", raw)
    require(replacements == 1, "Adapter package name could not be transformed unambiguously")
    (root / "pubspec.yaml").write_text(prepared, encoding="utf-8")
    shutil.copy2(adapter / "pubspec.lock", root / "pubspec.lock")
    source = "bool enabled() => true;\n" + "".join("bool unused%d() => true;\n" % index for index in range(unused))
    (root / "lib/value.dart").write_text(source, encoding="utf-8")
    if barrier:
        fixtures = Path(__file__).with_name("fixtures")
        shutil.copy2(fixtures / "real_gate_barrier.dart", root / "test/real_gate_barrier.dart")
        test = (fixtures / "real_gate_barrier_test.dart").read_text(encoding="utf-8")
    else:
        test = "import 'package:test/test.dart';\nimport 'package:failforge_platform_sdk_gate/value.dart';\nvoid main() { test('enabled', () => expect(enabled(), isTrue)); }\n"
    (root / "test/value_test.dart").write_text(test, encoding="utf-8")


def real_gate_cases(harness, installation, sdk, output, temporary, only_case=None):
    binary = installation / ("bin/failforge" + EXE)
    dart = sdk / ("bin/dart" + EXE)
    projects = output / "real-sdk-projects"
    projects.mkdir()
    large = projects / "five-mutants"
    admitted = projects / "two-admitted-mutants"
    selected_projects = ((admitted, 1, True),) if only_case == "all-admitted-completed" else ((large, 4, False), (admitted, 1, True))
    for project, unused, barrier in selected_projects:
        make_project(project, installation / "adapters/dart", unused, barrier)
        before_lock = digest(project / "pubspec.lock")
        harness.run(project.name + "-offline-pub-get", [dart, "--suppress-analytics", "pub", "get", "--offline", "--enforce-lockfile"], project, deadline=300)
        require(digest(project / "pubspec.lock") == before_lock, "Offline preparation changed the frozen lockfile")
    boundaries = {str(project): tree(project) for project, _, _ in selected_projects}
    write_json(output / "real-project-inputs.json", boundaries)
    results = []
    for name, project, workers, early, total, percent in (
        ("full-below-threshold", large, 1, False, 5, 20.0),
        ("early-unreachable", large, 1, True, 5, None),
        ("all-admitted-completed", admitted, 2, True, 2, 50.0),
    ):
        if only_case is not None and name != only_case:
            continue
        report = output / (name + "-report")
        args = [binary, "mutate", "--installation", installation, "--project", project, "--dart-sdk", sdk,
                "--family", "classic", "--operator", "BOOLEAN_LITERAL_FLIP", "--include", "lib/value.dart",
                "--workers", str(workers), "--min-score", "80", "--test-timeout", "120s",
                "--command-timeout", "180s", "--output", report]
        if early:
            args.append("--stop-when-unreachable")
        nonce = secrets.token_hex(16)
        environment = {"FF_PLATFORM_REAL_GATE_JOURNAL": str(report / "events.jsonl"),
                       "FF_PLATFORM_REAL_GATE_BARRIER_NONCE": nonce} if name == "all-admitted-completed" else {}
        harness.run(name, args, output, expected=1, deadline=900, extra_env=environment)
        summary = load(report / "summary.json")
        cleanup_evidence(summary, temporary)
        require(tree(project) == boundaries[str(project)], "Real SDK campaign changed its original project")
        require(summary["expectedMutants"] == total, "Unexpected discovery scope")
        require(summary["completedMutants"] == len(summary["records"]), "Retained record count mismatch")
        gate = summary["qualityGate"]
        require(gate["status"] == "failed" and gate["minScore"] == 80, "Unexpected gate verdict")
        require(gate["processedMutants"] == summary["completedMutants"], "Gate processed count mismatch")
        if name == "early-unreachable":
            require(summary["status"] == "quality_stopped" and summary["complete"] is False, "Early stop was not incomplete")
            require(summary["mutationScore"] is None, "Early stop exposed a final mutation score")
            require(2 <= summary["completedMutants"] <= 3, "Early stop did not suppress future queue admission")
            require(gate["reason"] == "threshold_unreachable" and gate["proof"] is not None, "Early stop proof missing")
            require(gate["maxPossiblePercent"] == 60.0, "Unexpected optimistic bound")
            require(gate["notRunMutants"] == total - summary["completedMutants"] > 0, "Unrun count mismatch")
        else:
            require(summary["status"] == "completed" and summary["complete"] is True, "All admitted work was marked incomplete")
            require(summary["completedMutants"] == total and gate["notRunMutants"] == 0, "Completed campaign skipped work")
            require(summary["mutationScore"]["percent"] == percent, "Final score changed")
            require(summary["mutationScore"]["denominator"] == total, "Final scored denominator changed")
            if name == "full-below-threshold":
                require(gate["reason"] == "below_threshold" and gate["proof"] is None, "Full campaign acquired an early-stop proof")
                require(summary["outcomeCounts"] == {"KILLED": 1, "SURVIVED": 4}, "Full outcomes differ")
            else:
                proof = gate["proof"]
                require(gate["reason"] == "threshold_unreachable" and proof is not None, "All-admitted barrier did not retain a proof")
                require(proof["killed"] == 0 and proof["survived"] == 1 and proof["remaining"] == 1, "Barrier did not prove stop-before-drain")
                require(gate["maxPossiblePercent"] == 50.0 and summary["outcomeCounts"] == {"KILLED": 1, "SURVIVED": 1}, "Drained outcomes differ")
        html = output / (name + ".html")
        barrier_proof = None
        if name == "all-admitted-completed":
            barrier_proof = validate_all_admitted_report(summary, report, nonce)
        harness.run(name + "-html", [binary, "report", report, "--html", html], output, deadline=60)
        require(html.is_file(), "Real SDK report export produced no HTML")
        results.append({"name": name, "status": summary["status"], "complete": summary["complete"],
                        "planned": total, "processed": summary["completedMutants"], "gate": gate,
                        "outcomeCounts": summary["outcomeCounts"], "score": summary["mutationScore"],
                        "sourceUnchanged": True, "cleanupConfirmed": True, "html": str(html),
                        "barrierProof": barrier_proof})
        write_json(output / "real-sdk-gates.json", results)
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installation", type=Path, required=True)
    parser.add_argument("--archive-installation", type=Path, required=True)
    parser.add_argument("--dart-sdk", type=Path, required=True)
    parser.add_argument("--pub-cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expected-source-sha", required=True, help="Exact clean checkout revision for this harness run")
    parser.add_argument("--product-source-sha", help="Reuse an archive from this revision only if all product inputs are unchanged")
    parser.add_argument("--only-case", choices=["all-admitted-completed"], help="Repeat just the two-worker barrier case")
    parser.add_argument("--archive-only", action="store_true", help="Run reliable offline archive SDK smoke without the additional three gate cases")
    parser.add_argument("--real-gates-only", action="store_true", help="Prepare only the fresh adapter offline, then run three real gate cases; a separate primary archive smoke already passed")
    options = parser.parse_args()
    require(not (options.archive_only and options.real_gates_only), "Choose only one partial-run mode")
    require(not options.only_case or options.real_gates_only, "--only-case requires --real-gates-only")
    require(not options.product_source_sha or options.real_gates_only,
            "Archive reuse is limited to real-gate checks; archive smoke requires the matching source")
    source = options.installation.resolve()
    installation = options.archive_installation.resolve()
    sdk = options.dart_sdk.resolve()
    cache = options.pub_cache.resolve()
    output = options.output.resolve()
    for protected in (source, installation, sdk, cache):
        require(protected.is_dir(), "Required prepared directory is unavailable: " + str(protected))
        require(not within(output, protected), "Output must be outside source, installation, SDK and old cache")
    for protected in (source, sdk, cache):
        require(not within(installation, protected), "The disposable extracted archive must be outside source, SDK and old cache")
    require(not output.exists(), "Output must be new")
    output.mkdir(parents=True)
    result = {"status": "running", "startedAt": now(), "platform": sys.platform,
              "source": str(source), "archiveInstallation": str(installation), "dartSdk": str(sdk),
              "oldPubCache": str(cache), "networkDownloadsRequested": False}
    before_source = before_sdk = before_cache = None
    roots = None
    try:
        result["supervisor"] = prepare_supervisor()
        tool = source / ("target/debug/failforge-tools" + EXE)
        if not options.real_gates_only:
            require(tool.is_file(), "Build the native developer tool before this runner")
        require((installation / ("bin/failforge" + EXE)).is_file(), "Archive executable is missing")
        require((sdk / ("bin/dart" + EXE)).is_file(), "Dart SDK executable is missing")
        before_source = source_boundary(source)
        require(not before_source["status"], "Source checkout must be clean")
        require(before_source["revision"] == options.expected_source_sha, "Harness checkout differs from expected source")
        manifest_source = load(installation / "package-manifest.json")["source"]
        product_revision = options.product_source_sha or before_source["revision"]
        require(re.fullmatch(r"[a-f0-9]{40}", product_revision), "Invalid expected product revision")
        require(manifest_source["revision"] == product_revision and manifest_source.get("dirty") is False,
                "Archive source binding differs from the expected clean product revision")
        require(git(source, "rev-parse", product_revision + "^{commit}").decode().strip() == product_revision,
                "Expected product revision is unavailable")
        require(not git(source, "diff", "--name-only", product_revision, before_source["revision"], "--", *PRODUCT_INPUTS),
                "Product inputs changed since the archive was built; rebuild the archive")
        result.update(harnessSourceSha=before_source["revision"], productSourceSha=product_revision,
                      productInputsUnchanged=True, selectedCase=options.only_case)
        before_sdk = tree(sdk)
        roots = public_cache_roots(cache)
        before_cache = old_cache_boundary(roots)
        write_json(output / "protected-inputs-before.json", {"source": before_source, "sdk": before_sdk, "publicOldCache": before_cache})
        result["cacheCopy"] = copy_public_cache(roots, output / "private-pub-cache")
        for name in ("temporary", "home", "appdata", "localappdata", "tool-state"):
            (output / name).mkdir()
        environment = os.environ.copy()
        for key in list(environment):
            if key.startswith("FAILFORGE_") or key.startswith("FF_CAMPAIGN_FIXTURE"):
                environment.pop(key, None)
        environment.update({"PUB_CACHE": str(output / "private-pub-cache"), "TMPDIR": str(output / "temporary"),
                            "TMP": str(output / "temporary"), "TEMP": str(output / "temporary"),
                            "USERPROFILE": str(output / "home"),
                            "APPDATA": str(output / "appdata"), "LOCALAPPDATA": str(output / "localappdata"),
                            "XDG_CONFIG_HOME": str(output / "tool-state"), "CI": "true", "DART_SUPPRESS_ANALYTICS": "true",
                            "FLUTTER_SUPPRESS_ANALYTICS": "true", "DASH__SUPPRESS_ANALYTICS": "true"})
        harness = Harness(output, environment)
        if options.real_gates_only:
            adapter = installation / "adapters/dart"
            before_lock = digest(adapter / "pubspec.lock")
            harness.run("adapter-offline-pub-get", [sdk / ("bin/dart" + EXE), "--suppress-analytics", "pub", "get", "--offline", "--enforce-lockfile"], adapter, deadline=300)
            require(digest(adapter / "pubspec.lock") == before_lock, "Fresh adapter preparation changed pubspec.lock")
            result["archiveSdkSmoke"] = {"status": "not_repeated", "reason": "separate primary archive smoke already passed"}
        else:
            harness.run("archive-sdk-smoke", [tool, "package-smoke", "--offline", "--installation", installation,
                        "--dart-sdk", sdk, "--output", output / "archive-sdk-smoke"], output, deadline=1500)
            smoke = load(output / "archive-sdk-smoke/summary.json")
            require(smoke.get("allPassed") is True and smoke.get("temporaryInputsRemoved") is True and smoke.get("ownerCleanupConfirmed") is True, "Native archive SDK smoke did not confirm completion and cleanup")
            result["archiveSdkSmoke"] = {"status": smoke["status"], "allPassed": True, "offline": smoke["offline"],
                                         "directYamlParity": smoke["results"]["directYamlParity"], "cleanupConfirmed": True,
                                         "summary": str(output / "archive-sdk-smoke/summary.json")}
        result["realDartGateCases"] = [] if options.archive_only else real_gate_cases(harness, installation, sdk, output, output / "temporary", options.only_case)
        result["commands"] = harness.commands
        result["status"] = "passed"
    except BaseException as error:
        result["status"] = "failed"
        result["error"] = repr(error)
        result["traceback"] = traceback.format_exc()
        raise
    finally:
        checks = {}
        for name, expected, current in (
            ("sourceUnchanged", before_source, lambda: source_boundary(source)),
            ("sdkUnchanged", before_sdk, lambda: tree(sdk)),
            ("oldPublicCacheUnchanged", before_cache, lambda: old_cache_boundary(roots)),
        ):
            if expected is not None:
                try:
                    actual = current()
                    checks[name] = actual == expected
                    if not checks[name]:
                        write_json(output / (name + "-after.json"), actual)
                        result["status"] = "failed"
                except BaseException as error:
                    checks[name] = False
                    result[name + "Error"] = repr(error)
                    result["status"] = "failed"
        result.update(checks)
        result["finishedAt"] = now()
        write_json(output / "verification.json", result)
    require(result["status"] == "passed", "Protected SDK/source/cache boundaries changed; see verification.json")
    print(json.dumps({"status": result["status"], "archiveSdkSmoke": result["archiveSdkSmoke"],
                      "realDartGateCases": len(result["realDartGateCases"]), "sourceUnchanged": result["sourceUnchanged"],
                      "sdkUnchanged": result["sdkUnchanged"], "oldPublicCacheUnchanged": result["oldPublicCacheUnchanged"]}, indent=2))


if __name__ == "__main__":
    main()

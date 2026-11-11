#!/usr/bin/env python3
"""Fresh path/hosted launcher acceptance. Python 3.9+, standard library only.

This never authenticates, publishes, changes tracked source, or reuses a runtime
cache. Native-control and fixture-only modes are explicit local harness checks;
neither mode establishes launcher acceptance and neither is exposed by CI.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import time


SOURCE = ("// Кошелёк α 🧪\r\n"
          "bool canWithdraw(int balance, int amount) {\r\n"
          "  /* 🧪 */ return balance >= amount;\r\n}\r\n")
TARGETS = {"Darwin": "aarch64-apple-darwin", "Linux": "x86_64-unknown-linux-gnu",
           "Windows": "x86_64-pc-windows-msvc"}
PAYLOAD_ROOT_FILES = {"README.md", "CHANGELOG.md", "LICENSE", "NOTICE", "pubspec.yaml"}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def tree(root):
    return {p.relative_to(root).as_posix(): sha256(p)
            for p in sorted(Path(root).rglob("*")) if p.is_file()}


def payload_tree(root):
    return {relative: digest for relative, digest in tree(root).items()
            if relative in PAYLOAD_ROOT_FILES or relative.startswith(("bin/", "lib/"))}


def generate_fixture(project, source_root):
    (project / "lib").mkdir(parents=True)
    (project / "test").mkdir()
    # Match the repository's locked test runner without adding a YAML dependency.
    lock = (source_root / "adapters/dart/pubspec.lock").read_text(encoding="utf-8")
    match = re.search(r'^  test:\n(.*?)(?=^  [^ ]|\Z)', lock, re.M | re.S)
    require(match is not None, "Cannot find the locked Dart test dependency")
    version = re.search(r'^    version: "([0-9]+\.[0-9]+\.[0-9]+)"$', match[1], re.M)
    require(version is not None, "Cannot identify the locked Dart test version")
    (project / "pubspec.yaml").write_text(
        "name: failforge_pub_acceptance\npublish_to: none\nenvironment:\n"
        "  sdk: '>=3.13.0 <4.0.0'\ndev_dependencies:\n  test: " + version[1] + "\n",
        encoding="utf-8")
    (project / "lib/кошелек.dart").write_bytes(SOURCE.encode("utf-8"))
    (project / "test/boundary_test.dart").write_text(
        "import 'package:test/test.dart';\n"
        "import 'package:failforge_pub_acceptance/кошелек.dart';\n"
        "void main() {\n  test('withdrawal boundary', () {\n"
        "    print('literal <script>globalThis.pubAcceptanceInjected = true</script>');\n"
        "    expect(canWithdraw(10, 10), isTrue);\n"
        "    expect(canWithdraw(9, 10), isFalse);\n"
        "    expect(canWithdraw(11, 10), isTrue);\n  });\n}\n", encoding="utf-8")
    return {"testVersion": version[1], "files": tree(project), "sourceCrlf": True}


class ReportParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.scripts = []
        self.current = None
        self.csp = None
        self.resource_refs = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag == "meta" and attrs.get("http-equiv", "").lower() == "content-security-policy":
            self.csp = attrs.get("content")
        for key in ("src", "href", "srcset"):
            if key in attrs and not attrs[key].startswith(("#", "data:")):
                self.resource_refs.append({"tag": tag, "attribute": key, "value": attrs[key]})
        if tag == "script":
            self.current = {"attrs": attrs, "text": ""}

    def handle_data(self, data):
        if self.current is not None:
            self.current["text"] += data

    def handle_endtag(self, tag):
        if tag == "script" and self.current is not None:
            self.scripts.append(self.current)
            self.current = None


class Acceptance:
    def __init__(self, args):
        self.args = args
        self.source = args.source_root.resolve()
        if args.version is None:
            spec = (self.source / "packages/failforge/pubspec.yaml").read_text(encoding="utf-8")
            match = re.search(r"^version:\s*(\S+)\s*$", spec, re.M)
            require(match is not None, "Source package version is missing")
            args.version = match[1]
        self.native_version = args.native_version or args.version
        self.output = args.output.resolve()
        require(not self.output.exists(), "Output already exists; use a fresh acceptance directory")
        self.output.mkdir(parents=True)
        self.logs = self.output / "commands"
        self.logs.mkdir()
        self.steps = []
        self.result = {"status": "running", "scope": "launcher-acceptance", "mode": args.mode,
                       "version": args.version, "platform": platform.platform(),
                       "python": sys.version.split()[0], "publicationAttempted": False}
        self.env = os.environ.copy()
        self.cache_root = (args.cache_root or (self.output / "caches")).resolve()
        require(self.cache_root != self.source and self.cache_root not in self.source.parents,
                "Cache root may not contain the source checkout")
        self.cache_root.mkdir(parents=True, exist_ok=True)
        for key, folder in (("PUB_CACHE", "pub-cache"), ("DART_DATA_HOME", "dart-data"),
                            ("FAILFORGE_CACHE", "runtime-cache")):
            path = self.cache_root / folder
            if key != "DART_DATA_HOME":
                require(not path.exists() or not any(path.iterdir()), "Acceptance cache is not fresh: " + key)
            else:
                require(not (path / "install").exists(), "Dart native install directory is not fresh")
            self.env[key] = str(path)
        self.temporary = self.cache_root / "temporary"
        self.temporary.mkdir(exist_ok=False)
        for key in ("TMPDIR", "TMP", "TEMP"):
            self.env[key] = str(self.temporary)
        self.env.update({"DART_SUPPRESS_ANALYTICS": "true", "FLUTTER_SUPPRESS_ANALYTICS": "true",
                         "CI": "true", "PUB_HOSTED_URL": "https://pub.dev"})
        self.result["isolatedCaches"] = {k: self.env[k] for k in ("PUB_CACHE", "DART_DATA_HOME", "FAILFORGE_CACHE")}

    def save(self):
        write_json(self.output / "commands.json", self.steps)
        write_json(self.output / "verification.json", self.result)

    def run(self, name, arguments, cwd=None, expected=0, timeout=420, env=None):
        arguments = [str(item) for item in arguments]
        stdout = self.logs / (name + ".stdout.log")
        stderr = self.logs / (name + ".stderr.log")
        command = arguments
        if os.name == "nt" and arguments[0].lower().endswith((".bat", ".cmd")):
            # cmd /s removes the outer pair; every controlled argument is quoted.
            for arg in arguments:
                require(not re.search(r'["\r\n%&|<>^]', arg), "Unsupported CMD metacharacter in owned argument")
            quoted = " ".join('"' + arg + '"' for arg in arguments)
            command = subprocess.list2cmdline([os.environ.get("COMSPEC", "cmd.exe"), "/d", "/v:off", "/s", "/c"]) + ' "' + quoted + '"'
        started = time.monotonic()
        timed_out = False
        with stdout.open("wb") as out, stderr.open("wb") as err:
            kwargs = {"cwd": str(cwd or self.output), "env": env or self.env,
                      "stdin": subprocess.DEVNULL, "stdout": out, "stderr": err}
            if os.name == "nt":
                kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
            else:
                kwargs["start_new_session"] = True
            process = subprocess.Popen(command, **kwargs)
            try:
                code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                if os.name == "nt":
                    subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"],
                                   stdout=err, stderr=err, check=False)
                else:
                    os.killpg(process.pid, signal.SIGKILL)
                code = process.wait(timeout=30)
        self.steps.append({"name": name, "arguments": arguments, "cwd": str(cwd or self.output),
                           "exitCode": code, "expectedExitCode": expected,
                           "seconds": round(time.monotonic() - started, 3), "timedOut": timed_out,
                           "stdout": str(stdout), "stderr": str(stderr)})
        self.save()
        print("{}: exit={} seconds={}".format(name, code, self.steps[-1]["seconds"]), flush=True)
        require(not timed_out, name + " exceeded its owned command deadline")
        require(code == expected, name + " failed; see the saved command logs")
        return stdout.read_text(encoding="utf-8", errors="replace")

    def release_guard(self):
        require(re.fullmatch(r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)', self.args.version),
                "Expected an exact stable release version")
        if self.args.source_sha:
            require(re.fullmatch(r'[0-9a-f]{40}', self.args.source_sha), "Expected immutable source SHA")
            actual = self.run("source-sha", ["git", "rev-parse", "HEAD"], cwd=self.source).strip()
            require(actual == self.args.source_sha, "Checkout differs from expected source SHA")
            self.result["sourceSha"] = actual
            dirty = self.run("source-clean", ["git", "status", "--porcelain=v1", "--untracked-files=all",
                             "--", "packages/failforge"], cwd=self.source)
            require(not dirty.strip(), "Package source has uncommitted changes")
            tracked = self.run("source-tracked", ["git", "ls-files", "-z", "--", "packages/failforge"], cwd=self.source)
            self.tracked_package = {name: sha256(self.source / name) for name in tracked.split("\0") if name}
        package = self.source / "packages/failforge"
        self.source_payload = payload_tree(package)
        require(len(self.source_payload) >= 10, "Source package payload is incomplete")
        spec = (package / "pubspec.yaml").read_text(encoding="utf-8")
        require(re.search(r'^version:\s*' + re.escape(self.args.version) + r'\s*$', spec, re.M),
                "Source launcher version differs from intended release")
        text = (package / "lib/src/release.dart").read_text(encoding="utf-8")
        self.pins = {}
        for target, body in re.findall(r"'([^']+)': ReleaseArtifact\((.*?)\n\s*\),", text, re.S):
            fields = {}
            for name in ("version", "sha256", "manifestSha256"):
                match = re.search(name + r":\s*'([^']+)'", body)
                require(match is not None, "Incomplete native release pin")
                fields[name] = match[1]
            require(re.fullmatch(r'(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)', fields["version"]),
                    "Native runtime version must be an exact stable SemVer")
            for name in ("sha256", "manifestSha256"):
                require(re.fullmatch(r'[0-9a-f]{64}', fields[name]) and fields[name] != "0" * 64,
                        "Native digest is missing or a placeholder")
            self.pins[target] = fields
        require(set(self.pins) == set(TARGETS.values()), "Need native pins for all three supported targets")
        native_versions = {pin["version"] for pin in self.pins.values()}
        require(len(native_versions) == 1, "Native versions differ between supported targets")
        self.native_version = next(iter(native_versions))
        if self.args.native_version:
            require(self.native_version == self.args.native_version, "Native pins differ from intended native version")
        self.result["nativeVersion"] = self.native_version
        self.result["packageSourcePayload"] = self.source_payload
        self.result["nativePins"] = self.pins

    def install(self):
        self.dart = Path(self.args.dart or shutil.which("dart") or "").resolve()
        require(self.dart.is_file(), "Dart executable not found")
        self.sdk = self.dart.parent.parent
        self.env["DART_SDK"] = str(self.sdk)
        self.env["PATH"] = str(self.dart.parent) + os.pathsep + self.env.get("PATH", "")
        self.run("dart-version", [self.dart, "--version"])
        if self.args.native_command:
            require(self.args.native_control and not os.environ.get("GITHUB_ACTIONS"),
                    "Native command override requires explicit local --native-control")
            self.result.update({"scope": "native-control-only", "launcherAcceptance": False,
                                "nativeVersion": self.native_version})
            executable = self.args.native_command.resolve()
            require(executable.is_file(), "Native control executable is missing")
            self.launcher = [str(executable)]
            self.native_root = executable.parent.parent
            self.pins = {}
        else:
            self.release_guard()
            package = self.source / "packages/failforge"
            if os.name == "nt":
                descriptor = ("failforge@{path: " + json.dumps(package.as_posix()) + "}"
                              if self.args.mode == "path" else "failforge@" + self.args.version)
                self.run("install", [self.dart, "--suppress-analytics", "install", descriptor], timeout=600)
                batch = Path(self.env["DART_DATA_HOME"]) / "install/bin/failforge.bat"
                require(batch.is_file(), "Native installer did not create the isolated batch command")
                self.launcher = [str(batch)]
            else:
                install = [self.dart, "--suppress-analytics", "pub", "global", "activate"]
                install += (["--source", "path", str(package)] if self.args.mode == "path"
                            else ["failforge", self.args.version])
                self.run("install", install, timeout=600)
                executable = Path(self.env["PUB_CACHE"]) / "bin/failforge"
                require(executable.is_file(), "Pub did not create the isolated launcher")
                self.launcher = [str(executable)]
            if self.args.mode == "hosted":
                installed = Path(self.env["PUB_CACHE"]) / "hosted/pub.dev" / ("failforge-" + self.args.version)
                require(installed.is_dir(), "Hosted package cache was not created")
                require(payload_tree(installed) == self.source_payload,
                        "Hosted package payload differs from the intended source")
                self.result["hostedPayloadMatchesSource"] = True
            self.run("setup", self.launcher + ["setup"], timeout=600)
            roots = [p for p in Path(self.env["FAILFORGE_CACHE"]).iterdir()
                     if p.is_dir() and (p / "package-manifest.json").is_file()]
            require(len(roots) == 1, "Expected exactly one fresh native runtime")
            self.native_root = roots[0]
            self.result["launcherAcceptance"] = True
        output = self.run("version", self.launcher + ["--version"])
        require(re.search(r'^failforge ' + re.escape(self.native_version) + r'\s*$', output, re.M),
                "Installed runtime returned the wrong version")
        self.result["launcherCommand"] = self.launcher
        self.result["nativeRuntime"] = self.verify_runtime()

    def verify_runtime(self):
        path = self.native_root / "package-manifest.json"
        manifest = json.loads(path.read_text(encoding="utf-8"))
        target = TARGETS.get(platform.system())
        require(target is not None and manifest["target"] == target, "Unexpected native target")
        require(manifest["version"] == self.native_version, "Unexpected native manifest version")
        require(manifest["source"]["dirty"] is False and
                re.fullmatch(r'[0-9a-f]{40}', manifest["source"]["revision"]), "Native release source is not clean/immutable")
        if self.args.native_source_sha:
            require(manifest["source"]["revision"] == self.args.native_source_sha, "Wrong native source revision")
        if target in self.pins:
            require(sha256(path) == self.pins[target]["manifestSha256"], "Native manifest differs from launcher pin")
        for entry in manifest["files"]:
            relative = Path(entry["path"])
            require(not relative.is_absolute() and ".." not in relative.parts, "Unsafe native payload path")
            file = self.native_root / relative
            require(file.is_file() and file.stat().st_size == entry["bytes"] and sha256(file) == entry["sha256"],
                    "Installed native payload mismatch: " + entry["path"])
        return {"manifestSha256": sha256(path), "payloadFilesVerified": len(manifest["files"]),
                "source": manifest["source"], "target": target, "version": manifest["version"]}

    def verify_campaign(self, report, profile=False):
        summary = json.loads((report / "summary.json").read_text(encoding="utf-8"))
        family = summary["families"]["classic"]["summary"] if profile else summary
        for value in (summary, family):
            require(value["complete"] is True and value["status"] == "completed" and value["error"] is None,
                    "Campaign did not finish cleanly")
            require(value["originalProjectFilesUnchanged"] is True and value["temporarySnapshotCleaned"] is True
                    and value["workspaceCleanupDeferred"] is False, "Native source/cleanup invariant failed")
            workspace = Path(value["workspaceDirectory"])
            require(not workspace.exists(), "Native campaign workspace remains")
        require(family["allDiscoveredMutantsProcessed"] is True and family["completedMutants"] == 1,
                "Expected all discovered mutants, exactly one completed")
        require(family["outcomeCounts"] == {"KILLED": 1}, "Expected exactly one KILLED")
        require(family["mutationScore"]["percent"] == 100.0 and family["mutationScore"]["denominator"] == 1
                and family["mutationScore"]["killed"] == 1 and family["mutationScore"]["survived"] == 0,
                "Unexpected mutation score")
        records = family["records"]
        require(len(records) == 1 and summary["records"] == records, "Root/family records differ")
        record = records[0]
        require(record["operatorId"] == "RELATIONAL_BOUNDARY" and record["file"] == "lib/кошелек.dart",
                "Unexpected mutation selection")
        require(len(family["originalControls"]) == 1, "Expected one final original control")
        require(record["tests"]["started"] == 1 and record["tests"]["failed"] == 1,
                "KILLED mutation did not retain a failing author test")
        for attempt in [family["baseline"], record] + family["originalControls"]:
            require(attempt["reporterComplete"] is True and attempt["temporaryCopyCleaned"] is True,
                    "Attempt evidence incomplete or temporary copy remains")
            require(not Path(attempt["isolatedWorkingDirectory"]).exists(), "Attempt directory remains")
        require(family["baseline"]["tests"]["passed"] == 1 and
                all(x["tests"]["passed"] == 1 and x["tests"]["failed"] == 0 for x in family["originalControls"]),
                "Original controls were not healthy")
        edit = record["edit"]
        original = SOURCE.encode("utf-8")
        offset, length = edit["offsetBytes"], edit["lengthBytes"]
        require(original[offset:offset + length] == edit["expected"].encode("utf-8"), "Edit does not match source bytes")
        prefix = original[:offset].decode("utf-8")
        line = prefix.count("\n") + 1
        column = len(prefix.rsplit("\n", 1)[-1].encode("utf-16-le")) // 2 + 1
        context = record["sourceContext"]
        require(context["line"] == line and context["column"] == column and
                context["sourceHash"] == hashlib.sha256(original).hexdigest(), "Retained Unicode source context is wrong")
        return {"mutantId": record["mutantId"], "counts": family["outcomeCounts"],
                "score": family["mutationScore"], "expectedLocation": "lib/кошелек.dart:{}:{}".format(line, column),
                "nativeCleanupConfirmed": True, "sourceContext": context}

    def verify_html(self, html, campaign):
        text = html.read_text(encoding="utf-8")
        parser = ReportParser()
        parser.feed(text)
        require(not parser.resource_refs, "HTML has external resource references")
        for style in re.findall(r"<style[^>]*>(.*?)</style>", text, re.I | re.S):
            require(not re.search(r"@import\b", style, re.I), "HTML CSS imports a resource")
            for reference in re.findall(r"url\(([^)]+)\)", style, re.I):
                reference = reference.strip().strip("\"'")
                require(reference.startswith(("data:", "#")), "HTML CSS refers to an external resource")
        require(parser.csp is not None and "default-src 'none'" in parser.csp and
                "connect-src 'none'" in parser.csp and "base-uri 'none'" in parser.csp,
                "HTML CSP is missing expected restrictions")
        data_scripts = [s for s in parser.scripts if s["attrs"].get("id") == "failforge-report-data"]
        require(len(data_scripts) == 1, "Expected one embedded report JSON")
        data = json.loads(data_scripts[0]["text"])
        require(data["run"]["complete"] is True and len(data["families"]) == 1, "Embedded run is incomplete")
        family = data["families"][0]
        require(len(family["records"]) == 1 and family["score"]["percent"] == 100.0,
                "HTML counts or score differ from retained summary")
        require(family["records"][0]["mutantId"] == campaign["mutantId"] and
                family["records"][0]["sourceContext"] == campaign["sourceContext"], "HTML lost retained mutation/source data")
        executable = [s for s in parser.scripts if s["attrs"].get("type") != "application/json"]
        require(len(executable) == 1, "Unexpected HTML script structure")
        script_hash = base64.b64encode(hashlib.sha256(executable[0]["text"].encode("utf-8")).digest()).decode("ascii")
        require("'sha256-" + script_hash + "'" in parser.csp, "CSP hash does not match inline script")
        write_json(self.output / "html-embedded-data.json", data)
        return {"path": str(html), "bytes": html.stat().st_size, "sha256": sha256(html),
                "selfContained": True, "externalResourceReferences": parser.resource_refs,
                "inlineScriptCspHashVerified": True, "expectedLocation": campaign["expectedLocation"]}

    def browser(self, html, location):
        if self.args.browser == "off":
            return {"status": "not-run", "reason": "Explicit --browser off"}
        candidates = [os.environ.get("CHROME_BIN"), os.environ.get("EDGE_BIN"),
                      shutil.which("google-chrome"), shutil.which("chromium"), shutil.which("chromium-browser"),
                      "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
                      os.path.expandvars(r"%ProgramFiles(x86)%/Microsoft/Edge/Application/msedge.exe"),
                      os.path.expandvars(r"%ProgramFiles%/Microsoft/Edge/Application/msedge.exe")]
        browser = next((Path(p) for p in candidates if p and Path(p).is_file()), None)
        node = shutil.which("node")
        playwright = self.args.playwright_root or (Path(self.env["PLAYWRIGHT_ROOT"]) if self.env.get("PLAYWRIGHT_ROOT") else None)
        if browser is None or node is None or playwright is None:
            require(self.args.browser != "required", "Required system browser/Node/Playwright-core is unavailable")
            return {"status": "not-run", "reason": "Browser, Node or explicit Playwright-core root unavailable"}
        probe = Path(__file__).with_name("pub_browser_check.cjs")
        self.run("browser", [node, probe, playwright, browser, html, self.output / "browser", location], timeout=180)
        return json.loads((self.output / "browser/verification.json").read_text(encoding="utf-8"))

    def execute(self):
        project = self.output / "target package α"
        self.result["fixture"] = generate_fixture(project, self.source)
        if self.args.fixture_only:
            require(not os.environ.get("GITHUB_ACTIONS"), "Fixture-only mode is local only")
            self.result.update({"status": "passed", "scope": "fixture-generation-only", "launcherAcceptance": False})
            self.save()
            return
        self.install()
        self.run("subject-pub-get", [self.dart, "--suppress-analytics", "pub", "get"], cwd=project)
        self.run("subject-baseline", [self.dart, "--suppress-analytics", "test", "--concurrency", "1"], cwd=project)
        profile = project / "profiles/boundary.yml"
        self.run("profile-generate", self.launcher + ["profile", "generate", "--file", project / "lib/кошелек.dart",
                 "--family", "classic", "--operator", "RELATIONAL_BOUNDARY", "--output", profile], cwd=project)
        profile_report = self.output / "yaml-report"
        with profile.open("a", encoding="utf-8") as stream:
            stream.write("\noutput: " + json.dumps(str(profile_report), ensure_ascii=False) + "\n")
        source_before = tree(project)
        outcomes = []
        for name, report, config in (("direct", self.output / "direct-report", None), ("yaml", profile_report, profile)):
            args = self.launcher + ["mutate"]
            if config:
                args += ["--config", config]
            else:
                args += ["--project", project, "--family", "classic", "--operator", "RELATIONAL_BOUNDARY",
                         "--include", "lib/кошелек.dart", "--workers", "1", "--command-timeout", "60s",
                         "--test-timeout", "5s", "--output", report]
            self.run(name, args, cwd=project, timeout=600)
            require(tree(project) == source_before, "Campaign changed original project inputs")
            outcomes.append(self.verify_campaign(report, profile=config is not None))
        require(outcomes[0]["mutantId"] == outcomes[1]["mutantId"], "Direct/YAML selected different mutants")
        self.result["campaigns"] = outcomes
        self.result["directYamlParity"] = True
        retained = tree(self.output / "direct-report")
        html = self.output / "report.html"
        self.run("html", self.launcher + ["report", self.output / "direct-report", "--html", html], cwd=project)
        require(tree(self.output / "direct-report") == retained, "HTML export changed retained evidence")
        self.result["html"] = self.verify_html(html, outcomes[0])
        self.result["browser"] = self.browser(html, outcomes[0]["expectedLocation"])
        require(self.verify_runtime() == self.result["nativeRuntime"], "Runtime payload changed during acceptance")
        if not self.args.native_command:
            require(payload_tree(self.source / "packages/failforge") == self.source_payload,
                    "Acceptance changed package source files")
            if hasattr(self, "tracked_package"):
                require(all((self.source / name).is_file() and sha256(self.source / name) == digest
                            for name, digest in self.tracked_package.items()),
                        "Acceptance changed a tracked package file or lockfile")
        self.result.update({"status": "passed", "sourceInputsUnchanged": True,
                            "retainedInputsUnchanged": True, "nativePayloadUnchanged": True})
        self.save()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, default=Path.cwd())
    parser.add_argument("--source-sha")
    parser.add_argument("--native-source-sha")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cache-root", type=Path)
    parser.add_argument("--mode", choices=("path", "hosted"), default="path")
    parser.add_argument("--version", help="Expected launcher version; defaults to source pubspec")
    parser.add_argument("--native-version", help="Optional expected native version; defaults to launcher pins")
    parser.add_argument("--dart")
    parser.add_argument("--browser", choices=("auto", "required", "off"), default="auto")
    parser.add_argument("--playwright-root", type=Path)
    parser.add_argument("--fixture-only", action="store_true")
    parser.add_argument("--native-control", action="store_true")
    parser.add_argument("--native-command", type=Path)
    args = parser.parse_args()
    acceptance = None
    try:
        require(not args.native_control or args.native_command is not None, "Native-control requires a native executable")
        acceptance = Acceptance(args)
        acceptance.execute()
        print(json.dumps({"status": acceptance.result["status"], "scope": acceptance.result["scope"],
                          "verification": str(acceptance.output / "verification.json")}))
    except Exception as error:
        if acceptance is not None:
            acceptance.result.update({"status": "failed", "error": str(error)})
            acceptance.save()
        print("Acceptance failed: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

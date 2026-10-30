# Regression matrix

This is the scenario index for Linux x86_64, macOS arm64 and Windows x64.
It links the executable checks to their expected results. A test being listed
does not establish that it passed on a particular operating system or revision.
Use the verification records below and the exact commit's native CI results.

## Automated scenarios

`SDK regression` below means `failforge-tools regression`. Its `--suite` values
select the groups shown in the table; the default `all` includes every group
available on that host. Both native executables and developer examples must be
built in the directory passed through `--binary-dir`.

| ID | Scenario and expected result | Platforms and SDKs | Executable check |
| --- | --- | --- | --- |
| R01 | Native process arguments, stdin, environment, working directory, output and exit status preserve the platform contract. | Linux/macOS/Windows; no Dart SDK required. | [Runner tests](../crates/failforge-runner/tests/); [Windows argv, PATH, capture and exit checks](../crates/failforge-runner/tests/windows.rs). |
| R02 | Cancellation and outer deadlines terminate the owned process tree; dropping a public handle preserves cleanup ownership. | Linux/macOS/Windows. Windows uses Job Objects; Unix uses process groups. | [Runner tests](../crates/failforge-runner/tests/), including the native Windows cancellation/timeout/handle-drop cases. |
| R03 | Working copies preserve original bytes and rebase prepared package metadata; changed or linked inputs are rejected. | Linux/macOS/Windows; Windows requires local NTFS. | [Workspace tests](../crates/failforge-workspace/src/); [Windows copy cases](../crates/failforge-workspace/src/copy_windows_tests.rs). |
| R04 | An active workspace lease prevents recovery; released stale leases are recovered. Invalid control files and replaced allocations are retained for diagnosis. | Linux/macOS/Windows. | [Windows lifecycle cases](../crates/failforge-workspace/src/lifecycle_windows_tests.rs); SDK regression `--suite recovery`. |
| R05 | Direct CLI and YAML select matching candidates; family switches, baseline errors, worker execution and final outcome counts follow the same contract. | Pinned/latest stable Flutter on all three hosts. | [SDK regression](../crates/failforge-tools/src/regression.rs), `--suite smoke`, `profiles`, `classification`. |
| R06 | A completed inner test timeout has verified test-failure evidence; an outer interruption remains `TIMEOUT` with complete cleanup and no fabricated kill. | Pinned/latest stable Flutter on all three hosts. | SDK regression `--suite timeouts`; [startup-budget and verdict assertions](../crates/failforge-tools/src/regression.rs). |
| R07 | Requested cancellation retains completed evidence, publishes an incomplete campaign without final score and confirms workspace/process cleanup. | Linux/macOS native signals; Windows private cancellation channel. | SDK regression `--suite cancellation`; [Windows adapter cancellation](../crates/failforge-dart/tests/campaign_cancellation_windows.rs). Real Windows console events are separate W01/W02 cases. |
| R08 | Replay reuses retained candidate IDs and verified inputs in fresh copies; changed source, baseline or transcript is rejected. Replay cancellation preserves cleanup evidence. | Pinned/latest stable Flutter on all three hosts. | [Replay regression](../crates/failforge-tools/src/regression_replay.rs), SDK regression `--suite replay`. |
| R09 | Profile generation validates applicability without executing author tests; Flutter campaigns distinguish killed, survived and statically invalid mutants. | Pinned/latest stable Flutter on all three hosts. | SDK regression `--suite applicability` and `--suite flutter`. |
| R10 | All 19 Dart semantic operators execute through the real CLI and VM. SDK acceptance is distinguished from the actual versions exercised. | Minimum Dart 3.13.0 on Linux; pinned Flutter 3.47.7 and latest stable Flutter on Linux/macOS/Windows. | [Semantic fixtures](../crates/failforge-tools/src/semantic.rs); [general CI](../.github/workflows/ci.yml); [Windows CI](../.github/workflows/windows.yml). Record the resolved latest-stable version from the run. |
| R11 | Profile and campaign publication preserve complete generations; changed bindings, sharing violations, concurrent publishers and staged substitutions cannot commit an inconsistent final summary. | Linux/macOS/Windows. Windows reports use regular files. | [Windows profile output tests](../crates/failforge-profile/src/output_windows.rs); [Windows campaign publication tests](../crates/failforge-cli/src/campaign_publish_windows_tests.rs); corresponding Unix tests in those crates. |
| R12 | A native archive has the expected version, checksums and notices. Its relocated installation runs version/help checks, profile generation and campaigns with newly prepared Pub dependencies. | Linux x86_64/macOS arm64/Windows x64; pinned Flutter for release packages. | [Package tests](../crates/failforge-tools/src/package_tests.rs), [package smoke](../crates/failforge-tools/src/package_smoke.rs), [shared Windows packaging script](../.github/scripts/windows_release_package.ps1). |
| W01 | An actual console Ctrl+C cancels a live mutation campaign with exit 130, an incomplete report and confirmed cleanup. | Native Windows; pinned/latest stable Flutter. | [Isolated console regression](../crates/failforge-tools/src/regression_console_windows.rs), case `windows-console-ctrl-c`, included in `--suite cancellation` and `all`. Negative control `windows-console-ctrl-c-ignored` proves that event delivery alone cannot pass the graceful-cancellation assertions. |
| W02 | An actual console Ctrl+Break cancels a live mutation campaign with exit 143 and the same report/cleanup guarantees. | Native Windows; pinned/latest stable Flutter. | Same regression, case `windows-console-ctrl-break`. |
| W03 | Abrupt death of the campaign owner terminates live Dart descendants; retained evidence has no successful final score, originals remain unchanged, and the next real campaign recovers the abandoned workspace. | Native Windows; pinned/latest stable Flutter. | [Owner-death regression](../crates/failforge-tools/src/regression_crash_windows.rs), case `windows-controller-crash`, included in `--suite recovery` and `all`. This is distinct from the lease-only R04 case. |

W01–W03 are additions after the beta.4 baseline below. Their first native
Windows result must be recorded for the implementation commit; beta.4's green
checks are not evidence for these new scenarios. The console host is a
developer-only example and is excluded from distributed runtime archives.

## Running the automated groups

Run the native suite on the operating system under evaluation. Cross-target
type checking does not exercise console events, Job Object teardown or NTFS
recovery. The workflows are the source of truth for SDK installation and the
complete build/preparation commands:

- [Linux/macOS and minimum-Dart workflow](../.github/workflows/ci.yml).
- [Windows native Rust, SDK startup and Flutter workflows](../.github/workflows/windows.yml).

The Windows SDK jobs build both ordinary and release binaries and examples,
prepare adapter/project dependencies, then execute the SDK regression with
`--binary-dir target/release`. To select a group from that prepared checkout:

```powershell
& target/release/failforge-tools.exe regression `
  --suite cancellation `
  --binary-dir target/release `
  --dart-sdk C:/absolute/flutter/bin/cache/dart-sdk `
  --output C:/absolute/new-regression-output
```

Use `--suite recovery` for owner-death and stale-workspace scenarios, or omit
`--suite` for the complete catalogue. Every invocation needs a new output
directory. The final developer-tool `summary.json` contains the executed case
names; a missing case is not a pass. Keep command receipts and failure captures.

## Acceptance outside the automated fixtures

These are explicit acceptance tasks, not results implied by hosted CI:

| ID | Environment and scope | Required evidence | Current record |
| --- | --- | --- | --- |
| A01 | Clean, ordinary-user Windows 10 version 1809+ and Windows 11, x64/local NTFS; install only the documented VC++ runtime and compatible SDK, with no Rust/MSVC build tools. | Follow the downloaded-archive instructions, prepare a fresh Pub cache, move the installation to a path with spaces/Unicode, then run doctor, direct/YAML campaigns, profile generation and classic replay. | Not recorded for beta.4. Hosted `windows-latest` is a different environment. |
| A02 | Selected real standalone Dart and Flutter projects on each advertised platform, with pinned project/SDK commits. | Compare the selected discovery scope, patches, outcome interpretation and direct/YAML results; keep original hashes and cleanup evidence. Explain platform-dependent tests instead of assuming identical outcomes. | Current cross-platform release acceptance remains to be recorded. The historical macOS public-project series is listed separately below. |
| A03 | A small real-project scope with one and four workers, followed by console cancellation and a later recovery run. | Consistent selected scope and report counters; no unexplained infrastructure/inconclusive regressions, no remaining child processes or owned working copies, and unchanged originals. | Not established by the beta.4 fixture results alone. |

Pub workspaces, Windows ARM64/network shares and macOS Intel remain outside the
advertised verification scope. They are documented support boundaries, not
failures of a supported regression row. See [beta limitations](beta-limitations.md)
and [Windows requirements](windows.md#requirements).

## Verification records

- **Beta.4, 2026-10-28:** exact source
  [`08b57412bd68d5690741803ac5d602c1f495d2d2`](https://github.com/AnuarAbitay/fail-forge/commit/08b57412bd68d5690741803ac5d602c1f495d2d2).
  [General CI: 10 successful jobs](https://github.com/AnuarAbitay/fail-forge/actions/runs/40669568533),
  [Windows CI: four successful jobs](https://github.com/AnuarAbitay/fail-forge/actions/runs/40669568522),
  and [three native release packages](https://github.com/AnuarAbitay/fail-forge/actions/runs/40678476702).
  [Published archives and verification](https://github.com/AnuarAbitay/fail-forge/releases/tag/v0.1.0-beta.4)
  cover the existing automated rows, with their stated platform differences;
  W01–W03 were not yet implemented at that revision.
- **Historical macOS public-project run, 2026-10-20:** the selected 17 scenarios
  on 13 projects, including the separately labelled partial-cancellation case, are preserved in
  [known issues and verification history](known-issues.md#регрессия-после-исправления-ff-001-и-ff-002--macos-2026-10-20).
  They do not establish a Windows/Linux public-project result or a run of every
  archived mutation candidate.

For a new verification record, retain: source SHA and clean/dirty state, native
OS/architecture, resolved Rust/Dart/Flutter versions, selected row IDs and
commands, success/failure/skipped/not-run status, and CI run or local receipt
locations. Keep failed attempts alongside retries. A later green run does not
erase an unexplained earlier failure.

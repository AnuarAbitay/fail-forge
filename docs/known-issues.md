# Known issues and verification history

Historical status as of 2026-10-20. This document separates fixes with regression
tests from checks of earlier versions. The run counts, timings and before/after
observations below are maintainer-reported historical results. Their original
local logs and saved project inputs are not distributed in this repository, so
those results cannot be independently audited from this checkout. They do not
establish verification of the current revision or a later release.

## Checks available in the repository

The regression sources for the two cleanup races are tracked in Git:

- FF-001: [workspace fork/exec tests](../crates/failforge-profile/src/analysis/workspaces_fork_tests.rs),
  including `unrelated_fork_exec_lease_does_not_fail_cleanup`, real-owner,
  cancellation, recovery, identity-substitution and shared-deadline cases.
- FF-002: [Runner platform tests](../crates/failforge-runner/src/platform_tests.rs),
  including `permission_error_during_owned_exit_waits_for_terminal_delivery_proof`,
  deadline/error-preservation cases and explicit native macOS probes.
- Adapter cancellation: [campaign cancellation tests](../crates/failforge-dart/tests/campaign_cancellation.rs),
  including `buffered_cancellation_before_helper_start_is_not_lost`.

From the repository root, run the workspace regression group on Linux or macOS:

```sh
cargo test -p failforge-profile --lib --locked analysis::workspaces::fork_tests -- --test-threads=8
```

Run the macOS-specific Runner regressions and the ignored native transition
probe on macOS:

```sh
cargo test -p failforge-runner --lib --locked platform::tests
cargo test -p failforge-runner --lib --locked native_leader_term_kill_transition_census -- --ignored --nocapture
```

Run the adapter cancellation regression on Linux or macOS:

```sh
cargo test -p failforge-dart --locked --test campaign_cancellation buffered_cancellation_before_helper_start_is_not_lost
```

These commands exercise the current checkout; they do not reconstruct the
historical binaries, environments or repeat counts below. See the
[native verification tools](../crates/failforge-tools/README.md#regression-setup)
for prepared Dart/Flutter SDK requirements and fresh regression commands,
[public-project regression](../crates/failforge-tools/README.md#archived-public-projects)
for the manifest/input requirements of archived comparisons, and the tracked
[CLI process tests](../crates/failforge-cli/tests/mutate.rs) for campaign behavior.
The historical public-project manifest and prepared projects are not bundled.

## Fixed cleanup races

### FF-001 — transient cleanup failure during concurrent fork/exec

- **Status:** fixed, verified by reproduction and parallel tests.
- **Component:** [`Workspace::close()`](../crates/failforge-profile/src/analysis/workspaces.rs).
- **Symptom:** `workspace still has an active cleanup owner`. The earlier `analysis::workspaces::tests::cleanup_unlinks_payload_alias_without_touching_target` test failed in a parallel Rust suite.
- **Mechanism:** an unrelated child inherits a lock descriptor between `fork` and `exec`. `FD_CLOEXEC` closes it only at `exec`. The parent has already closed its descriptor, but an immediate nonblocking `flock` still sees the child's lock.
- **Previous impact:** cleanup was safely deferred, but concurrent process creation could produce a spurious error, including parallel profile API calls. This affected more than the test suite.
- **Historical reproduction:** a standalone Rust scenario using the unchanged module from `de09daeb` reproduced the exact error **3 out of 3 times**. After allowing `exec`, cleanup succeeded while the unrelated process was still running. The control without concurrent `fork` passed; symlink targets were unchanged.
- **Evidence limit:** the original failure did not record the lock owner's PID. Attributing that specific failure to an adjacent discovery test remains an inference; the mechanism and matching error were reproduced.
- **Fix:** acquiring the global lock and the lease shares one one-second wait budget. Only lock contention and `EINTR` are retried; other errors are retained. Descriptors and identities of directories, leases and control files are retained before waiting and checked before deletion. `LOCK_UN` is not called on an inherited file description.
- **Safety:** a real owner retains its working copy after the deadline. Cancellation does not skip required cleanup. After an explicit `close()` fails, the destructor makes only an immediate attempt without adding another second. A later explicit retry or recovery remains available. The budget limits lock waits, not the entire filesystem traversal.
- **Historical verification:** the new test failed on the previous implementation; after the fix, **10 parallel runs of 18 tests** and the entire profile module, **93 tests with 8 threads**, passed. Coverage included the real `fork/exec` window, a real owner, shared deadline, cancellation, recovery, substitutions during both wait phases and concurrent calls.
- **Startup recovery:** busy leases are still skipped without waiting for each campaign. The test that manually closes the parent's lease now accounts for brief inheritance by an adjacent process and additionally checks explicit recovery.

### FF-002 — repeated-signal error while a process exits on macOS

- **Status:** mechanism reproduced, fixed and checked through repeated native runs.
- **Scenario:** the adapter answered the handshake but delayed exit after EOF. When the close deadline expired, the owner requested the existing forced-cancellation path.
- **Observation:** SIGTERM succeeded; the following SIGKILL returned `errno: 1`. The process exited from SIGTERM, `child_reaped: true`, the group was `NoLiveMembers { zombies: 1 }`, and both streams were fully captured. The signal error kept `cleanup.is_complete()` at `false`.
- **Cause:** macOS already excludes an exiting process from group-signal recipients while `waitid(WNOWAIT)` does not yet expose terminal status. A single check rejected `EPERM` in this short interval. A native scenario reproduced it **500 out of 500 times**; this describes that reproduction, not the frequency experienced by users.
- **Fix:** waiting is allowed only after a complete initial census with no ordinary live recipients. Asynchronous retries must confirm terminal status of the owned child and a safe census within the grace/forced-wait budget already in progress. Live recipients, an unknown group, lost ownership and probe errors retain the original error.
- **Safety:** a separate error signalling the owned PID is not hidden by a later group result. A repeated force request interrupts the grace wait; signal time is recorded before waiting. `INEXIT` does not imply complete cleanup, and the `cleanup.is_complete()` conditions are unchanged.
- **Historical verification:** the deterministic test initially failed; after the fix, **500 out of 500** native transitions resolved correctly and **100 out of 100** early-cancellation repeats passed. Separate tests preserve real permission errors and check deadline expiration, census changes, yielding the Tokio thread and repeated cancellation.
- **Pre-fix observation, 2026-10-20:** a sequential suite after Python removal reproduced `PermissionDenied` with `NoLiveMembers { zombies: 1 }` in `buffered_cancellation_before_helper_start_is_not_lost`. A repeated full suite passed; that did not establish elimination of the race.
- **Pre-fix series after fixing the separate adapter-response bug:** of 20 early-cancellation repeats, 16 passed and 4 hit FF-002; `unsolicited_output` did not recur. This characterizes that local series, not user-facing frequency.

## Regression after FF-001 and FF-002 — macOS, 2026-10-20

- Full Rust suite: **1278 passed, 0 failed, 13 ignored** on stable and separately
  on minimum supported Rust **1.85.1**. Each total comprised **1213** production-crate
  tests and **65** `failforge-tools` tests; repeated checks are not included.
- **13 ignored** refers to tests disabled by default. Product tests comprised
  four retained JSON oracles, two native probes and three SDK scenarios;
  SDK scenarios were registered twice through an example and its including binary.
  All nine unique scenarios were run explicitly. The additional tools test
  `interrupt_probe` is a subprocess fixture: a normal test invokes it for
  SIGINT/SIGTERM at three phases. Retained corpora were checked without Python.
- Dart adapter: **429 passed, 0 skipped**, including the scaling probe.
- Real Dart/Flutter SDK regression: **54 out of 54 scenarios**, **788.50 s**.
  Checks covered direct/YAML execution, mutation families, configuration errors,
  initialization, timeouts, cancellation, crash recovery, applicability, Flutter
  and replay. After subsequent fixes confined to the verification tool,
  production-binary SHA-256 hashes still matched this SDK series.
- Semantic VM fixtures: all **19** operators, **19 original programs and
  23 mutants**, **17.13 s**. `OBSERVED`/`UNCHANGED` describe fixture observations,
  not a project's mutation score.
- External projects: **17 out of 17 scenarios on 13 projects**, sequentially,
  **3461.31 s (57 min 41 s)**. All **184 mutant executions** in completed scopes
  matched the archive: **134 KILLED, 33 SURVIVED, 12 INVALID, 5 TIMEOUT**, with
  no `INFRA_ERROR`. This counts executions, including CLI/YAML repetitions,
  rather than unique mutants.
- A separate `characters` scenario confirmed the entire selected discovery set
  of **1127 candidates** and normal cancellation after the first result.
  One `SURVIVED` was retained, with status `cancelled` and no final score.
  Originals, process cleanup and working-copy cleanup were checked in every
  scenario; source and binary SHA-256 hashes were unchanged after the series.
- The new Rust `public-regression` tool was also checked against real serialized
  product reports. Fixes addressed incorrect requirements for root-profile
  scores and completed-family scores in incomplete profiles, a missing check
  for absent selected-family summaries, and a false binary-change message when
  hash verification was interrupted. Tests failed before the fixes and passed
  after them; real mismatches and verification errors remain failures. The
  stopped first attempt and successful full series were recorded separately.

Scope: local verification covered macOS arm64; Linux and remote CI were not run
in this series. The main matrix selected historical scopes rather than rerunning
all **9606** archived candidates. It also did not establish exhaustive end-to-end
coverage of every one of the **31** classic operators. Characters used a partial
diagnostic fixture with a fixed seed; a cancelled campaign has no complete score.

## Rust migration completion

The public campaign, metadata, journal, initial reports and final reports run
under one Rust owner. An outer Rust process forwards signals; loss of its pipe
requests cancellation and waits for cleanup. The Python controller and bridges
were removed. Test Flutter installation, measurements and SDK regressions moved
to `failforge-tools`. The adapter and its analyzer remain Dart.
Retained classic candidates run through the same engine with `failforge replay`,
archive verification and fresh control runs.

FF-001 and FF-002 were deliberately deferred during migration completion.
They were fixed in the separate subsequent stage described above.
Current code and commands are documented in the [architecture](architecture.md)
and [verification tools](../crates/failforge-tools/README.md).

### Migration-completion verification — macOS, 2026-10-20

- Full sequential Rust suite: **1238 passed, 0 failed, 12 ignored**.
  After the last enum memory-layout change, all **344 unit tests and
  25 public CLI process tests** passed again.
- Dart adapter: **428 passed, 1 skipped**. Rust formatting, Clippy with
  `-D warnings`, Rust 1.85.1 checks, debug/release builds, doctests and
  documentation with warnings denied passed.
- Real SDK: **8 replay scenarios — 89.91 s**, **4 recovery scenarios — 41.91 s**,
  **19 semantic operators — 17.67 s**. The last set executed 19 originals and
  23 mutants; its `OBSERVED`/`UNCHANGED` values are VM-fixture results, not
  campaign mutation scores.
- Earlier in this series, the full SDK suite passed **46 scenarios in 734.69 s**.
  After the cancellation-protocol fix, 3 classification scenarios and
  2 cancellation scenarios were run separately. These were different recorded
  runs, not one run of every scenario on a single binary.
- Maintained Python files had already been removed during final verification.
  Old local reports and comparison corpora were retained outside tracked code.
- Linux/macOS GitHub CI and an actual SDK download through the new installer
  were not run in this local verification; offline tests checked the installer.
  FF-001 and FF-002 were still open at the time of these checks.

## Migration stages and historical checks

- Previous Dart isolation stage: `de09daeb`.
- Full attempt lifecycle, production discovery and full mutant lifecycle: `3547c6e3`.
- Family execution migration: `3381f967`.
- Unified campaign scheduler on a prepared snapshot: `7a6adf8f`.
- Full `campaign-run` lifecycle: `844500ab`.
- Final summaries, scores and tables in Rust: `0e7514a9`.
- Atomic final-file publication in Rust: `17ebeecb`.
- Live and final `progress.json` in Rust: `5435b214`.

The following records cover intermediate stages. Python suites describe earlier
versions, not commands supported by the current installation.

- After progress migration: Rust — **1108 passed, 11 ignored**, sequentially; Python — **735 passed, 1 skipped**; scripts — **314 passed, 2 skipped**. Added 23 Rust tests and 14 Python tests. Rustfmt, Clippy with warnings denied, Rust 1.85.1 and a release build passed.
- Real CLI (**36.18 s**), single-family YAML (**44.08 s**) and three-family profile (**111.04 s**) runs passed sequentially. The first two produced the same `KILLED`; the profile produced four `KILLED`. A zero cap (**32.85 s**) correctly returned `failed/125` before baseline. Progress matched final counts and statuses; JSON, tables, sources and cleanup were checked. These were correctness checks, not comparative benchmarks.
- After atomic-publication migration: Rust — **1085 passed, 11 ignored**, sequentially; Python — **721 passed, 1 skipped**; scripts — **314 passed, 2 skipped**. Rustfmt, Clippy with warnings denied, Rust 1.85.1 and a release build passed. Added 15 Rust publisher tests, one environment-cleanup test and 32 Python tests. The initial sandbox run could not create the existing CLI harness's Unix sockets; a full run outside the sandbox passed.
- With the Rust publisher, real CLI (**35.67 s**), single-family YAML (**44.87 s**) and three-family profile (**113.45 s**) runs passed sequentially. The first two produced the same `KILLED`; the profile produced four `KILLED`. A zero cap (**35.23 s**) correctly returned `failed/125` before baseline and published a separate incomplete generation without a score. JSON, tables, sources and cleanup were checked. These were correctness checks, not comparative benchmarks.
- After final-report migration: Rust — **1069 passed, 11 ignored**, sequentially; Python — **689 passed, 1 skipped**; scripts — **314 passed, 2 skipped**. Rustfmt, Clippy with warnings denied, Rust 1.85.1 and a release build passed.
- Final-report calculation migration was covered by 31 Rust tests and 24 Python tests included in the full totals. Checks covered all families and eight outcomes, JSON/table parity with the previous implementation, partial results, final-callback cancellation, formatter/publication errors, strict response schemas, numeric precision, 10,000 mutants and retention of details larger than 64 MiB without retransmitting them through the control channel.
- With the Rust formatter, real CLI (**38.80 s**), single-family YAML (**45.73 s**) and three-family profile (**119.31 s**) runs passed sequentially. The first two produced the same `KILLED`; the profile produced four `KILLED`. A zero cap (**33.88 s**) correctly returned `failed`/125 before baseline without a score. JSON, tables and mutant records agreed; sources were preserved and copies removed. These were correctness checks, not comparative benchmarks.
- After lifecycle migration: Rust — **1038 passed, 11 ignored**, sequentially; Python — **665 passed, 1 skipped**; scripts — **314 passed, 2 skipped**. Formatting, Clippy with warnings denied, Rust 1.85.1 and a release build passed.
- Added 22 lifecycle bridge/reporting tests. They covered the single-request contract without workspace descriptors, event boundaries and digests, bootstrap/cleanup errors, late-result retention, profile-report atomicity, manifest and `native_completed` errors, and stream interruption before cleanup: records are retained, scores are absent and cleanup is not treated as confirmed.
- Added 13 lifecycle process scenarios: both output-selection methods, classic without an explicit operator, three families, preflight without records, cancellation during preparation/execution, original/SDK changes, retention of unknown data after cleanup refusal, crash recovery and owner termination after controller SIGKILL. All passed in **20.40 s** using a synthetic SDK.
- After lifecycle migration, real CLI (**39.00 s**) and YAML (**47.64 s**) runs produced the same `KILLED`. Each used one `campaign-run`, without separate native workspace-inputs/workspace-lifecycle/discovery/campaign-execute/family/mutant/attempt calls. Tables matched JSON, sources were preserved and copies removed. These were correctness checks, not comparative benchmarks.
- A real profile through the full lifecycle passed in **119.69 s**: four candidates from three families, all `KILLED`. A zero global cap discovered the same four candidates in **33.80 s** and correctly stopped before baseline, without a score or mutant executions, with exit code 125. Both scenarios used one `campaign-run`; tables agreed with JSON, originals were preserved and cleanup completed.
- After scheduler migration: Rust — **1020 passed, 11 ignored**; Python — **630 passed, 1 skipped**; scripts — **314 passed, 2 skipped**. All Rust tests ran sequentially because FF-001 was deferred.
- Added 11 scheduler process scenarios: discovery/family order, a global cap including `INVALID`, malformed discovery responses, an incorrect component operator without candidates, both empty-campaign contracts, stopping after failure, cancellation with worker drain and late-result retention, and original/SDK changes. These used a synthetic SDK; real checks are described separately.
- After scheduler migration, real CLI (**38.22 s**) and YAML (**46.95 s**) runs produced the same `KILLED`. Each used exactly one `campaign-execute`, without separate native discovery/family/mutant/attempt calls. Tables matched JSON, sources were preserved and copies removed. These were individual correctness checks, not comparative benchmarks.
- A real three-family profile passed in **118.40 s**: `classic` — 1 candidate, `failure_mode` — 2, `language_specific` — 1, all four `KILLED`. All discovery finished before the first baseline; families ran sequentially. A repeat with `max_mutants: 0` discovered the same four candidates in **34.38 s** and correctly returned `failed`/125 before baseline and mutant execution. No score was present; both runs preserved sources, removed copies and produced tables consistent with JSON.
- Final sequential suite after family migration: Rust — **1001 passed, 11 ignored**; Python — **601 passed, 1 skipped**; scripts — **314 passed, 2 skipped**.
- Formatting, Clippy with warnings denied, Rust 1.85.1 and a release build passed.
- Added 10 family process scenarios: normal execution and static `INVALID`, baseline/final-control failures, both empty-family contracts, worker limits/concurrency, cancellation, progress-handler errors and SDK changes. Separate tests checked late-result retention in final CLI/YAML reports.
- After family migration, real CLI (**32.02 s**) and YAML (**39.52 s**) runs produced the same `KILLED`; each used one native discovery and one native family call. Separate native mutant/attempt calls were removed. Tables matched JSON, sources were preserved and copies removed. These were individual correctness checks, not comparative benchmarks.
- Causal confirmation within a native family on a real SDK passed in **52.25 s**: `SURVIVED → LOAD_FAILURE → SURVIVED`, one family owner, six distinct fresh copies including baseline, the initial attempt and final control. The test prohibited Python attempt/causal-control schedulers; sources were preserved and all copies removed.
- A real Dart SDK checked baseline → mutant → control, causal confirmation of initialization failure through three controls, and direct/YAML execution. Sources were preserved and working copies removed.
- After discovery migration, all three families were checked separately; results matched the previous independent journal validator. Full direct/YAML runs again produced the same `KILLED`, with one native discovery and three native attempts each; tables agreed with JSON, sources were preserved and copies removed.
- Added 10 full-mutant-lifecycle process scenarios: normal execution, static `INVALID`, forged patches, three causal controls and early stopping, report-path substitution, source changes, SIGTERM and parallel mutants. They passed with the full suite.
- After full-mutant-lifecycle migration, real CLI (**25.51 s**) and YAML (**34.27 s**) runs produced the same `KILLED`; each used one native discovery, one native mutant and two separate native attempts for baseline/final control. Causal confirmation on a real SDK passed in **43.69 s**: `SURVIVED → LOAD_FAILURE → SURVIVED`, one Rust mutant owner and five distinct fresh copies including baseline and the initial attempt. The test prohibited the old Python patch checks, causal-control scheduler and final-mutant writer. Copies were removed and sources preserved.
- Full comparative performance measurement of the new lifecycle and Linux checks were not performed at this stage. These are verification limits, not discovered defects.

## Other fixed defects

These historical defects should be reopened only with a new reproduction:

- Symlink substitution in the report-directory path: path-binding checks and canonical paths were added for capture files and the planner.
- Private reuse data survived an infrastructure failure or cancellation: data is now revoked, namespace re-preparation is serialized and plan artifacts are kept within a separate attempt.
- During full-mutant-lifecycle migration, a final source recheck hid an already recorded `INFRA_ERROR/source_input_changed`, returning 125 without a final record. A process test confirmed the regression; the fix retains authenticated diagnostics and revokes kill confirmation. A separate test checks source-error precedence over cancellation when no result is ready.
- During family migration, cancellation from a progress handler or `native_completed` could hide native execution failure or a missing terminal event. Regression tests confirmed the bug; process status and a verified terminal event now take precedence over such cancellation.
- An arbitrary progress-handler error, such as `RuntimeError`, escaped direct-campaign error handling. It is now retained in the final report with status `failed`, as in profile runs. Tests of both forms confirm late-result retention, absence of a score and snapshot cleanup.
- During scheduler migration, `verify_inputs` failure after a successful family lost its family association in the aggregate report. The regression test failed before the fix and passed afterward: the profile and affected family receive `failed`, without publishing a score.
- A discovery-event size-limit failure could leave an open phase in the scheduler stream. Size validation is now part of the phase that can complete; the boundary test checks balanced completion, prefix retention and a true final `failed` status.
- During lifecycle migration, general cleanup deleted a substituted snapshot directory after retained-inode validation had already refused its removal. A regression test reproduced deletion of a control file. Refusal of targeted cleanup now blocks the general reaper: `.cleanup-blocked` is created through the retained campaign descriptor, returning `cleanupDeferred: true` and `snapshotCleaned: false`. Later recovery also preserves the directory for inspection. Tests confirm retention of the replacement directory, moved snapshot, foreign registry and existing marker.
- At the new preflight → controller boundary, default classic operators expanded to a list although the earlier contract left the field absent. The contract was preserved; separate Rust, bridge and process tests cover `classic` without an explicit operator list.
- In the new publisher, uncertain-outcome verification could hang if a regular file was replaced with a FIFO between metadata validation and open. Reproduction confirmed the block; opening is now nonblocking, with type and identity checked before reading. The regression test checks refusal without reading the FIFO.
- Checking only the staged report's inode missed content changes. A failing Rust test confirmed the bug. File size, metadata and SHA-256 are now checked before publication.
- During profile verification, later hashing left time to change a previously checked file or directory. Two Rust tests and a Python uncertain-outcome test reproduced it. Descriptors are retained until commit; a final check of names, directory identities and each file's metadata follows all hashing. Review of these fixes found no further confirmed defects.
- In the new progress writer, hashing only after writing could accept substituted bytes as expected content. The hash is now derived from the original serialized value before filesystem operations; a regression test checks same-length substitution before the first stage verification.
- The new progress validator initially rejected incomplete reports with an unknown total mutant count and retained results. Rust direct/profile-report tests preserve that contract; successful completion still requires exact known totals.
- A simultaneous progress-write failure and SDK change let the progress message hide the original execution error. A process test failed before the fix and passed afterward. Existing execution errors now retain precedence; a progress error still prevents success and outranks cancellation, while cleanup errors outrank both.
- During early adapter-request cancellation, an already sent valid response could incorrectly become `unsolicited_output` at close. A fast Rust fixture exposed the existing window; a separate barrier scenario reproduced it. The client now retains the interrupted request's ID, command and consumed prefix, and validates at most one matching response during close. Duplicate, foreign, malformed or incomplete responses remain errors; the real cleanup result is retained. This was a separate protocol defect, not the FF-002 fix.
- In the new terminal output, a short queue could classify a healthy consumer as blocked during rapid progress events. Saturation now skips only an intermediate terminal update; the journal and final data remain complete. Tests cover 10,000 events, the final table, broken output and bounded waiting for a truly blocked consumer.
- A profile with a nonexistent literal `include` path returned infrastructure exit code `125`. Selection validation moved into configuration: the CLI returns `2` before starting the SDK or creating a report. Validation repeats before execution to detect file changes; glob selections and project-readiness errors retain their earlier contracts. Unit, public CLI and SDK scenarios confirmed this behavior.

These defects were reported as reproduced, fixed and checked at the historical
stages described above. Use the tracked tests and current verification commands
to assess a new revision.

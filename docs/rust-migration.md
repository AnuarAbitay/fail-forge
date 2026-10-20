# Rust migration map

The production entry points are `failforge mutate`, `failforge replay` and
`failforge profile generate`.
Configuration, campaign ownership, scheduling, workspaces, evidence, reports and
developer workflows are Rust. The Dart adapter remains Dart. The tracked Python
package, its interpreter option, PyYAML requirement and Python tests have been
removed; local historical evidence under ignored `artifacts/` is preserved.

This is a functional migration, not a translation of every old test function.
Many former tests checked Python-to-Rust transport that the in-process controller
no longer uses. Passing historical tests is not a percentage of current coverage,
and a smaller test count does not by itself establish equivalent coverage.

## Ownership and verification map

Paths in the first column refer to the former `python/failforge_campaign/` package;
old test names refer to `python/tests/`.

| Former responsibility | Rust implementation | Current verification |
| --- | --- | --- |
| `config.py`, `native_config.py`, `operator_catalogue.py`, `language_specific.py`; CLI/YAML and profile configuration tests | `failforge-config` (`normalize`, `yaml`, `paths`, `duration`, `catalogue`); CLI `args` and `mutate` | Config unit tests, CLI argument tests and public `tests/mutate.rs` reject unknown/disabled settings, wrong families, conflicting options and invalid timeouts before execution. |
| `cli.py`, `runtime.py`, `native_session.py`; campaign entry, signal and readiness tests | CLI `mutate`, `campaign_controller`, `campaign_display`, `campaign_session` | Public native SDK fixture tests cover execution with an empty `PATH`, closed stdin, SIGTERM, controller SIGKILL, retained late results and cleanup. Display and watchdog unit tests cover bounded output and parent-channel loss. |
| `engine.py`, `profile.py`, `native_campaign.py`, `native_family.py`; engine/profile and owner-process tests | CLI `campaign_execution`, `family_execution`, `family_queue` | Unit tests enforce canonical families, global limits, worker bounds, completion and failure precedence. Public CLI tests exercise direct/YAML runs, all discovery before baselines, independent family reports, empty selection, cancellation and multiple workers. |
| `input_files.py`, `workspace.py`, `native_inputs.py`; file/copy/runtime integrity tests | `failforge-workspace` (`inputs`, `copy`); CLI `workspace_inputs`, `campaign_workspace` | Tests cover bounded regular-file reads, FIFO/symlink rejection, exact source inventories, transient changes, dependency/tool bindings, metadata rebasing and unchanged originals. Public fixtures inject source and SDK drift. |
| `workspace_lifecycle.py`, `native_workspaces.py`; lifecycle, crash and cleanup tests | `failforge-workspace::lifecycle`; CLI `campaign_workspace` and `campaign_controller` | Native lifecycle tests cover leases, simultaneous reapers, interrupted commit points and foreign inode preservation. Public controller-death tests and `failforge-tools regression --suite recovery` exercise owner drain and stale-lease recovery. |
| `namespace.py`, `native_namespace.py`; namespace runtime tests | `failforge-dart::namespace`; CLI `dart_namespace`, `namespace_planner` | Namespace scanner/plan/isolation tests, a committed 72-case scanner oracle, adapter cancellation integration tests and public regression scenarios. |
| `native_discovery.py`; discovery/process tests | `failforge-dart::discovery`, `AdapterClient`; CLI `adapter_discovery` | Candidate/source proof tests, adapter integration tests, forged/truncated discovery rejection through public CLI and profile applicability scenarios. |
| `native_attempt.py`, `native_mutant.py`; attempt/mutant/process tests | CLI `attempt`, `attempt_execution`, `mutant_execution`, `mutant_confirmation`; `failforge-dart::attempt_workspace` | Unit/integration tests cover exact patches, static invalidity, capture and cleanup, report-directory guards and isolated fresh copies; public fixtures exercise completed, invalid, malformed, cancelled and concurrent attempts. |
| `native_evidence.py`, catalogue and initialization wrappers | `failforge-dart::test_reporter`, `test_outcome`, `test_catalog`, `initialization`; CLI `test_evidence` | Native policy/parser tests and `tests/native_test_evidence.rs` cover raw evidence and ordered causal controls. `regression --suite classification` checks registration failures and setup witnesses; `--suite timeouts` distinguishes inner test failures from outer Runner interruption. |
| `report.py`, `native_session_reports.py`, `native_report.py`; report/progress/scaling tests | CLI `campaign_reports`, `campaign_reports_fs`, `campaign_report`, `campaign_progress` | Tests cover exact schemas, independent scores, unknown versus zero totals, retained opaque evidence, 10,000-result compact accounting, late worker facts, journal order and failed reporting without a score. |
| `_ProfileReports`, `native_publish.py`; profile/final publication tests | CLI `campaign_publish`, `campaign_publish_fs`, `campaign_reports_fs` | Fault injection checks the final atomic commit, all precommit failures, partial retries, persistent progress failure, replaced directories/aliases/files, same-length content changes and payloads larger than the private JSON limit. |
| `terminal.py`; terminal/profile table tests | CLI `campaign_report_terminal` and `campaign_display` | Golden table, family/outcome/score, partial result, invalid input and bounded display tests. |
| Historical Python fixture and measurement scripts | `failforge-tools` (`regression`, `semantic`, `benchmark`, `installer`) and `failforge-config/examples/config_bench.rs` | Rust tool unit tests plus explicit SDK regression commands. SDK scenarios are sequential and require prepared dependencies; their completion must be established from a particular run's evidence. |
| `scripts/run_isolated_dart_replay.py`; retained transcript, archive baseline and isolated replay tests | `failforge replay`; CLI `retained_replay`; shared `failforge-dart::discovery::load_retained_transcript`; existing campaign/family/attempt owners | Strict transcript order, exact candidate set and regenerated source/patch proofs; baseline raw reporter and identity verification; retained file descriptors and hashes; source/framework drift and replacement tests. Replay uses two fresh original controls before the existing mutant queue, existing causal confirmations and a final original control, with normal atomic reports and cleanup. |

Profile generation already resides in `failforge-profile`, with preparation,
applicability, output-race and public CLI tests. It shares native workspace and
adapter facilities with campaigns. No legacy controller is retained as a fallback.

`failforge replay` accepts the former classic replay inputs: project, pristine
framework copy, transcript, baseline archive, output, worker count and timeout
policy. The framework is an exact upstream copy of the subject library, not the
`package:test` dependency. It is explicitly used by native namespace isolation.
The supported archived baseline has `runnerExitCode`, `runnerSummary`,
`outputSource` and `completedTestIdentities`, with sibling `provenance.json` and
`dependency-result.json`; optional `workingDirectory` identifies a historical
temporary capture directory. Replay reuses static-validation evidence only after
checking all source proofs; it does not rerun discovery or silently accept a
different candidate list. This uses the same native execution engine as `mutate`.

Classic summaries retain category/operator outcome groups and outcome/reason
and timeout-kind counts. Ordinary runs expose `notApplicableOperators` relative
to the actual filtered discovery scope; it does not claim that an operator is
inapplicable to every source file in the project. Replay instead exposes
`withoutRetainedCandidates`, derived from the archived candidate set even when
execution is interrupted. `allDiscoveredMutantsProcessed` records whether every
candidate has a result, independently of cleanup success or score eligibility.
Catalogues without an optional category use the explicit `unclassified` group.

## Preserved differential evidence

`crates/failforge-dart/tests/fixtures/namespace_scanner.json` remains committed and
is checked by the normal Rust suite. The independent semantic catalogue remains
in `protocol/fixtures/dart-semantic-catalog.json`.

Larger frozen JSONL oracles remain local, unchanged and ignored by Git:

| Environment variable for the explicit Rust comparison test | Local corpus | Cases |
| --- | --- | ---: |
| `FAILFORGE_EVIDENCE_CORPUS` | `artifacts/evidence-migration/2026-10-19/oracle-cases-byte-arrays.jsonl` | 232 |
| `FAILFORGE_CATALOGUE_CORPUS` | `artifacts/catalogue-migration/2026-10-19/oracle/cases.jsonl` | 71 |
| `FAILFORGE_INITIALIZATION_ORACLE` | `artifacts/initialization-migration/2026-10-19/oracle/cases.jsonl` | 88 |
| `FAILFORGE_INITIALIZATION_POLICY_ORACLE` | `artifacts/initialization-migration/2026-10-19/policy-oracle/cases.jsonl` | 114 |

Their SHA-256 values, in table order, are:

```text
49eb8a8548c39d59e62f05c8556abf91873167269bcf632e796fc73fb75f7525
a496a594edf87003fd1f77d4d822f623065de2a41381a4829800083dea6f4fef
5f885414f596a9766e70d502b0307527f8b7c171fd0798f84c22ae61a236dc64
35eea49aeb7bce007db9c255294ee5a74b78531619b0116d91dbc60ffd54322d
```

These optional comparison tests are ignored by default and require their corpus
path plus `--ignored`; a clean checkout does not contain those local files.
The evidence corpus uses the existing numeric-byte-array representation required
by the Rust reader. Its original base64 corpus and transformation receipt remain
alongside it; expected verdicts were not changed by that representation conversion.
Historical captures and Python oracle snapshots under `artifacts/` are evidence,
not a runtime, installed tool or required CI dependency. They were not deleted or
rewritten during removal of the tracked package.

The four Rust comparison tests were rerun on 2026-10-20 and passed. The evidence
corpus contains 230 applicable comparisons: 229 exact matches and one previously
documented diagnostic change (`integer_limit_exceeded` rather than a later
`invalid_event_time`), with the same `TIMEOUT` verdict. Two invalid Python input
types cannot be constructed through the typed Rust interface. The 71 catalogue,
88 initialization-evidence and 114 policy cases passed their comparisons. These
are corpus replay results, not live SDK campaign results.

## Limits of the migration evidence

Completion was verified locally on macOS on 2026-10-20: 1,238 Rust tests passed
with 12 ignored; 428 Dart adapter tests passed with one skipped. After the final
enum storage adjustment, all 344 CLI unit tests and 25 public campaign process
tests passed again. Formatting, Clippy with warnings denied, Rust 1.85.1 checking,
debug/release builds, doctests and documentation passed.

With the maintained Python files already absent, real-SDK checks passed eight
replay scenarios, four recovery scenarios and all 19 semantic operator fixtures
(19 passing originals and 23 mutants). The earlier full SDK regression passed
46 scenarios; subsequent classification and cancellation checks passed three and
two scenarios respectively. These are separate runs with retained binary
provenance, not one final-build run of every SDK scenario. Local evidence and
commands are under `artifacts/native-completion/2026-10-20/validation/README.md`.
No maintained `.py` files or Python requirements remain; historical Git commits
and ignored evidence are unchanged.

- The Python import/callback API is retired. The supported product interface is
  the CLI and YAML profiles; this migration does not introduce a drop-in Rust
  embedding API for `failforge_campaign.native_session.run` or Python exception
  callbacks.
- Removed Python decoder/readiness/descriptor tests for subprocess bridges are
  not counted as newly passed Rust tests. Public execution now uses typed native
  calls; retained diagnostic commands have their own native request validation.
- Unit and finite process fixtures establish specific contracts, not immunity to
  every filesystem race, every signal timing or arbitrary hostile Dart code.
  Live SDK tests and cross-platform CI remain necessary; SDK tests are not run by
  listing their commands or by passing `cargo test` alone.
- Full byte-for-byte legacy parity, line/branch coverage percentages and a new
  end-to-end performance/resource improvement are not claimed. Optional frozen
  comparisons and historical performance results have distinct provenance.
- Deferred issues FF-001 and FF-002 remain tracked in
  [known issues](known-issues.md). Rust tests run serially while FF-001 is open.

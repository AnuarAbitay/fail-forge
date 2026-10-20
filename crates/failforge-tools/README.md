# Native developer tools

`failforge-tools` contains the maintained regression and measurement workflows.
All orchestration is Rust. The Dart adapter and its tests remain Dart. None of
these commands invokes Python or installs project dependencies implicitly.

Build the native executables before a real SDK regression, then keep production
sources, binaries, prepared dependency metadata and SDK files unchanged until
that regression finishes:

```sh
cargo build --workspace --bins --examples --locked
cargo test -p failforge-tools --locked -- --test-threads=1
target/debug/failforge-tools semantic-e2e --output /absolute/new-semantic-report
target/debug/failforge-tools regression --suite all --output /absolute/new-regression-report \
  --dart-sdk /absolute/flutter/bin/cache/dart-sdk --flutter-sdk /absolute/flutter
```

`--dart-sdk` and `--installation` select explicit prepared installations.
For release or relocated binaries, add `--binary-dir /absolute/executables` to
`regression`; its default is the selected installation's `target/debug`. Without
those flags, the tools use the checkout and its `.toolchain/flutter/bin/cache/dart-sdk`.
Every output directory must be new. Receipts include the executed command,
actual exit code, wall time and errors; captures remain available on failure.
The harness compares immutable fixture inputs and final reports, including
progress, counts, completion, cleanup, family scores and retained identities.

Regression suites can run separately:

| Suite | Actual integration checks |
| --- | --- |
| `smoke` | Direct/YAML identity parity, no candidates, failed baseline, classic/failure/Dart families, parallel killed + survived results |
| `profiles` | All families, omitted/disabled parents and operators, global cap before tests, zero candidates, unknown IDs, required language, rejected legacy fields and non-boolean flags |
| `classification` | Initialization failure with original/mutant/original confirmations; ordinary failure witness with setup suppression; suppression without a witness stays unclassified |
| `timeouts` | An asynchronous inner test timeout is killed with complete reporter evidence; a busy-loop outer timeout remains timeout and is excluded from score |
| `cancellation` | SIGINT and SIGTERM only after a real mutant author test starts; cancelled records and cleanup retained |
| `recovery` | Active owner lease protection, outer-controller SIGKILL, native owner drain, deterministic released-lease recovery on the next YAML campaign, preserved captures |
| `applicability` | Valid versus statically invalid versus absent/off-scope operators; dry-run; no author execution; all-false rejection; prepared-analysis versus unprepared template |
| `flutter` | Classic killed/survived/static-invalid cases, synchronous and asynchronous entry failures, both MethodChannel errors |
| `replay` | Ordinary campaign seed, exact retained candidates without rediscovery, authenticated archived baseline, forced namespace isolation, two original controls before mutants and one after, rejected changed archives, cancellation |
| `all` | All suites in sequence; Flutter is included when `--flutter-sdk` is supplied |

The recovery suite deliberately overlaps an empty campaign with a live owner to
prove lease protection. Otherwise command execution is sequential; the parallel
mutation fixture uses the production scheduler's two workers.
After controller death it retains the lease descriptor across workspace renames,
waits for the surviving owner's final cleanup report, and verifies the private
registry is clear before removing its temporary root.

`semantic-e2e` checks the committed 19-operator catalogue against real discovery,
static validation, source-bound patch proofs and cleanup. It executes each
healthy original plus every discovered mutant through the production Runner.
An arbitrary VM error or source line containing the assertion marker does not
count as an observed mutation: only the fixture's actual uncaught assertion does.

Tool SIGINT and SIGTERM are forwarded to the active native owner, including
while waiting for READY or an author-test barrier. The tool exits with 130 or
143 after drain. Commands have an external deadline followed by a separate 30-second cooperative
SIGINT drain grace. If an owner still cannot be reaped, the command fails with
`cleanupUnconfirmed`, its PID and retained temporary inputs. The harness neither
reports success nor deletes potentially live workspaces in that situation.

## Benchmark

```sh
cargo build --release --workspace --bins --examples --locked
target/release/failforge-tools benchmark --output /absolute/new-measurement \
  --iterations 20 --warmup 3 --bytes 1048576 --deadline 10
```

The benchmark preserves the original three scenarios: exit, bytes on both
streams, and cancellation after the fixture's READY barrier. It compares the
production `microbench` example's reused Runner with a new CLI process per
sample. `raw.csv`, `summary.json`, `environment.json` and library output are
retained. Statistics use median and nearest-rank p95. Durations are nanoseconds;
fresh CLI completion is observed by polling at 200 microseconds, included in
the measurement. Captured byte counts, native cause/status and cleanup are
verified before accepting a sample. This command measures latency, not RSS or
CPU consumption.

## Pinned CI SDK

```sh
target/debug/failforge-tools install-test-flutter --destination /absolute/sdk-parent
```

This is the only command that downloads anything. It selects Flutter 3.47.7 for
the current supported macOS/Linux architecture from the official release
manifest, verifies the complete archive SHA-256, extracts into staging and
publishes without replacing any existing destination. The installer unit tests
are offline; they cover pin/architecture/channel selection, manifest ambiguity,
archive path/hash validation and a concurrently created destination.

## Replaced Python workflows and retained checks

The old Python implementations were independent diagnostic engines. The native
product now supplies their behavior directly; keeping a second implementation
would create two competing verdict, isolation and scheduling policies.

| Removed workflow or helper | Rust replacement and regression coverage |
| --- | --- |
| `benchmark.py`, `test_benchmark.py` | `benchmark` command; `microbench`; statistics, CSV sequence/byte, READY/deadline and receipt tests in this crate; native `test_outcome_tests` receipt corpus |
| `install_test_flutter.py`, installer tests | `install-test-flutter`; offline manifest, platform, hash/path and atomic nonreplacement tests |
| `dart_semantic_e2e.py`, its tests | `semantic-e2e`; exact 19-operator catalogue, strict terminal transcript, production `verify_candidate`, VM observation and capture checks |
| `mutate_cli_e2e.py`, `mutation_profile_e2e.py`, their tests | `regression smoke/profiles/classification/timeouts/cancellation/flutter`; public CLI process matrix in `failforge-cli/tests/mutate.rs`; native report/terminal/publication tests |
| `profile_applicability_e2e.py`, its tests | `regression applicability`; `failforge-profile` preparation/analysis/output and CLI profile-generation process tests |
| `workspace_recovery_e2e.py` | `regression recovery`; workspace lifecycle adversarial tests and native public-controller SIGKILL process test |
| `dart_adapter_e2e.py`, its tests | `regression flutter`; production Dart adapter/session/discovery tests; native report grouping and Flutter reporter corpus |
| `dart_initialization_e2e.py` | `regression classification`; native initialization signature/evidence/policy tests, including compiler/resource ambiguity and ordered original/mutant/original gates |
| `dart_timeout_e2e.py`, `dart_timeout_campaign_e2e.py`, timeout tests | `regression timeouts`; native reporter/outcome timeout-prefix and inner-timeout diagnostics tests; native campaign reports |
| `dart_test_evidence.py`, Flutter/evidence/progress tests | `failforge-dart` reporter/outcome/capture classification, including the retained frozen evidence corpus; public `classify_dart_capture` for trusted historical receipts |
| `dart_test_policy.py`, policy tests | `failforge-config` duration and normalized timeout policy; native attempt specifications and configuration tests |
| `dart_namespace.py`, namespace/adversarial tests | `failforge-dart::namespace`, its scanner/isolation adversarial tests and retained scanner corpus; production namespace planner tests |
| `mutant_initialization.py`, verdict tests | Native initialization evidence/policy/handoff plus test catalogue selection and ordinary-failure witness tests |
| `run_public_dart_campaign.py`, campaign helper tests | `failforge mutate` production lifecycle/discovery/family scheduler/fresh attempts; CLI native process matrix and real SDK suites |
| `run_isolated_dart_replay.py`, replay tests | Public `failforge replay`; retained transcript and baseline admission; production lifecycle, exact candidate queue, forced isolation and fresh controls; `regression replay` and native CLI process tests |

Full campaigns use `failforge mutate`. Archived classical candidates can be
executed without rediscovery through `failforge replay --project ... --framework
... --transcript ... --baseline ... --output ...`; this shares the production
campaign owner and reports. Diagnostic capture classification and retained
transcript/candidate verification are also public Rust APIs. The
frozen JSON evidence corpora remain test data, with no Python interpreter needed.
Current FF-001 and FF-002 status and evidence are tracked in
[known issues](../../docs/known-issues.md).

## Archived public projects

`public-regression` runs an explicitly selected manifest group through the fixed
`failforge` executable. There is no shell-command field or default subset:

```sh
target/release/failforge-tools public-regression --manifest /absolute/manifest.json \
  --group canonical --binary-dir /absolute/release --output /absolute/new-results
```

Add `--check-only` to validate retained paths and configuration without starting
an SDK. A version1 manifest lists named cases with a prepared project, optional
profile generation, the exact normalized direct configuration, historical
profile and per-ID verdict oracle, `direct`/`profile` variants, and a bounded
`budgetSeconds`. Source files, historical profiles and prior results are read
only. Generation and each variant use separate fresh production workspace copies.
Generated applicability is checked against the old profile; profile execution
uses its newly generated selection with the retained manual scope and timeouts.

Each variant compares complete discovery proofs, IDs, edits and outcomes,
aggregate/family consistency, scores, baseline/final controls and actual cleanup.
The original input hashes and all command captures remain in the new output.
Independent failures are retained while the remaining cases continue. Unconfirmed
process cleanup or a nonempty private workspace registry stops new commands.
An explicit `cancelAfterRecords` case is labeled partial and cannot establish a
complete mutation score. No archived scope is silently reduced to a smoke test.

# Mutation campaigns

`failforge mutate` runs one selected mutation family through the same campaign
implementation, configured either by an explicit YAML file or by CLI arguments.
The Rust CLI replaces itself with the Python controller. Each native test command
is owned by the existing Rust Runner; the controller does not implement another
test timeout or kill that cleanup owner.

## Prepare the installation and target

The first distribution uses this repository's installation layout, Python 3.9+
and PyYAML. Build both native executables and prepare the adapter separately:

```sh
cargo build --workspace --locked
python3 -m pip install -r python/requirements.txt
dart --suppress-analytics pub get --enforce-lockfile --directory adapters/dart
```

The target must already have its dependencies prepared and contain
`pubspec.yaml`, `.dart_tool/package_config.json` and
`.dart_tool/package_graph.json`. The campaign never runs `pub get`, changes a
target's dependencies or downloads its source. This version supports one
standalone Dart package, with `test` or `flutter_test`; Pub workspaces and source
symlinks are rejected explicitly.

The default SDK is `.toolchain/flutter/bin/cache/dart-sdk` in the installation.
Use `--dart-sdk /absolute/dart-sdk` for another Dart SDK, or
`--flutter-sdk /absolute/flutter` for Flutter. The SDK must match the adapter's
documented version constraints. A Flutter project can also use the installation's
Flutter SDK when its Dart SDK has that layout. Runtime paths and effective rules
are recorded in the manifest.

`--installation PATH` selects a relocated installation containing `python/`,
`scripts/` and `adapters/`. `--python PROGRAM` selects the interpreter, including
a virtual environment. `failforge` and `failforge-adapter-campaign` must be in the
same binary directory. A standalone copied `failforge` binary is insufficient.

## Explicit CLI configuration

```sh
failforge mutate --project ./my_package --family classic
failforge mutate --project ./my_package --family failure_mode
failforge mutate --project ./my_package --family language_specific --language dart
```

`--project` defaults to the invocation directory. `--family` is required without
`--config`. The CLI does not discover or read `failforge.yml` automatically.
`classic` enables the adapter's 31 default classical operators. `failure_mode`
enables `EXCEPTION_AT_ENTRY` for Dart, plus `METHOD_CHANNEL_ERROR` for Flutter.
An explicitly selected operator must belong to the chosen family. Method-channel
mutation requires a prepared Flutter project.

`language_specific` selects the Dart semantic catalogue, with 19 operators in
six categories. `--language dart` names the subtype explicitly. See the
[Dart semantic mutation guide](dart-semantic-mutations.md) for operator IDs,
category switches and YAML component selection. This family's report uses
`languageSpecificScore`, and the terminal summary includes `Language: dart`.

Optional filters and execution settings:

```sh
failforge mutate --project ./my_package --family classic \
  --operator BOOLEAN_LITERAL_FLIP --include 'lib/**.dart' \
  --method enabled --workers 2 \
  --test-timeout 30s --command-timeout 120s \
  --output /absolute/new-run-directory
```

`--operator`, `--include`, `--exclude`, `--class` and `--method` are repeatable.
The adapter combines different filter categories with AND, and entries inside a
category with OR. The visible default scope for both families is `lib/**.dart`;
normal generated/test/dependency exclusions still apply. Explicit empty lists in
YAML preserve empty selection. The legacy `CONDITIONAL_NEGATION` operator remains
available through explicit selection.

## Explicit YAML configuration

```sh
failforge mutate --config failforge.yml
```

Prepared examples are [failforge.classic.yml](../examples/failforge.classic.yml)
and [failforge.failure-mode.yml](../examples/failforge.failure-mode.yml). Both
target the repository's Flutter fixture; prepare that example's dependencies
before using them.

```yaml
version: 1
project: ./my_package
family: classic
workers: 1
timeouts:
  test: 30s
  command: 120s
rules:
  include: ['lib/**.dart']
  exclude: ['lib/generated/**']
  operators: [BOOLEAN_LITERAL_FLIP, RELATIONAL_BOUNDARY]
limits:
  max_mutants: 1000
output: ./reports/run-001
```

YAML paths are relative to the file's directory; CLI paths are relative to the
invocation directory. Omitting `project` in YAML selects the file's directory.
YAML also accepts `dart_sdk`, `flutter_sdk`, `adapter_root`
and `rules.classes`/`rules.methods`. `dart_sdk` and `flutter_sdk` are mutually
exclusive input choices. Unknown keys, duplicate keys, YAML tags/anchors/aliases,
multiple documents and files larger than 1 MiB are rejected. YAML settings cannot
be mixed with semantic CLI settings, including `--output`; interpreter and
installation options remain available in either mode.

Defaults are one worker, `test: 30s`, `command: 120s` and 1,000 mutants. Workers
range from 1 to 4; `max_mutants` ranges from 0 to 10,000. The adapter refuses a
truncated discovery instead of publishing a score for a hidden partial set.
Both durations use positive integer `ms`, `s` or `m` units. The command budget
must exceed the test default by at least five seconds. It bounds the entire test
process; metadata, multiple timeouts and teardown can still reach the outer limit.

## Results and correctness

The controller snapshots the prepared project, runs fresh discovery and static
validation, establishes a passing baseline, executes each valid mutant in a
fresh temporary copy and checks an original control afterward. Framework-name
collisions such as `pool` use the unchanged snapshot for the test framework and a
separate subject alias. Unsupported alias rewrites fail explicitly. Ordinary
packages retain their self package imports. Originals, snapshots, dependencies,
SDK and adapter inputs are checked for changes.

A failing `setUpAll` can prevent some tests and child groups from starting, so
their names never appear in the JSON reporter. When the baseline and observed
group metadata explain this suppression, a separately observed ordinary test
can still establish `KILLED`: its suite, name, group ancestry, source location and
metadata must uniquely match a passing baseline test, with a verified author
failure. The result retains the actual observed catalogue, missing-entry counts
and `catalogueIdentityProven: false`. It never invents unstarted test results.
Without that witness, the controller records `UNCLASSIFIED` with
`author_test_catalogue_incomplete_after_setup_failure` and excludes it from the
score. `SURVIVED` requires the complete expected catalogue. Unexplained selection
changes and execution-input changes still fail the guard.

An author null assertion during registration can be caused by a subject method
that has already returned, so its frame need not appear in the exception stack.
The production controller can use a narrowly checked author-runtime signature
to initiate fresh original/mutant/original controls. The signature alone never
establishes a kill. Both originals must pass the full expected catalogue; the
mutant replay must reproduce the initial error and stack after replacing only
the generated workspace path. Both raw streams, native cleanup, suite selection
and source/dependency/SDK bindings remain mandatory.

### Framework namespace isolation

When the selected package is also a test-framework dependency, author tests use
the subject alias while the framework keeps its unchanged library. This applies
to every mutation family. The header rewriter recognizes URI token ranges with
comments, metadata such as `@TestOn('vm')`, named or unnamed `library` directives,
import prefixes (`as`), `deferred`, `show`/`hide`, conditional import/export URI
branches and URI-bearing `part`/`part of` directives. It preserves those constructs
and replaces only a canonical `package:<selected-package>/` prefix inside a
supported URI literal. Comments and other header text are not reformatted.

Headers alone need no analyzer subprocess. If author test code contains the
identifier `resolvePackageUri` or `resolvePackageUriSync`, the controller obtains
one read-only namespace plan for that frozen project through the Rust Runner.
The Dart tool resolves SDK bindings without executing author code, running
`pub get` or changing dependency metadata. It accepts a direct call to the actual `dart:isolate.Isolate`
resolver whose argument is an actual `dart:core.Uri.parse` call with one canonical
simple string literal. Only an owning-package prefix is changed; canonical URI
literals for other packages are left alone. Prefixed SDK imports are supported,
and a same-named custom class or method cannot authorize an edit.

The internal plan binds the original package configuration, complete author-test
file inventory and each file's SHA-256. Each attempt rechecks these bindings and
the exact expected bytes before applying non-overlapping prefix edits to its
copy. Reversing those precise edits must reproduce every original byte and hash;
bytes outside the namespace ranges remain unchanged. Runtime URI edits are
recorded separately from header edits, with the planner receipt and raw captures
under `namespace-plan/`. The plan is produced internally, not accepted as a user
configuration or persisted as a source-hash-only cache.

SDK resolver tear-offs, indirect/computed/interpolated URI arguments, escaped or
encoded URI spellings and unrecognized owning-package references are rejected
explicitly. The implementation does not rewrite arbitrary assertion/body
strings. Self-package URI references in the subject's `lib/` remain unsupported
when framework isolation is required: rewriting that library would change the
mutation's bound source and offsets. A namespace preparation failure stops the
campaign before its test process launches; it is not a killed mutant or a successful
baseline, and a failed campaign has no score. Packages without a framework-name
collision do not require these alias rewrites.

By default a run creates `.failforge/runs/<run-id>/` in the selected project:

```text
manifest.json          effective configuration and original input hashes
input-bindings.json    dependency, adapter, SDK and executable bindings
events.jsonl           ordered stage/progress events and process intervals
summary.json           status, outcomes, stage timings and family-specific score
discovery/             fresh adapter transcript and static-validation evidence
baseline/              original test results and exact author-test catalogue
original-after/        final original control
mutants/<id>/          patch metadata, result and raw captures
```

An explicit output directory must not already exist. Output inside the target
project must be below `.failforge/`, which is excluded from copies and source
bindings. Temporary copies are removed after each attempt. The CLI prints the
report path. Captures retain the Runner's private file permissions.

The CLI prints a final ASCII table to stdout, followed by the report path.
Progress messages go to stderr. For example:

```text
FailForge mutation summary
Family: classic
Status: completed
Mutants: 2/2

+--------------+-------+----------+
| Outcome      | Count | In score |
+--------------+-------+----------+
| KILLED       |     1 | yes      |
| SURVIVED     |     1 | yes      |
| INVALID      |     0 | no       |
| TIMEOUT      |     0 | no       |
| INFRA_ERROR  |     0 | no       |
| UNCLASSIFIED |     0 | no       |
| NO_TESTS     |     0 | no       |
| CANCELLED    |     0 | no       |
+--------------+-------+----------+

Mutation score: 50.00% (1/2)
Wall time: 19.43s
Report: /path/to/run/summary.json
```

`Mutants` shows processed/discovered counts; `?` means discovery did not establish
the expected total. All eight outcomes appear, including zero counts. `In score`
marks outcome eligibility. The score line is `Failure mode score` for that family.
An incomplete campaign displays `N/A (campaign incomplete)`; a complete campaign
with no killed or surviving mutants displays `N/A (no scored mutants)`. Wall time
is elapsed campaign time. The final `Report:` line points to the JSON evidence.

Only `KILLED` and `SURVIVED` enter the score denominator. `TIMEOUT`, `INVALID` and
inconclusive results are excluded explicitly. Completed inner test timeouts retain
`KILLED / test_timeout`; externally interrupted suites retain `TIMEOUT` even if
earlier inner errors were observed. Runtime load failures require fresh causal
A/B/A confirmation; a nonzero exit alone cannot kill a mutant. Classical results
use `mutationScore`; failure-mode results use `failureModeScore`. They are never
combined. Failed or cancelled campaigns have no final score.

Exit codes: `0` for a completed campaign (including survivors or bounded mutant
timeouts), `2` for invalid configuration, `125` for failed or inconclusive
execution, `130` for Ctrl+C and `143` for SIGTERM. Cancellation requests the active
Rust CLI owners to stop and waits for their cleanup; it does not kill them. The
adapter helper drains its own Runner after a closed consumer pipe. The retained
report is marked incomplete, with no final score.

## Recovery of abandoned working copies

Before allocating working copies, each valid `mutate` campaign scans FailForge's
private per-user registry, `<system-temporary-directory>/failforge-workspaces-<uid>`.
Snapshots and attempts are created inside a registered campaign directory. The
registry must be outside the target project, dependencies, SDK and controller
inputs; a `TMPDIR` inside those trees is rejected before creating the registry.
The controller and its native Rust cleanup owners hold an operating-system file lock
on that campaign. A new campaign removes a registered directory only after it can
acquire the lock, so killing the controller does not make a live Runner's working
copies eligible for deletion. This also permits recovery after a reboot once all
owners are gone, if the operating system has retained the temporary directory.

Recovery validates directory ownership, private permissions, protocol metadata
and lock files. Deletion stays relative to opened directory descriptors and never
follows payload symlinks. Active and unrecognized entries are retained; unsafe
registry roots are rejected. Recovery does not search arbitrary temporary folders
by prefix or age, and does not delete working copies from older FailForge versions
that have no registry metadata. Failed directory removal is not reported as a
successful recovery. Payload trees deeper than 128 levels are retained with the
explicit reason `workspace_depth_limit`; they cannot prevent other campaigns
from starting. Control files are opened without blocking on special files.

The event journal records recovery and the newly allocated workspace directory;
the final summary includes `workspaceRecovery`, `workspaceDirectory` and
`workspaceCleanupDeferred`. Reports and captures remain in the configured output
directory and are not deleted by recovery. Restarting starts a fresh campaign;
recovery does not reuse mutation results or resume a previous campaign.

The lease protects the controller and its native cleanup owners. It is not a
system sandbox or a replacement for Runner process supervision. The private
lease descriptor is closed on exec into the tested program. Force-killing a Rust
cleanup owner itself bypasses that owner's cleanup protocol.

## Verify

```sh
PYTHONPATH=python:scripts python3 -B -m unittest discover -s python/tests -v
python3 -B -m unittest discover -s scripts -p 'test_*.py'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
# Requires the prepared tooling and Flutter example described above:
python3 scripts/mutate_cli_e2e.py --output /absolute/new-evidence-directory --flutter-sdk /absolute/flutter
python3 scripts/workspace_recovery_e2e.py --output /absolute/new-crash-evidence-directory
```

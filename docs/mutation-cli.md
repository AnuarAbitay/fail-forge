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
```

`--project` defaults to the invocation directory. `--family` is required without
`--config`. The CLI does not discover or read `failforge.yml` automatically.
`classic` enables the adapter's 31 default classical operators. `failure_mode`
enables `EXCEPTION_AT_ENTRY` for Dart, plus `METHOD_CHANNEL_ERROR` for Flutter.
An explicitly selected operator must belong to the chosen family. Method-channel
mutation requires a prepared Flutter project.

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

## Verify

```sh
PYTHONPATH=python:scripts python3 -B -m unittest discover -s python/tests -v
python3 -B -m unittest discover -s scripts -p 'test_*.py'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
# Requires the prepared tooling and Flutter example described above:
python3 scripts/mutate_cli_e2e.py --output /absolute/new-evidence-directory --flutter-sdk /absolute/flutter
```

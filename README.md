# FailForge

**Find the bugs your Dart and Flutter tests would miss.**

FailForge makes small changes to your code and runs your tests against them.
When a test catches a change, the mutant is **killed**. When the tests still
pass, the mutant **survives** and gives you a concrete place to review your
assertions, edge cases or requirements.

FailForge is preparing for its first public beta. Installation currently uses
a source checkout. Read the [installation guide](docs/installation.md) and
[beta limitations](docs/beta-limitations.md) before your first run.

[Get started](#get-started) ·
[Mutation operators](docs/mutation-operators.md) ·
[Profiles](#yaml-profiles) ·
[Results](#understand-the-results) ·
[Changelog](CHANGELOG.md)

## Why mutation testing?

A passing test suite tells you that the current code satisfies its assertions.
Mutation testing checks whether those assertions can detect changes in behavior.
For example:

```dart
// Original
bool canWithdraw(int balance, int amount) => balance >= amount;

// Mutant: change the comparison boundary
bool canWithdraw(int balance, int amount) => balance > amount;
```

A test where `balance == amount` can distinguish these versions. If all your
tests use a larger balance, this mutant may survive despite the line being
covered.

A survivor needs investigation: it can expose a missing test, but it can also
be equivalent to the original for your application's valid inputs.

## What you can do

- Test standalone Dart packages and Flutter packages with existing test suites.
- Choose classical mutations, failure injection or Dart-specific mutations.
- Generate a YAML profile for one source file, enabling operators that have
  statically valid candidates in that file.
- Run from a saved profile or directly from command-line options.
- Narrow mutations by file patterns, class names, method names and operator IDs.
- Run with one to four workers and configure per-test and whole-command timeouts.
- See live progress and a final terminal table, with JSON reports and captured
  test output for investigation.
- Stop a run with Ctrl+C while retaining completed results. Abandoned working
  copies can be cleaned up on a later run.

Mutations execute in separate working copies. Keep the target project unchanged
while a run is active. These copies are **not a security sandbox**: tests execute
with your user account's permissions.

## Mutation families

| Family | Operators | What it checks |
| --- | ---: | --- |
| `classic` | 31 in the default catalogue | Comparisons, boolean logic, arithmetic, constants, returns, control flow, loops and other common behavior changes. |
| `failure_mode` | 2 | Error handling when execution throws or a Flutter method-channel operation fails. |
| `language_specific` + `dart` | 19 | Null safety, parameter defaults, switches, patterns, collection elements and enum values. |

The compatibility operator `CONDITIONAL_NEGATION` is also available by explicit
selection; it is outside the 31 default classical operators.
`METHOD_CHANNEL_ERROR` requires Flutter. Applicable mutation sites depend on the
source code and supported constructs, so enabling an operator does not guarantee
that it will produce a mutant.

The [complete operator catalogue](docs/mutation-operators.md) lists every ID,
transformation and family. Scores are reported separately for each family.

## Get started

The current beta targets **Linux and macOS**. Windows is unsupported.
Mutation analysis requires a compatible **Dart SDK `>=3.13.0 <3.14.0`**,
or Flutter with a Dart SDK in that range. Building from source requires
**Rust 1.85.1 or newer** and a C linker.

Follow the [installation guide](docs/installation.md) to build FailForge,
prepare its adapter and prepare your target package's dependencies. It also
explains Flutter setup and the platforms verified so far.
After building, you can [move FailForge into a separate installation](docs/installation.md#relocate-an-installation)
without keeping the original checkout. A compatible SDK and prepared adapter
dependencies are still required.

The examples below run from the **target package root**, with these paths set:

```sh
export FAILFORGE_HOME=/absolute/path/to/fail-forge
export DART_SDK=/absolute/path/to/dart-sdk
export PATH="$FAILFORGE_HOME/target/debug:$PATH"
```

Replace `lib/src/calculator.dart` with an existing source file in your package.
Begin with one operator and one file:

```sh
failforge mutate \
  --project . \
  --family classic \
  --operator RELATIONAL_BOUNDARY \
  --include 'lib/src/calculator.dart' \
  --workers 1 \
  --test-timeout 30s \
  --command-timeout 120s \
  --output .failforge/first-run \
  --installation "$FAILFORGE_HOME" \
  --dart-sdk "$DART_SDK"
```

The output directory must be new; choose another name for each run. Output
inside the target package belongs under `.failforge/`. Omit `--operator` to
select the default classical catalogue.

For a Flutter package, replace `--dart-sdk "$DART_SDK"` with
`--flutter-sdk /absolute/path/to/flutter`.

### Generate a profile

Save your choices in a profile next to your project:

```sh
failforge profile generate \
  --file lib/src/calculator.dart \
  --family classic \
  --output profiles/calculator.yml \
  --installation "$FAILFORGE_HOME" \
  --dart-sdk "$DART_SDK"

failforge mutate \
  --config profiles/calculator.yml \
  --installation "$FAILFORGE_HOME"
```

Generation sets an operator to `true` only if it has at least one statically
valid candidate in the selected file. It does not run your tests or establish
that the mutation changes observable behavior. Review the generated profile
before starting a large campaign.

Repeat `--family` to select several families; add `--language dart` when
selecting `language_specific`. Use `--dry-run` to preview the profile,
`--force` to replace an existing file, or `--template` to generate a catalogue
template without source analysis. Template generation does not accept SDK flags.

The generated profile records the selected SDK path. When using `--config`,
change execution settings in YAML rather than adding `--dart-sdk`, `--workers`
or other campaign options to the command.

### Run other families directly

```sh
failforge mutate \
  --project . \
  --family failure_mode \
  --operator EXCEPTION_AT_ENTRY \
  --include 'lib/src/calculator.dart' \
  --output .failforge/failure-run \
  --installation "$FAILFORGE_HOME" \
  --dart-sdk "$DART_SDK"

failforge mutate \
  --project . \
  --family language_specific \
  --language dart \
  --include 'lib/src/calculator.dart' \
  --output .failforge/dart-run \
  --installation "$FAILFORGE_HOME" \
  --dart-sdk "$DART_SDK"
```

Direct CLI mode selects one family per run. A YAML profile can select several.

## YAML profiles

One YAML file represents one profile. Put it inside the target package; the
nearest parent `pubspec.yaml` identifies that package. There are no `version`
or `project` fields.

For example, `profiles/calculator.yml`:

```yaml
dart_sdk: /absolute/path/to/dart-sdk
workers: 2
timeouts:
  test: 30s
  command: 120s
rules:
  include: ['lib/src/calculator.dart']
mutations:
  classic:
    enabled: true
    operators:
      RELATIONAL_BOUNDARY: true
      BOOLEAN_LITERAL_FLIP: false
  failure_mode:
    enabled: false
    operators:
      EXCEPTION_AT_ENTRY: true
  language_specific:
    enabled: true
    language: dart
    operators:
      NSM-004: true
limits:
  max_mutants: 1000
```

- A family and its operator must both be explicitly `true` to run.
- Omitted families, omitted operators and `false` switches are disabled.
- A `language_specific` section requires `language: dart`, even when disabled.
- Unknown keys, operator IDs and invalid values produce configuration errors.
- At least one operator must be enabled. If analysis finds no applicable
  mutations, the profile reports `no_mutants` and has no score.
- File filters are relative to the package root. SDK and output paths in YAML
  are relative to the profile file unless absolute.
- Class and method filters select mutation sites; they do not select individual
  tests to run.

Use `flutter_sdk` instead of `dart_sdk` for Flutter. The
[profile reference](docs/mutation-cli.md#explicit-yaml-configuration) covers
all settings, defaults and path rules.

## Understand the results

An illustrative completed run:

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

| Outcome | Meaning |
| --- | --- |
| `KILLED` | The tests detected the mutation. |
| `SURVIVED` | The tests passed with the mutation applied. |
| `INVALID` | The mutation was rejected as invalid. |
| `TIMEOUT` | The complete test command exceeded its execution limit. |
| `INFRA_ERROR` | An execution or infrastructure problem prevented a reliable verdict. |
| `UNCLASSIFIED` | The available evidence did not establish a verdict. |
| `NO_TESTS` | No usable test execution was established. |
| `CANCELLED` | Execution was interrupted by cancellation. |

For a completed campaign:

```text
score = KILLED / (KILLED + SURVIVED) × 100%
```

Other outcomes are excluded. Incomplete campaigns and campaigns with no scored
mutants show `N/A`. A test framework timeout that completes as a confirmed test
failure can be `KILLED`; an externally timed-out command remains `TIMEOUT`.
See [result interpretation](docs/mutation-cli.md#results-and-correctness)
for details.

The terminal prints the report location. Use `summary.json` for final results,
`progress.json` for live status and the retained mutant results and test captures
to investigate failures or survivors. Reports and captures can contain sensitive
project data; review them before sharing.

**Surviving mutants alone do not cause a nonzero exit code.** Execution and
inconclusive-result errors can still return `125`, even when all candidates were
processed. The beta has no built-in minimum-score gate; CI integrations must
inspect the report to apply their own quality threshold.

## Commands

| Command | Purpose |
| --- | --- |
| `failforge profile generate` | Generate a profile using applicability analysis or a catalogue template. |
| `failforge mutate` | Run a mutation campaign from YAML or command-line options. |
| `failforge replay` | Re-run retained classical candidates with matching source and baseline evidence; see the [replay requirements](docs/mutation-cli.md#replay-a-retained-classic-transcript). |
| `failforge run` | Run an external command with timeout, output capture and cancellation; see the [command guide](docs/cli.md). |

Use `failforge --help` or `<command> --help` for available options.

## Documentation and feedback

- [Installation and first run](docs/installation.md)
- [Complete mutation operator catalogue](docs/mutation-operators.md)
- [Mutation commands and profile reference](docs/mutation-cli.md)
- [Beta limitations and verification scope](docs/beta-limitations.md)
- [Changelog](CHANGELOG.md)
- [Security policy and vulnerability reporting](SECURITY.md)

Report ordinary bugs and feature requests in
[GitHub Issues](https://github.com/AnuarAbitay/fail-forge/issues).
Include your FailForge revision/version, OS, SDK versions, a minimal reproduction
and sanitized diagnostics. Use the security policy for suspected vulnerabilities.

## License

Copyright 2026 AnuarAbitay.

FailForge's original source code, including its Rust crates and Dart adapter,
is licensed under the [Apache License, Version 2.0](LICENSE).
See [NOTICE](NOTICE) for attribution. Third-party dependencies retain their
respective licenses; see [Third-party notices](THIRD_PARTY_NOTICES.md) and the
[dependency license review](docs/dependency-licenses.md).

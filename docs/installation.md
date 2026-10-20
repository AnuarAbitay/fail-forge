# Installation and first run

This guide installs FailForge from source. It does not require a published
release archive. Keep the source checkout available while using this installation.
Read the [beta limitations](beta-limitations.md) before running a campaign.

## Requirements

| Requirement | Current support |
| --- | --- |
| Operating system | Linux or macOS. Windows is unsupported. Platform support does not imply that every distribution or architecture has been tested. |
| Rust | Rust and Cargo 1.85.1 or newer, with the platform's C linker. |
| Dart adapter SDK | Dart `>=3.13.0 <3.14.0`. A Flutter installation may supply this Dart SDK. |
| Target package | One standalone Dart or Flutter package with a passing `test` or `flutter_test` suite and prepared dependencies. |
| Disk space | Space for the original package, temporary working copies and retained reports. Larger packages and more workers require more space. |

The adapter pins `analyzer` to `14.5.0`. Use the checked-in
`adapters/dart/pubspec.lock`; do not upgrade adapter dependencies as part of
installation. Python is not required.

Use projects and tests you trust. FailForge runs their code with your account's
permissions; working-copy isolation is not a security sandbox.

## 1. Build FailForge

Obtain a checkout or source download from the
[FailForge repository](https://github.com/AnuarAbitay/fail-forge).
The following examples use a POSIX shell. Replace the absolute paths with your
own paths:

```sh
export FAILFORGE_HOME="/absolute/path/to/fail-forge"
cd "$FAILFORGE_HOME"

rustc --version
cargo --version
cargo build --workspace --locked
```

The first build may download Rust dependencies. The default build is optimized;
you do not need to add `--release`. Cargo places the executables in
`target/debug/` with this project's current build settings.

Check the public command:

```sh
"$FAILFORGE_HOME/target/debug/failforge" --help
"$FAILFORGE_HOME/target/debug/failforge" mutate --help
"$FAILFORGE_HOME/target/debug/failforge" profile generate --help
```

Keep `failforge` and `failforge-adapter-campaign` together in `target/debug/`.
Profile analysis needs the second executable. Copying only `failforge` to another
directory is insufficient.

For convenience, add the build directory to the current shell's search path:

```sh
export PATH="$FAILFORGE_HOME/target/debug:$PATH"
```

Repeat these exports in a new shell or add them to your shell configuration.
The commands below pass `--installation "$FAILFORGE_HOME"` explicitly so that
the installation is selected consistently.

## 2. Prepare the adapter

For a standalone Dart SDK, set its root directory, not its `bin/dart` executable:

```sh
export DART_SDK="/absolute/path/to/dart-sdk"
"$DART_SDK/bin/dart" --version
"$DART_SDK/bin/dart" --suppress-analytics pub get \
  --enforce-lockfile --directory "$FAILFORGE_HOME/adapters/dart"
```

The version must satisfy `>=3.13.0 <3.14.0`. Preparing the adapter may download
its dependencies.

For Flutter, use the Dart SDK bundled with your chosen Flutter installation:

```sh
export FLUTTER_SDK="/absolute/path/to/flutter"
export DART_SDK="$FLUTTER_SDK/bin/cache/dart-sdk"
"$DART_SDK/bin/dart" --version
"$DART_SDK/bin/dart" --suppress-analytics pub get \
  --enforce-lockfile --directory "$FAILFORGE_HOME/adapters/dart"
```

The same Dart version restriction applies. Run Flutter's normal setup first if
its bundled Dart SDK is not yet available. Do not supply both `--dart-sdk` and
`--flutter-sdk` to a FailForge command.

## 3. Prepare the target package

Set the root of the package whose tests you want to assess:

```sh
export FAILFORGE_PROJECT="/absolute/path/to/your_package"
cd "$FAILFORGE_PROJECT"
```

For a Dart package:

```sh
"$DART_SDK/bin/dart" pub get
"$DART_SDK/bin/dart" test
```

For a Flutter package, use these commands instead:

```sh
"$FLUTTER_SDK/bin/flutter" pub get
"$FLUTTER_SDK/bin/flutter" test
```

Resolve dependency or test failures before starting mutation testing. The package
must contain `pubspec.yaml`, `.dart_tool/package_config.json` and
`.dart_tool/package_graph.json`. FailForge does not run `pub get` for the target.

Use a standalone package, not a Pub workspace. Source symlinks are unsupported.
Keep the target source, prepared dependencies and SDK unchanged during a run.

## 4. Run a small campaign without YAML

Start with one source file, one method and one operator. The example below
assumes your package has `lib/src/calculator.dart` containing a method named
`isEligible` with a comparison such as `age >= 18`. Replace the file and method
with existing names from your package:

```sh
failforge mutate \
  --installation "$FAILFORGE_HOME" \
  --dart-sdk "$DART_SDK" \
  --project "$FAILFORGE_PROJECT" \
  --family classic \
  --operator RELATIONAL_BOUNDARY \
  --include 'lib/src/calculator.dart' \
  --method isEligible \
  --workers 1 \
  --test-timeout 30s \
  --command-timeout 120s \
  --max-mutants 100 \
  --output "$FAILFORGE_PROJECT/.failforge/first-classic-run"
```

For Flutter, replace `--dart-sdk "$DART_SDK"` with
`--flutter-sdk "$FLUTTER_SDK"`.

The file and method filters select mutations. They do not limit which author
tests execute. If the selected code has no applicable mutations, expand or
change the selection. The mutant limit is a safety cap: exceeding it fails the
campaign rather than selecting an arbitrary subset.

The output directory must not already exist. Choose a new name for another run.
Output inside the target must be under `.failforge/`; alternatively, select a
new directory outside the target. Do not delete an earlier report merely to
reuse its name.

## 5. Generate and run a YAML profile

From the prepared target package, generate a profile for one existing library
source file:

```sh
cd "$FAILFORGE_PROJECT"
failforge profile generate \
  --installation "$FAILFORGE_HOME" \
  --dart-sdk "$DART_SDK" \
  --file lib/src/calculator.dart \
  --family classic \
  --output profiles/calculator.yml
```

For Flutter, again replace the SDK option with
`--flutter-sdk "$FLUTTER_SDK"`.

Generation analyzes applicability and writes explicit operator switches. It
does not run the target's tests. If no selected operator has a statically valid
candidate, it reports that no profile was written.

Open the generated YAML before running it. To keep the same method scope as the
previous example, add `methods` to its existing `rules` mapping:

```yaml
rules:
  include: ['lib/src/calculator.dart']
  methods: [isEligible]
```

Keep the other generated settings. The generator determines applicability for
the whole file; after narrowing to a method, some enabled operators may have no
candidates in that method.

The profile must be inside the target package. One file is one profile; it has
no `version` or `project` field. The generator records an explicit SDK override
as an absolute path in the YAML.

Run it with:

```sh
failforge mutate \
  --installation "$FAILFORGE_HOME" \
  --config profiles/calculator.yml
```

Do not add `--dart-sdk`, `--flutter-sdk`, `--output` or mutation-selection flags
to this command: those settings belong in the profile when using `--config`.
With no YAML `output` setting, FailForge chooses a new report directory under
the target's `.failforge/` directory. An explicit YAML output path is relative
to the profile file, and it must identify a new directory.

To move the profile to another machine, update its absolute SDK paths. See
[mutation campaigns](mutation-cli.md) for multiple families, all configuration
rules and the complete command reference.

## 6. Read the result

FailForge prints progress, a final outcome table, the score when available and
the path to `summary.json`. Keep that directory for detailed mutant results and
test output.

`KILLED` means the tests detected a mutant; `SURVIVED` means they did not.
Only those two outcomes enter the score denominator. Inspect `INVALID`,
`TIMEOUT` and inconclusive outcomes separately. Surviving mutants alone do not
cause a nonzero exit code; execution and inconclusive-result errors can still
return `125`. The command is not a quality-threshold gate.

Use Ctrl+C to request cancellation and allow cleanup to finish. An interrupted
campaign has no final score and does not automatically resume on the next run.
See the [report guide](mutation-cli.md#results-and-correctness) for details.

## Installation troubleshooting

| Symptom | Check |
| --- | --- |
| Adapter dependency resolution fails | Confirm the SDK is Dart 3.13.x and use the checked-in adapter lockfile. |
| Installation or adapter cannot be found | Pass `--installation` pointing to the complete source checkout and prepare `adapters/dart`. |
| Native analysis helper cannot be found | Build the workspace and keep `failforge-adapter-campaign` beside `failforge`. |
| Package metadata is missing | Run the appropriate `pub get` in the target with the selected SDK. |
| Baseline tests fail or time out | Run the target's normal test command first, then review the baseline report and timeout settings. |
| Output already exists | Choose a new report directory. Generated profile files also reject replacement unless `--force` is explicit. |
| Profile flags conflict | Put campaign and SDK settings in YAML; keep `--installation` on the `mutate --config` command. |

For unsupported project layouts and current release constraints, see
[beta limitations](beta-limitations.md).

# Installation and first run

This guide covers FailForge `0.1.0-beta.3`. Obtain a native archive and matching
verification files from the
[beta.3 release](https://github.com/AnuarAbitay/fail-forge/releases/tag/v0.1.0-beta.3),
or build the tagged source. Choose the operating system and architecture listed
for the archive. Developers can also
produce a local archive with the [packaging command](packaging.md).
Read the [beta limitations](beta-limitations.md) before running a campaign.
The broader Dart SDK range below is included in beta.3; older releases retain
their original SDK constraints.

## Requirements

| Requirement | Current support |
| --- | --- |
| Operating system | Linux or macOS. Windows is unsupported. Platform support does not imply that every distribution or architecture has been tested. |
| Rust for source builds | Rust and Cargo 1.85.1 or newer, with the platform's C linker. Not required when using a prepared native archive. |
| Dart adapter SDK | Stable Dart `>=3.13.0 <4.0.0`. A Flutter installation may supply this Dart SDK. |
| Target package | One standalone Dart or Flutter package with a passing `test` or `flutter_test` suite and prepared dependencies. |
| Disk space | Space for the original package, temporary working copies and retained reports. Larger packages and more workers require more space. |

The adapter pins `analyzer` to `14.5.0`. Use the checked-in
`adapters/dart/pubspec.lock`; do not upgrade adapter dependencies as part of
installation. Python is not required.

Local checks used Flutter 3.47.7 with Dart 3.13.5 and standalone Dart 3.13.0.
The SDK compatibility baseline `e8b850d` passed all
[10 CI jobs](https://github.com/AnuarAbitay/fail-forge/actions/runs/39950688183),
including pinned Flutter, latest stable Flutter and minimum Dart 3.13.0. This
run predates beta.3 release preparation. Review the release's verification for
the tagged revision. The accepted SDK range does not establish compatibility
with every newer SDK or language feature; those need separate validation with
the pinned analyzer.

Use projects and tests you trust. FailForge runs their code with your account's
permissions; working-copy isolation is not a security sandbox.

## 1. Choose an installation

### Install from an archive

Choose one target and obtain its three matching files from the
[release](https://github.com/AnuarAbitay/fail-forge/releases/tag/v0.1.0-beta.3).

| Platform | Target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| macOS arm64 | `aarch64-apple-darwin` |

- `failforge-0.1.0-beta.3-<target>.tar.gz`
- `package-manifest-<target>.json`
- `SHA256SUMS-<target>`

Check that the release's version and target match your installation. GitHub's
automatically generated source archives do not contain the native executables;
use the named `.tar.gz` asset or follow [Build from source](#build-from-source).

Verify the checksum in the directory containing those files, then extract into
a new parent directory:

```sh
cd /absolute/path/to/archive-directory
export FAILFORGE_TARGET=aarch64-apple-darwin
shasum -a 256 -c "SHA256SUMS-$FAILFORGE_TARGET"
```

Continue only when every checksum reports `OK`:

```sh
mkdir /absolute/path/to/new-install-parent
tar -xzf "failforge-0.1.0-beta.3-$FAILFORGE_TARGET.tar.gz" \
  -C /absolute/path/to/new-install-parent
export FAILFORGE_HOME="/absolute/path/to/new-install-parent/failforge-0.1.0-beta.3-$FAILFORGE_TARGET"
export PATH="$FAILFORGE_HOME/bin:$PATH"
failforge --version
failforge --help
```

On Linux, set `FAILFORGE_TARGET=x86_64-unknown-linux-gnu` and use
`sha256sum -c "SHA256SUMS-$FAILFORGE_TARGET"` instead of `shasum` if preferred.
Each target's checksum file verifies its archive and manifest, so you do not
need the other platform's files. Obtain all three from the same trusted source;
a checksum is not a digital signature.

The extraction example uses the macOS arm64 target; choose the Linux target
before the checksum and extraction steps when appropriate. macOS Intel has not
been verified for this release.

The archive contains both native executables, the adapter sources and required
project documentation/notices. Keep that installation together. The SDK and
prepared Pub dependencies are separate: continue with
[Prepare the adapter](#2-prepare-the-adapter). Rust and Cargo are not needed for
this installation path.

### Build from source

Obtain the versioned source from the
[FailForge repository](https://github.com/AnuarAbitay/fail-forge):

```sh
git clone --branch v0.1.0-beta.3 --depth 1 \
  https://github.com/AnuarAbitay/fail-forge.git
```

The following examples use a POSIX shell. Set `FAILFORGE_HOME` to the absolute
path of that checkout:

```sh
export FAILFORGE_HOME="/absolute/path/to/fail-forge"
cd "$FAILFORGE_HOME"

rustc --version
cargo --version
cargo build --workspace --locked
```

The first build may download Rust dependencies. The default build is optimized;
you do not need to add `--release`. With Cargo's default output directory, this
project places the executables in `target/debug/`.

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
the installation is selected consistently. Standard `target/debug/` and
`target/release/` builds can also locate the checkout without this flag.

If you use `CARGO_TARGET_DIR`, `cargo --target` or another binary layout, adjust
the executable paths and `PATH` above, and keep the explicit `--installation`
option. FailForge does not fall back to the checkout path used at build time.

## 2. Prepare the adapter

For a standalone Dart SDK, set its root directory, not its `bin/dart` executable:

```sh
export DART_SDK="/absolute/path/to/dart-sdk"
"$DART_SDK/bin/dart" --version
"$DART_SDK/bin/dart" --suppress-analytics pub get \
  --enforce-lockfile --directory "$FAILFORGE_HOME/adapters/dart"
```

The stable version must satisfy `>=3.13.0 <4.0.0`. Preparing the adapter may download
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

Starting with `0.1.0-beta.2`, FailForge can discover a prepared SDK automatically:

```sh
export PATH="$DART_SDK/bin:$PATH"
```

For Flutter, add `"$FLUTTER_SDK/bin"` instead. An explicit SDK option or YAML
SDK field takes priority over `PATH`. If no recognizable SDK is found, FailForge
uses the installation's `.toolchain/flutter/bin/cache/dart-sdk` location.
Discovery does not install an SDK, prepare Flutter or check version
compatibility. Nonstandard SDK shims need an explicit root path.
The published `0.1.0-beta.1` archive requires explicit SDK paths; these
automatic defaults were added in `0.1.0-beta.2`.

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

Check these prerequisites before starting:

```sh
failforge doctor --project "$FAILFORGE_PROJECT" \
  --installation "$FAILFORGE_HOME" --dart-sdk "$DART_SDK"
```

For Flutter, use `--flutter-sdk "$FLUTTER_SDK"` instead of `--dart-sdk`.
The command reads local state and suggests manual fixes. It does not install
dependencies or run tests. See [doctor diagnostics](doctor.md) for its scope,
profile mode and exit codes. Historical note: the `0.1.0-beta.1` archive does not
include this command.

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
source file. With the SDK on `PATH`:

```sh
cd "$FAILFORGE_PROJECT"
failforge profile generate \
  --installation "$FAILFORGE_HOME" \
  --file lib/src/calculator.dart \
  --family classic
```

The default output is `profiles/lib/src/calculator.yml`, preserving the
source's directory structure inside the package. An explicit `--output PATH`
selects another destination; replacing an existing file requires `--force`.
Historical note: for the `0.1.0-beta.1` archive, add `--output
profiles/lib/src/calculator.yml` and `--dart-sdk "$DART_SDK"` (or
`--flutter-sdk "$FLUTTER_SDK"`). Explicit SDK options remain supported in
`0.1.0-beta.3` as well.

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
as an absolute path in the YAML. A discovered SDK is not saved; each profile
run discovers it through its own `PATH`.

Run it with:

```sh
failforge mutate \
  --installation "$FAILFORGE_HOME" \
  --config profiles/lib/src/calculator.yml
```

Do not add `--dart-sdk`, `--flutter-sdk`, `--output` or mutation-selection flags
to this command: those settings belong in the profile when using `--config`.
With no YAML `output` setting, FailForge chooses a new report directory under
the target's `.failforge/` directory. An explicit YAML output path is relative
to the profile file, and it must identify a new directory.

When moving a profile with explicit absolute SDK paths, update those paths.
Profiles using discovery can keep the SDK fields omitted. See
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

## Relocate an installation

After building, the runtime can live separately from the source checkout. Keep
the two native executables together and preserve this adapter layout:

```text
installation/
  bin/
    failforge
    failforge-adapter-campaign
  adapters/
    dart/
      bin/
      lib/
      pubspec.yaml
      pubspec.lock
      LICENSE
      NOTICE
```

The [packaging command](packaging.md) creates this runtime layout with the user
documentation and project/dependency license materials. You can also arrange it
manually with executables built for the destination operating system and
architecture. When distributing it, preserve the licenses, notices and inventory
described in the [dependency license review](dependency-licenses.md#distribution-boundaries).

Prepare the adapter at its final location using a compatible SDK:

```sh
export FAILFORGE_HOME="/absolute/path/to/installation"
export DART_SDK="/absolute/path/to/dart-sdk"
export PATH="$FAILFORGE_HOME/bin:$PATH"
"$DART_SDK/bin/dart" --suppress-analytics pub get \
  --enforce-lockfile --directory "$FAILFORGE_HOME/adapters/dart"
```

Do not transfer another machine's `.dart_tool/` directory or package cache.
The SDK and dependencies remain separate prerequisites. When moving an existing
installation, run this preparation again so the dependency paths match its new
location. For Flutter, use its bundled compatible Dart SDK for this step and
select `--flutter-sdk` for campaigns as described above.

For `installation/bin/failforge`, the installation root is discovered from the
executable's location. No `--installation` flag is necessary in this layout.
A symlink to the executable uses the real executable's location. You can still
pass `--installation PATH` to select a different root; an explicit value always
wins, and a relative value is resolved from the directory where you invoke
FailForge.

An executable outside the installed or standard Cargo layouts needs
`--installation PATH`; the error message asks for it. The same selection rules
apply to `mutate`, `replay` and `profile generate`.

The default adapter is `<installation>/adapters/dart`. To use a separately
prepared adapter, pass `--adapter-root PATH` in direct commands or profile
generation; with `mutate --config`, use `adapter_root` in YAML. This override does
not replace installation selection, and the native analysis helper must still
remain beside `failforge`. Add the SDK's `bin/` directory to `PATH`, or pass
`--dart-sdk` or `--flutter-sdk` explicitly. With a YAML profile, explicit SDK
paths belong in the corresponding profile field.

The installed layout does not depend on the original build checkout. Keep the
checkout if you want to rebuild, and retain the adapter's `bin/` and `lib/`
files in the installation.

## Installation troubleshooting

| Symptom | Check |
| --- | --- |
| Archive checksum fails | Stop and obtain the matching, unmodified archive and checksum files before extracting. |
| Executable cannot run on this machine | Check the archive's operating system and architecture. Packaging does not produce a universal executable. |
| Adapter dependency resolution fails | Confirm the SDK is stable Dart `>=3.13.0 <4.0.0` and use the checked-in adapter lockfile. Check the resolver's dependency constraints if it still fails. |
| Installation cannot be determined | Use the installed `bin/` layout or pass `--installation` pointing to the installation root. Custom Cargo output locations need an explicit root. |
| Adapter cannot be found | Prepare `<installation>/adapters/dart`, or select a prepared adapter with `--adapter-root` or the profile's `adapter_root`. |
| Native analysis helper cannot be found | Keep `failforge-adapter-campaign` beside `failforge`; extract the complete archive or build both executables from source. |
| Package metadata is missing | Run the appropriate `pub get` in the target with the selected SDK. |
| Baseline tests fail or time out | Run the target's normal test command first, then review the baseline report and timeout settings. |
| Output already exists | Choose a new report directory. Generated profile files also reject replacement unless `--force` is explicit. |
| Profile flags conflict | Put campaign and SDK settings in YAML; keep `--installation` on the `mutate --config` command. |

For unsupported project layouts and current release constraints, see
[beta limitations](beta-limitations.md).

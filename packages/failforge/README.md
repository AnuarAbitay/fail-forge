# FailForge

Find the bugs your Dart and Flutter tests would miss.

FailForge makes small changes to your source code and runs your tests against
them. Changes that survive identify behavior your assertions may not cover.

This package provides the `failforge` command through a small Dart launcher.
It downloads the native FailForge runtime for your platform and prepares its
Dart adapter. Rust is not required on the user's machine.

**Local prerelease prototype:** this package has not been published to pub.dev.
Launcher `0.1.0-beta.1` installs native FailForge **`0.1.0`**. It does not include
features added to the repository after that release, including the unreleased
HTML report command.

## Requirements

- A stable Dart SDK `>=3.13.0 <4.0.0`, or Flutter with a Dart SDK in that range.
- Linux x86_64 with glibc, macOS arm64, or Windows x64. Windows requires Windows 10 version
  1809 or newer and a local NTFS working directory.
- Network access to GitHub release assets and pub.dev for initial setup.
- A Dart or Flutter package with prepared dependencies and a passing test suite.

These are the native release's supported targets. The launcher needs separate
verification on each platform before publication.

## Try the local package

On macOS or Linux, from the FailForge repository root:

```sh
dart pub global activate --source path packages/failforge
failforge setup
failforge --version
```

Add Pub's executable directory to `PATH` if `failforge` is not found. On macOS
and Linux it is normally `$HOME/.pub-cache/bin`. A custom `PUB_CACHE` changes
that location.

On Windows, use Dart's native CLI installer from PowerShell:

```powershell
$package = (Resolve-Path 'packages/failforge').Path.Replace('\', '/')
dart install "failforge@{path: '$package'}"
failforge setup
failforge --version
```

Add the installation bin directory printed by `dart install` to `PATH`. Keep
the compatible Dart SDK available on `PATH` or through `DART_SDK` for adapter
preparation and mutation analysis. If you previously used Pub global activation,
remove its old batch entry first with `dart pub global deactivate failforge`.

Windows requires this native entry point to preserve Ctrl+C, Ctrl+Break and
the final native exit code. Legacy `dart pub global activate`, `dart run` and
VM snapshot entry points are rejected before setup or a campaign starts, with
instructions to install the native launcher. See Dart's
[CLI installation guide](https://dart.dev/tools/dart-install).

Activation installs the Dart launcher. `failforge setup` downloads and prepares
the native runtime. The first ordinary command also performs setup when needed.
Subsequent commands reuse the prepared runtime.

Local path activation uses Pub's `global run` script, which can print dependency
resolution messages before the command output. The launcher itself sends setup
diagnostics to stderr.

From your target package root, check the environment:

```sh
failforge doctor
```

Then try a small mutation run. Replace the example source path with a file in
your package and choose a new output directory for each run:

```sh
failforge mutate \
  --project . \
  --family classic \
  --operator RELATIONAL_BOUNDARY \
  --include 'lib/src/calculator.dart' \
  --workers 1 \
  --output .failforge/first-run
```

Use `failforge --help` and `failforge mutate --help` for command options.
The [installation guide](https://github.com/AnuarAbitay/fail-forge/blob/v0.1.0/docs/installation.md)
and [mutation guide](https://github.com/AnuarAbitay/fail-forge/blob/v0.1.0/docs/mutation-cli.md)
describe native FailForge `0.1.0`.

## Runtime installation

The launcher selects a fixed release archive for the operating system and CPU
architecture. Downloads use anonymous HTTPS. It checks the archive against a
SHA-256 digest included in the launcher, checks the extracted payload against
its package manifest, and retains both native executables, adapter sources and
license notices together.

The default runtime cache depends on the operating system:

| Platform | Cache root |
| --- | --- |
| macOS | `$HOME/Library/Caches/failforge` |
| Linux | `$XDG_CACHE_HOME/failforge`, or `$HOME/.cache/failforge` |
| Windows | `%LOCALAPPDATA%\failforge\cache` |

Set `FAILFORGE_CACHE` to use a different cache root. Pub's `PUB_CACHE` setting
is inherited when preparing adapter dependencies.

Installations are separated by native release, Dart SDK and Pub cache so different
shell environments do not rewrite an active runtime's adapter configuration.
Setup uses an operating-system lock, retries incomplete preparation, and checks
cached payload hashes before execution.

For a native CLI installation, `dart uninstall failforge` removes the installed
command. For Pub global activation on macOS/Linux, use
`dart pub global deactivate failforge`. The downloaded runtime stays in the
FailForge cache for later reuse.

Adapter dependencies are prepared at the final installation path with
`dart pub get --enforce-lockfile`. The native CLI runs in your original working
directory, receives your command arguments and standard streams, and returns
its exit code.
Keep the compatible Dart or Flutter SDK available for mutation analysis.

## License

FailForge's original source is licensed under the [Apache License, Version 2.0](LICENSE).
See [NOTICE](NOTICE). The downloaded native runtime retains its own dependency
notices and third-party license files.

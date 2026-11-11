# FailForge

Find the bugs your Dart and Flutter tests would miss.

FailForge makes small changes to your source code and runs your tests against
them. Changes that survive identify behavior your assertions may not cover.

This package provides the `failforge` command through a small Dart launcher.
Launcher **`0.2.1`** installs native FailForge **`0.2.0`**, including offline HTML
reports. It downloads the runtime for your platform and prepares its Dart
adapter. Rust is not required on your machine.

## Requirements

- A stable Dart SDK `>=3.13.0 <4.0.0`, or Flutter with a Dart SDK in that range.
- Linux x86_64 with glibc, macOS arm64, or Windows x64. Windows requires Windows
  10 version 1809 or newer and a local NTFS working directory.
- Network access to GitHub release assets and pub.dev for initial setup.
- A Dart or Flutter package with prepared dependencies and a passing test suite.

## Install

On macOS or Linux:

```sh
dart pub global activate failforge 0.2.1
```

Add Pub's executable directory to `PATH` if `failforge` is not found. On macOS
and Linux it is normally `$HOME/.pub-cache/bin`. A custom `PUB_CACHE` changes
that location.

On Windows, use Dart's native CLI installer from PowerShell. If you previously
used Pub global activation, first remove its old batch entry with
`dart pub global deactivate failforge`.

```powershell
dart install failforge@0.2.1
```

Add the installation bin directory printed by `dart install` to `PATH`.

Windows requires this native entry point so Ctrl+C and Ctrl+Break wait for
campaign cleanup and preserve the final exit code. Legacy Pub global, `dart run`
and VM snapshot entry points explain how to migrate before starting work.
See Dart's [CLI installation guide](https://dart.dev/tools/dart-install).

After installing on any platform:

```sh
failforge setup
failforge --version
```

`failforge setup` downloads and prepares the native runtime. The first ordinary
command also performs setup when needed. Subsequent commands reuse the prepared
runtime. Keep a compatible Dart or Flutter SDK available on `PATH` or through
`DART_SDK` for adapter preparation and mutation analysis.

## Run a mutation test

From your target package root, check the environment:

```sh
failforge doctor
```

Choose a source file to mutate and a new output directory. Replace the example
source path with one from your package:

```sh
failforge mutate --project . --family classic --operator RELATIONAL_BOUNDARY --include lib/src/calculator.dart --workers 1 --output .failforge/first-run
```

Export the saved run to a single HTML file:

```sh
failforge report .failforge/first-run --html .failforge/first-run.html
```

Open that file in a browser to inspect scores, mutations, source changes and test
output offline. Choose a new HTML output path for each export.

Use `failforge --help` and `failforge mutate --help` for command options. The
[installation guide](https://github.com/AnuarAbitay/fail-forge/blob/v0.2.0/docs/installation.md),
[mutation guide](https://github.com/AnuarAbitay/fail-forge/blob/v0.2.0/docs/mutation-cli.md)
and [HTML report guide](https://github.com/AnuarAbitay/fail-forge/blob/v0.2.0/docs/html-report.md)
describe native FailForge `0.2.0`.

## Runtime installation

The launcher selects a fixed release archive for your operating system and CPU
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

Installations are separated by native release, Dart SDK and Pub cache, so
different shell environments do not rewrite an active runtime's adapter
configuration. Setup uses an operating-system lock, retries incomplete
preparation, and checks cached payload hashes before execution.

Adapter dependencies are prepared at the final installation path with
`dart pub get --enforce-lockfile`. The native CLI runs in your original working
directory, receives your arguments and standard streams, and returns its exit
code. Setup diagnostics go to stderr.

## Uninstall

On macOS or Linux, remove Pub's command with:

```sh
dart pub global deactivate failforge
```

On Windows, remove the native command with:

```powershell
dart uninstall failforge
```

The downloaded runtime stays in the FailForge cache for later reuse.

## Local development

To try a checkout, run these commands from the FailForge repository root.
On macOS or Linux:

```sh
dart pub global activate --source path packages/failforge
failforge setup
```

On Windows, use the native installer with a local path:

```powershell
$package = (Resolve-Path 'packages/failforge').Path.Replace('\', '/')
dart install "failforge@{path: '$package'}"
failforge setup
```

Local path activation on macOS/Linux uses Pub's `global run` script, which can
print dependency-resolution messages before native stdout.

## License

FailForge's original source is licensed under the [Apache License, Version 2.0](LICENSE).
See [NOTICE](NOTICE). The downloaded native runtime retains its own dependency
notices and third-party license files.

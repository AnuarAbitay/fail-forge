# Windows source build

Native Windows support is being developed in `feature/windows-support`. It is
not included in the published `0.1.0-beta.3` archives. No new beta is required
to build and test this branch.

## Requirements

- Windows 10 version 1809 or newer, x64, with a local NTFS working directory.
- Rust 1.85.1 or newer using `x86_64-pc-windows-msvc`, Visual Studio Build Tools
  with the Desktop development with C++ workload and a Windows SDK.
- Git and PowerShell. Packaging also uses the Windows `tar.exe` utility.
- A prepared stable Dart SDK `>=3.13.0 <4.0.0`, or Flutter containing that SDK.
  Flutter projects require the Flutter SDK and prepared Flutter dependencies.

Windows ARM64, network shares, and filesystems without the required file ID,
locking, ACL, hard-link and atomic rename operations are outside this port's
verification scope. Reparse points in controlled working copies and reports
are rejected. A source checkout can use a normal directory with spaces or
Unicode in its path.

## Build and prepare

Run these commands from the repository root in a Developer PowerShell where
the MSVC linker is available. Adjust the SDK path to your installation:

```powershell
$env:FAILFORGE_HOME = (Get-Location).Path
$env:DART_SDK = 'C:\tools\flutter\bin\cache\dart-sdk'
cargo build --workspace --locked
if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
& "$env:DART_SDK\bin\dart.exe" --suppress-analytics pub get --enforce-lockfile --directory adapters/dart
if ($LASTEXITCODE -ne 0) { throw 'Adapter preparation failed' }
$env:PATH = "$env:FAILFORGE_HOME\target\debug;$env:DART_SDK\bin;$env:PATH"
```

Keep `failforge.exe` and `failforge-adapter-campaign.exe` together. The adapter
sources and their prepared dependencies are also required; copying the CLI
executable alone is insufficient.

Prepare your target package's dependencies, change to its root, and check the
installation:

```powershell
failforge doctor --installation "$env:FAILFORGE_HOME" --dart-sdk "$env:DART_SDK"
failforge mutate --installation "$env:FAILFORGE_HOME" --dart-sdk "$env:DART_SDK" --family classic --include 'lib/src/calculator.dart' --operator RELATIONAL_BOUNDARY
```

Replace the example source path and operator with values applicable to your
package. For Flutter, use `--flutter-sdk C:\tools\flutter` instead of
`--dart-sdk`. The mutation engine invokes Flutter's prepared tool snapshot
through `dart.exe`; it does not send project arguments through a batch shell.
The existing [profile and replay commands](mutation-cli.md) use the same native
Windows backend.

## Process and report behavior

Arguments are native Windows strings. Bare executable lookup uses the child's
effective `PATH`, case-insensitive environment keys and `.exe` candidates.
`failforge run` rejects implicit `.bat` and `.cmd` execution. If a batch shell
is needed, request `cmd.exe` explicitly and supply its arguments yourself.
Completed native commands retain all 32 bits of their exit status.

Each launched process tree belongs to a Job Object before the first child
instruction runs. Timeout, Ctrl+C, controller cancellation and owner death
terminate the whole job. Windows cancellation is immediate termination; the
Unix graceful signal period does not apply. Capture draining and cleanup have
their own budgets after termination. Completed campaign results are retained
when cancellation can finish normally. After abrupt owner death, abandoned
working copies are recovered on a later run using file leases.

Reports use regular files, without requiring Windows symlink privileges.
Family report files are installed before the root `summary.json`. The atomic
replacement of that root summary commits the final generation. During
publication, readers of several files can observe different generations;
wait for final `summary.json` and check the run identifiers before combining
files. `progress.json` remains the live checkpoint.

## Verify locally

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
```

Native Windows CI also exercises real Dart/Flutter commands, mutation and
timeout regressions, retained replay, and a moved packaged installation with
a fresh Pub cache. Cross-compilation on another operating system only checks
types; use the native job results for the revision being evaluated.

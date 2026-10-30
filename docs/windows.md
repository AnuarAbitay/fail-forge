# Windows installation and source build

FailForge `0.1.0-beta.4` includes a native Windows x64 archive. Use the
[beta.4 release](https://github.com/AnuarAbitay/fail-forge/releases/tag/v0.1.0-beta.4)
or build the tagged source. The historical beta.3 downloads are Linux/macOS only.

## Requirements

- Windows 10 version 1809 or newer, x64, with a local NTFS working directory.
- PowerShell and the Windows `tar.exe` utility for archive installation.
- Native archives require the latest supported
  [Microsoft Visual C++ v14 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist?view=msvc-170)
  for x64. Install it before first running either FailForge executable;
  [Microsoft's x64 installer](https://aka.ms/vc14/vc_redist.x64.exe) supplies the runtime.
- A prepared stable Dart SDK `>=3.13.0 <4.0.0`, or Flutter containing that SDK.
  Flutter projects require the Flutter SDK and prepared Flutter dependencies.
- Source builds additionally require Git, Rust 1.85.1 or newer using
  `x86_64-pc-windows-msvc`, Visual Studio Build Tools with the Desktop development
  with C++ workload and a Windows SDK. Archive users do not need Rust, Cargo or
  MSVC build tools.

Windows ARM64, network shares, and filesystems without the required file ID,
locking, ACL, hard-link and atomic rename operations are outside this port's
verification scope. Reparse points in controlled working copies and reports
are rejected. An installation or source checkout can use a normal directory
with spaces or Unicode in its path.

## Install from an archive

Download these three matching assets from the release:

- `failforge-0.1.0-beta.4-x86_64-pc-windows-msvc.tar.gz`
- `package-manifest-x86_64-pc-windows-msvc.json`
- `SHA256SUMS-x86_64-pc-windows-msvc`

In PowerShell, change to their download directory and verify both entries in the
checksum file before extracting. Obtain all three files from the same trusted
source; checksums are not digital signatures.

```powershell
$ErrorActionPreference = 'Stop'
Set-Location 'C:\Downloads\FailForge'
$target = 'x86_64-pc-windows-msvc'
$archiveRoot = "failforge-0.1.0-beta.4-$target"
$expected = @("$archiveRoot.tar.gz", "package-manifest-$target.json")
$checked = @{}
foreach ($line in Get-Content "SHA256SUMS-$target") {
    if ($line -notmatch '^([0-9a-f]{64})  ([A-Za-z0-9._-]+)$') { throw 'Invalid checksum record' }
    $name = $Matches[2]
    $hash = $Matches[1]
    if ($expected -notcontains $name -or $checked.ContainsKey($name)) { throw 'Unexpected checksum entry' }
    if ((Get-FileHash -Algorithm SHA256 -LiteralPath $name).Hash.ToLowerInvariant() -ne $hash) { throw "Checksum mismatch: $name" }
    $checked[$name] = $true
}
if ($checked.Count -ne 2) { throw 'Missing archive or manifest checksum' }
```

Choose new extraction and installation directories. This example moves the
installation before preparing its adapter:

```powershell
$parent = 'C:\tools\failforge-extracted'
New-Item -ItemType Directory $parent | Out-Null
tar.exe -xzf "$archiveRoot.tar.gz" -C $parent
if ($LASTEXITCODE -ne 0) { throw 'Archive extraction failed' }
$env:FAILFORGE_HOME = 'C:\tools\FailForge'
if (Test-Path -LiteralPath $env:FAILFORGE_HOME) { throw 'Choose a new installation directory' }
Move-Item -LiteralPath (Join-Path $parent $archiveRoot) -Destination $env:FAILFORGE_HOME
$env:PATH = "$env:FAILFORGE_HOME\bin;$env:PATH"
failforge --version
if ($LASTEXITCODE -ne 0) { throw 'CLI startup failed' }
failforge --help
```

Keep the entire installation together, including `bin\failforge.exe`,
`bin\failforge-adapter-campaign.exe`, adapter sources and license material.
Continue with [Prepare the adapter and target](#prepare-the-adapter-and-target).
If you move the installation again, prepare the adapter at its new location.
The environment settings above apply to the current PowerShell session.

## Build from source

In a Developer PowerShell where the MSVC linker is available:

```powershell
$ErrorActionPreference = 'Stop'
git clone --branch v0.1.0-beta.4 --depth 1 https://github.com/AnuarAbitay/fail-forge.git
if ($LASTEXITCODE -ne 0) { throw 'Source download failed' }
Set-Location fail-forge
$env:FAILFORGE_HOME = (Get-Location).Path
cargo build --workspace --locked
if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
$env:PATH = "$env:FAILFORGE_HOME\target\debug;$env:PATH"
```

Keep `failforge.exe` and `failforge-adapter-campaign.exe` together in
`target\debug`. The adapter sources and their prepared dependencies are also
required; copying the CLI executable alone is insufficient.

## Prepare the adapter and target

Select the actual Dart SDK root. For Flutter, use its bundled Dart SDK after
completing Flutter's normal setup. A standalone Dart SDK also works:

```powershell
$env:DART_SDK = 'C:\tools\flutter\bin\cache\dart-sdk'
& "$env:DART_SDK\bin\dart.exe" --version
if ($LASTEXITCODE -ne 0) { throw 'Dart startup failed' }
& "$env:DART_SDK\bin\dart.exe" --suppress-analytics pub get --enforce-lockfile --directory "$env:FAILFORGE_HOME\adapters\dart"
if ($LASTEXITCODE -ne 0) { throw 'Adapter preparation failed' }
$env:PATH = "$env:DART_SDK\bin;$env:PATH"
```

Do not copy another machine's `.dart_tool` or Pub cache. Prepare your target
package's dependencies, run its normal tests, then check the installation. For a
Dart package:

```powershell
$env:FAILFORGE_PROJECT = 'C:\projects\your_package'
Set-Location $env:FAILFORGE_PROJECT
& "$env:DART_SDK\bin\dart.exe" pub get
if ($LASTEXITCODE -ne 0) { throw 'Target preparation failed' }
& "$env:DART_SDK\bin\dart.exe" test
if ($LASTEXITCODE -ne 0) { throw 'Target tests failed' }
failforge doctor --installation "$env:FAILFORGE_HOME" --project "$env:FAILFORGE_PROJECT" --dart-sdk "$env:DART_SDK"
if ($LASTEXITCODE -ne 0) { throw 'Resolve the doctor diagnostics before running a campaign' }
failforge mutate --installation "$env:FAILFORGE_HOME" --dart-sdk "$env:DART_SDK" --family classic --include 'lib/src/calculator.dart' --operator RELATIONAL_BOUNDARY
```

Replace the example source path and operator with values applicable to your
package. For Flutter, prepare and test the target with its normal `flutter pub get`
and `flutter test` commands, then use `--flutter-sdk C:\tools\flutter` instead of
`--dart-sdk` for `doctor` and `mutate`. The mutation engine invokes Flutter's
prepared tool snapshot through `dart.exe`; it does not send project arguments
through a batch shell.
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

Runner children use redirected standard streams and do not inherit the public
CLI's console. Console events are handled by the CLI, which cancels the owned
job; Ctrl+Break therefore cannot terminate a captured child ahead of that request.

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

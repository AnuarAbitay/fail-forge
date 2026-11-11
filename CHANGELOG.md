# Changelog

User-visible changes are recorded by version. Check
[GitHub Releases](https://github.com/AnuarAbitay/fail-forge/releases) for published
downloads and their verification results.

## 0.2.0

This native version includes the local Pub launcher `0.1.0-beta.1` prototype,
which pins native `0.1.0`. Launcher `0.2.0` requires a separate package update
with the verified archive and manifest hashes of the matching native assets.

### Added

- A local Dart Pub launcher prototype in `packages/failforge`, version
  `0.1.0-beta.1`, installs native FailForge `0.1.0` through `failforge setup` or
  first use. It verifies pinned release and payload hashes, prepares adapter
  dependencies, isolates SDK/Pub cache configurations and preserves native
  command arguments, streams, exit status and Unix cancellation. Includes
  package documentation, tests and a three-platform CI workflow. The package
  has not yet been published to pub.dev.
- `failforge report RUN --html FILE` exports a saved direct campaign or
  multi-family profile as one offline HTML report. Browse source files and
  mutant outcomes, inspect exact mutation edits, and review retained test
  evidence and bounded output captures. Family scores remain separate;
  incomplete campaigns have no final score.
- Mutation details include a one-based source line and UTF-16 column, with
  nearby original code and the affected lines highlighted. New runs retain a
  bounded source context; older runs can recover it with `report --source-root`
  only when the source hash and edit match the saved run.
- **Copy location** copies a relative `file:line:column` from a standalone HTML
  report, using one-based lines and UTF-16 columns. If clipboard access is
  unavailable, selected text remains available for manual copying.

### Changed

- A fully processed campaign can retain its family scores when individual
  mutants have `INFRA_ERROR` outcomes. Those mutants stay excluded from the
  denominator, the HTML report names the errors and the scored scope, and the
  CLI still returns `125`. Cancellation and campaign-wide preparation, control,
  integrity or cleanup failures remain unscored.

### Fixed

- Preserve Windows Ctrl+C and Ctrl+Break cleanup and exit status in the Pub
  launcher through Dart's standard native CLI installer. Legacy Windows VM
  entry points stop before starting a campaign and explain the migration.
- Await archive file writes in the Pub launcher so open/write/flush/close
  failures reach normal error handling, resources close, and setup can retry.
  Validate cached manifest, preparation marker and lock file types before
  opening streams, avoiding hangs on unexpected special files.
- Accept unpaired UTF-16 code units in Dart reporter error, stack-trace and
  print diagnostics using a lossless internal encoding. Unicode diagnostics no
  longer turn complete test failures into `malformed_reporter_output` errors.
  Protocol identities, metadata and private context documents retain strict
  Unicode validation, and raw captures remain unchanged.

## 0.1.0

### Changed

- Windows executables link the Visual C++ runtime statically. Archives no longer
  require the Visual C++ Redistributable, which previously made both executables
  exit with `0xC0000135` and no message. Packaging rejects any executable that
  still imports the dynamic runtime, including delay-loaded and pre-2015
  runtimes.
- Windows integrity checks within one run reuse a file's digest while its
  identity, size, write time and change time are unchanged and no other
  process has it open for writing. Files changed in the last two seconds are
  always reread. A profile no longer rereads the Dart SDK and Pub cache for
  every family.
- Windows terminal output and error messages show paths without the `\\?\`
  prefix when the ordinary spelling is unambiguous.
- `doctor` prints a `Cause:` line with the failing path and error for adapter
  dependency and project checks, before the suggested fix.

### Fixed

- Let Windows author tests change into their retained attempt directories without
  conflicting with a directory handle held by FailForge. Directory identity and
  cleanup checks remain in place.
- Confirm repeated Dart stack overflows when the VM starts the trace at different
  points in the same recognizable recursive cycle. Unrecognized traces retain
  exact diagnostic matching, and healthy original controls are still required.
- Use shorter private Windows test paths to avoid Dart startup failures caused
  by long attempt paths. Recovery still recognizes existing attempt directories.
- Close FailForge's own Windows writer before binding the rollback target's
  identity, avoiding false reports that the rollback target changed.
- Recheck Windows cached input digests under a retained read guard so an edit
  completed between the stamp lookup and writer check cannot reuse a stale hash.
- Keep captured Windows child processes outside the controller's console so
  Ctrl+Break is handled through the CLI's owned Job Object cancellation rather
  than terminating Dart first and producing an infrastructure error.
- Accept a leading UTF-8 byte-order mark in mutation profiles and project
  `pubspec.yaml` files, as written by Windows PowerShell 5.1 and some editors.
  Such files previously failed with `invalid YAML configuration`.
- On Windows, system sleep no longer expires command or adapter request
  timeouts, which now count only awake time. `mutate`, `replay` and
  `profile generate` hold a power request against idle sleep and Modern
  Standby. A control run that timed out while the system slept names the sleep
  in its error, and runs report how long the system slept.
- Windows input errors include the path and the operating-system error. An entry
  redirected by a file-system filter (MSIX virtualization, sandboxes, sync
  clients) now reports NTSTATUS `0xC000050B` with a remedy instead of a bare
  localized message.

## 0.1.0-beta.4 — 2026-10-28

### Added

- Native Windows x64 process execution, mutation campaigns, profiles and replay,
  with Job Object process ownership, private handle-based workspaces, named-pipe
  cancellation, Windows SDK discovery and native CI coverage. Native archives
  are available for Windows x64 alongside Linux x86_64 and macOS arm64.
- Manual Windows x64 release packaging with exact-source Linux/macOS and Windows
  CI checks before upload to an existing draft release.

## 0.1.0-beta.3 — 2026-10-24

### Changed

- Accept stable Dart SDKs `>=3.13.0 <4.0.0`; `doctor` warns for minor versions
  above 3.13 without changing the exit code. Keep analyzer pinned to `14.5.0`
  and distinguish accepted SDKs from the locally checked Flutter 3.47.7 / Dart
  3.13.5 and standalone Dart 3.13.0 environments.
- Check the pinned Flutter SDK, latest stable Flutter and minimum Dart 3.13.0
  in CI. A wider accepted range does not establish future SDK compatibility.

### Fixed

- Keep the replay cancellation regression's command budget large enough for
  Dart compiler and test startup while preserving timeout and cancellation
  verdict checks.
- Refresh dependency notice fingerprints after the SDK constraint update and
  verify the checked-in inventory before packaging.

## 0.1.0-beta.2 — 2026-10-21

### Added

- Read-only `failforge doctor` diagnostics for SDK compatibility, prepared
  adapter and project dependencies, test prerequisites and environment paths,
  with remediation commands and support for checking an existing YAML profile.
- Automatic discovery of prepared Dart and Flutter SDKs through `PATH` when no
  explicit SDK is configured. Explicit paths take priority; the installation's
  `.toolchain` SDK remains the fallback.
- Optional `profile generate --output`: the default mirrors the Dart source
  path under the package's `profiles/` directory. SDKs discovered through `PATH`
  are not embedded into generated profiles.

## 0.1.0-beta.1 — 2026-10-21

### Added

- Mutation testing for standalone Dart packages and Flutter test projects,
  with direct command arguments or an explicit YAML profile.
- Three mutation families: 31 default classical operators, two failure-mode
  operators and 19 Dart-specific operators. The legacy
  `CONDITIONAL_NEGATION` operator remains available by explicit selection.
- Profile generation for a Dart library file, including applicability analysis,
  explicit operator switches, template mode and preview without writing a file.
- File, class, method and operator filters; one to four concurrent workers;
  separate test and command time limits.
- Fresh isolated copies for test attempts, original test controls, input
  integrity checks, cancellation cleanup and recovery of recognized abandoned
  workspaces after a crash.
- Terminal result tables and retained JSON reports with per-family scores,
  mutant outcomes and diagnostic evidence.
- Replay of a verified retained classical mutation set.
- User guides for installation, mutation operators and beta limitations, plus a
  security reporting policy.
- Apache-2.0 licensing for FailForge's original source code.
- A dependency license inventory with retained third-party notices, archive
  checksum verification and distribution scope notes.
- Local native beta packaging with `failforge-tools package`: a `.tar.gz` archive
  containing the CLI, analysis helper, adapter source, user documentation and
  license material, plus checksums and an inventory of packaged files. SDKs and
  prepared Pub dependencies remain separate installation requirements.
- An installed-archive smoke check for fresh dependency preparation, relocation,
  profile generation, direct/YAML identity parity, source preservation and cleanup.

### Fixed

- Permanent output-read errors are recorded without waiting for a readability
  event that Linux may never emit; capture workers can then finish cleanup.
- Relocated installations find their installation root relative to the executable
  instead of the original build checkout. Standard source-build locations work
  as well; `--installation` remains an explicit override for custom layouts.
- Invalid Dart package configuration is rejected before starting a test process.
  Malformed metadata, duplicate package roots and an alias pointing outside the
  current working copy are reported as infrastructure failures.
- Completed infrastructure failure records are preserved when cancellation or
  source changes are observed afterward.
- A profile that names a nonexistent literal source path fails as a
  configuration error before creating the report or starting tests.

### Current limitations

- One standalone package is supported per campaign; Pub workspaces and source
  symlinks are rejected.
- Target and adapter dependencies must be prepared before a run. FailForge does
  not run `pub get` for the target.
- Interrupted campaigns retain completed results but cannot automatically resume
  from the next mutant.
- Applicability varies by construct and resolved type. A surviving mutant may be
  equivalent to the original, and incomplete or inconclusive results do not
  establish a final mutation score.

See the [mutation catalogue](docs/mutation-operators.md) and
[campaign guide](docs/mutation-cli.md) for supported operators and usage.

# Changelog

User-visible changes are recorded by version. Check
[GitHub Releases](https://github.com/AnuarAbitay/fail-forge/releases) for published
downloads and their verification results.

## Unreleased

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

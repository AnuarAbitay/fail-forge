# Changelog

User-visible changes are recorded here. The current beta preparation is
unreleased; no published release or release date is implied.

## [Unreleased]

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

### Fixed

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

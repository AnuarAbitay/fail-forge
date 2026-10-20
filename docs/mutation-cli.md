# Mutation campaigns

`failforge mutate` runs explicitly selected mutations. A YAML profile can select
multiple families; direct CLI arguments select one family.
The Rust CLI reads, validates and normalizes both launch modes through
`failforge-config`, then replaces itself with the Python execution controller.
The effective configuration is transferred through an unlinked regular-file
descriptor and consumed once; the controller does not re-read the YAML or apply
its Python configuration defaults on this path. Each native test command
is owned by the existing Rust Runner; the controller does not implement another
test timeout or kill that cleanup owner.

Primary Dart/Flutter reporter parsing, author-test catalogue comparison and
outcome classification run in Rust. This includes conservative setup-suppression
analysis, independent ordinary-failure witnesses and initialization confirmation.
The native attempt owner authenticates both raw captures against the Runner
result before accepting its verdict. A private input descriptor transfers the opaque
baseline catalogue and expected test selection to each native owner before it
starts the test process. For an eligible loading failure, Rust requests the next
original/mutant/original control through opaque state and validates its order and
candidate binding. The native mutant owner executes those controls and checks
input bindings between them. A native family owner runs the baseline, worker queue
and final original control. The campaign controller retains profile family ordering,
campaign-wide input checkpoints, cancellation and aggregate report assembly.

Prepared package inspection, source and tool hashes, dependency bindings,
snapshot copying and fresh attempt copying execute in Rust through
`failforge-workspace`. Each campaign has its own native input client, including
native family dispatch; native errors fail the operation without a Python fallback.
Copy requests retain the campaign workspace lease. Read-only source checks still
execute after cancellation and report changed inputs as infrastructure failure.
Registry allocation, leases, recovery and removal of owned copies also execute
in Rust. Dart namespace scanning, copy rewrites, inverse proofs and read-only
analyzer planning also execute in Rust through `failforge-dart` and the existing
Runner. The native family owner retains its namespace-plan cache. The controller
retains inherited descriptor capabilities, campaign context lifetimes, profile
family ordering and aggregate report assembly.

Discovery also runs through one native owner. It starts the persistent
`AdapterClient` directly, validates the handshake, complete discovery and each
candidate, checks source-bound patches, drains the adapter and Runner, verifies
unchanged inputs and publishes the transcript and receipt. The controller sends
one private request while retaining the campaign lease; it neither starts the
adapter helper nor re-parses its protocol journal on this production path.
The receipt's `command` remains the reproducible helper recipe. Discovery
artifacts include `payload.json`, `adapter-session.jsonl`, `adapter.stderr` and
`result.json`. Transcript records retain the 8 MiB limit, and the complete native
transcript and private response are each bounded to 64 MiB. Exceeding a bound
fails discovery without publishing candidates for execution.
Source verification retains its 4 MiB per-file limit. The native owner also
authenticates raw adapter captures, accepting at most 128 MiB of stdout and
8 MiB of stderr; a capture issue or an exceeded bound fails the operation.
When the output directory remains valid, cancellation or failure also leaves a
receipt with `freshStaticValidation: false`: `exitCode` is 130 for SIGINT,
143 for SIGTERM or 125 for an infrastructure failure. A cleanly closed adapter
session does not override a failed final source or artifact check.

One native owner now performs each complete attempt: allocation, fresh copying,
namespace preparation, source-bound patch application, test execution, capture
authentication, input verification, temporary-copy cleanup and private report
publication. The controller borrows four workspace capabilities while that owner
runs. It transports the expected patch hash; Rust independently reconstructs the
patch from the bound mutant and rejects a mismatch before starting tests.
Opaque baseline catalogues, initialization continuations and namespace plans stay
outside public attempt JSON. The native family owner retains them directly between
shared attempt calls. The standalone `TestExecutor` bridge also validates private
responses without parsing raw captures or rewriting the native result.

A complete mutant is now one native operation as well. Rust checks the candidate
and source-bound patch before calling the shared attempt implementation directly.
Statically invalid candidates receive an `INVALID` record without starting author
tests. Initialization failures follow the existing native causal policy through
at most three fresh attempts: original-before, mutant-replay and original-after.
The owner checks the frozen campaign bindings before and after each control and
publishes the final `result.json` with mutant metadata and the public control
proof. The Python bridge retains the workspace capabilities until completion;
it does not repeat the patch checks, schedule those controls or rewrite that
record. Private continuation state remains outside public JSON.

Each selected family runs through one native scheduling operation. Rust checks
all candidates and frozen inputs, establishes a passing baseline, writes its
catalogue, executes a bounded queue of one to four workers, and runs the final
original control. Workers call shared native mutation and attempt functions
directly. Cancellation stops admission and waits for all active workers and their
temporary-copy cleanup. An empty direct campaign runs both original controls;
an empty profile family skips author execution.

The controller receives progress and completed records as bounded LF-framed JSON
events, draining stderr concurrently. Each frame is limited to 64 MiB; the event
count is limited to selected mutants plus nine. A terminal event commits the
preceding raw bytes with SHA-256 and validates the completed count. Records arriving
after cancellation remain in the partial report. If a progress or report callback
fails, the controller requests cancellation and retains subsequent records without
calling that failing publication path again. A successful native terminal still
requires the outer input checks, workspace cleanup and final aggregate publication
before the campaign can report success.

An attempt becomes terminal after source verification and cleanup. Cancellation
observed before that point is recorded in its result; a later cancellation stops
subsequent campaign work without changing an already sealed attempt. Input,
capture or cleanup failures still take precedence over cancellation. The public
`command` retains the reproducible `failforge run ... -- dart ...` or Flutter
recipe. The attempt owner starts the SDK child directly through Runner; it does
not recursively invoke that recipe. `exitCode` records the native test-execution
exit policy. Successful private transport can therefore
return a killed, timed-out or cancelled attempt as a complete result.

Configuration loading itself needs neither Python nor an SDK and never starts
author tests. Python is still required for campaign execution. Profile generation
runs through the native `failforge-profile` crate without Python or PyYAML. The native
configuration library also exposes the direct settings and profile APIs for
later migration stages. Profile family ordering, campaign preparation and aggregate
reports still use the Python controller. Standalone diagnostic tools
retain their Python transcript implementation during migration; a native
production error never falls back to that implementation.

## Prepare the installation and target

The first distribution uses this repository's installation layout, Python 3.9+
and PyYAML. Build both native executables and prepare the adapter separately:

```sh
cargo build --workspace --locked
python3 -m pip install -r python/requirements.txt
dart --suppress-analytics pub get --enforce-lockfile --directory adapters/dart
```

The target must already have its dependencies prepared and contain
`pubspec.yaml`, `.dart_tool/package_config.json` and
`.dart_tool/package_graph.json`. The campaign never runs `pub get`, changes a
target's dependencies or downloads its source. This version supports one
standalone Dart package, with `test` or `flutter_test`; Pub workspaces and source
symlinks are rejected explicitly.

Fresh copies preserve source bytes, read/write/execute permission bits and modification times,
including directory permissions, except for the deliberate package-config URI
rebase. Their enclosing directories remain private (`0700`). Retained tool state
must also contain only regular files and directories; aliases and special files
are rejected. Extended attributes and ACLs are outside this copy contract.
Cleanup can remove owned read-only copied directories without changing the
original project's permissions.

The default SDK is `.toolchain/flutter/bin/cache/dart-sdk` in the installation.
Use `--dart-sdk /absolute/dart-sdk` for another Dart SDK, or
`--flutter-sdk /absolute/flutter` for Flutter. The SDK must match the adapter's
documented version constraints. A Flutter project can also use the installation's
Flutter SDK when its Dart SDK has that layout. Runtime paths and effective rules
are recorded in the manifest.

`--installation PATH` selects a relocated installation containing `python/`,
`scripts/` and `adapters/`. `--python PROGRAM` selects the interpreter, including
a virtual environment. `failforge` and `failforge-adapter-campaign` must be in the
same binary directory. A standalone copied `failforge` binary is insufficient.

## Generate a profile

`failforge profile generate` creates an explicit YAML profile for one existing
Dart source file under `lib/`. By default it discovers mutations with the
existing Dart adapter and statically validates them. An operator is written as
`true` only when at least one candidate in that file is statically valid. An
operator with no candidates, or only statically invalid candidates, is `false`.
Generation does not execute author tests or run a mutation campaign.
Run this example from a prepared target package:

```sh
failforge profile generate --file lib/src/calculator.dart --family classic \
  --output profiles/calculator.yml
```

Default analysis requires the prepared package metadata, dependencies, SDK and
adapter described above. `--dart-sdk PATH` or `--flutter-sdk PATH` selects an SDK;
they are mutually exclusive. `--adapter-root PATH` selects a prepared adapter.
Explicit SDK and adapter overrides are resolved from the invocation directory
and saved as absolute YAML paths, so the suggested `mutate --config` command uses
the same selections. Generation never installs dependencies or runs `pub get`.

Use `--template` to write the catalogue defaults without source analysis or
prepared dependencies:

```sh
failforge profile generate --template --file lib/src/calculator.dart \
  --family classic --output profiles/calculator.yml
```

Template mode still checks the source/profile paths and writes explicit switches,
but it does not determine which operators apply to the file. It needs neither
Python nor an SDK. `--template` cannot be combined with SDK or adapter
overrides.

`--file`, at least one `--family`, and `--output` are required. Relative CLI paths
are resolved from the invocation directory. The source must be a regular
`.dart` file under that package's `lib/`; its nearest parent `pubspec.yaml`
identifies the package. Files outside `lib/` are rejected before writing a
profile because the mutation engine discovers library sources. Existing
generated-file exclusions apply during analysis and the campaign. The
generated YAML contains a literal, project-relative `rules.include` for that
file, with no `version` or `project` field. Source paths containing glob syntax
(`*`, `?`, `[`, `]`, `{`, `}`) or backslashes are rejected so the include cannot
expand to other files. Spaces, Unicode and quotes are supported.
Components inside the package use their actual filesystem names, including when macOS
accepts a differently cased or Unicode-normalized spelling. This keeps the
generated include consistent with the adapter's case-sensitive file matching.
Ancestors above the package need only search permission; generation does not
enumerate their contents.
The output must be a `.yml` or `.yaml` file inside that same package, so
the nearest parent `pubspec.yaml` also identifies the same project when the
profile is used later. Here `--output` names the YAML file; it does not set the
campaign's report directory or add a YAML `output` setting.

Repeat `--family` to select multiple families. `--language dart` is required
when `language_specific` is selected and is rejected otherwise:

```sh
failforge profile generate --file lib/src/calculator.dart \
  --family classic --family language_specific --language dart \
  --output profiles/calculator.yml --dry-run
```

Each selected family gets `enabled: true` and its complete known operator
catalogue as explicit lower-case boolean switches. Omitted families are off.
Without `--operator`, analysis considers the 31 default classical operators and
all 19 Dart semantic operators in the selected families. The legacy
`CONDITIONAL_NEGATION` stays off unless explicitly selected. For `failure_mode`,
it considers `EXCEPTION_AT_ENTRY` and additionally `METHOD_CHANNEL_ERROR` when
the prepared test framework is Flutter, identified by `flutter_test` in
`.dart_tool/package_config.json`. A dependency on `flutter` alone does not
establish that test framework. Only operators with a statically valid candidate
are enabled in the resulting profile.

Repeat `--operator` to restrict analysis to those IDs across the selected
families. This whitelist does not force an inapplicable operator on. All other
operators in their catalogues are written as `false`. Unknown IDs and IDs
outside the selected families are rejected:

```sh
failforge profile generate --file lib/src/calculator.dart \
  --family classic --family language_specific --language dart \
  --operator RELATIONAL_BOUNDARY --operator SWM-001 \
  --output profiles/calculator.yml
```

An explicitly selected family whose operators are all `false` remains visible
in the YAML but creates no campaign, matching the normal profile rules.
An explicit `METHOD_CHANNEL_ERROR` requires a Flutter package.
If analysis finds no applicable operator in any selected family, generation
reports that no profile was written and creates neither YAML nor output
directories.

Template mode writes the same complete catalogues but uses the catalogue
defaults or explicit operator whitelist directly, without static validation.
When prepared package metadata is absent, an explicit
`flutter_test: {sdk: flutter}` dependency in either `dependencies` or
`dev_dependencies` identifies the Flutter test framework for the template.
Empty or null dependency sections are accepted.

Analysis uses the existing family discovery limit of 10,000 candidates per
family. Adapter failures, incomplete or truncated discovery, failed validation,
changed inputs and incomplete cleanup stop generation without publishing YAML.
Statically invalid candidates are ordinary analysis results; they cannot enable
an operator without a separate valid candidate.

Generated settings use one worker, `test: 30s`, `command: 120s`, and
`max_mutants: 1000`. They remain editable before running the campaign.
`--dry-run` performs the selected analysis and prints the YAML without publishing
a profile or creating its output directories. Temporary analysis workspaces are
cleaned afterward. Existing destinations are
protected by default. `--force` permits replacing an existing regular `.yml` or
`.yaml` file. Reserved package configuration names (`pubspec.yaml`,
`pubspec_overrides.yaml`, `analysis_options.yaml`, `build.yaml`, `dart_test.yaml`,
`l10n.yaml`) cannot be used as destinations, including case variants. Both source and output paths must
stay outside `.git/`, `.dart_tool/`, `.failforge/`, `build/`, `target/` and
`__pycache__/` directories, also ignoring case. Symlinks anywhere in the
source or output path within the package, directories and special files are
rejected. Publication binds the selected package and destination directory
identities during preparation and checks them again around the atomic write.
Observed directory moves, replacements and changes of the nearest package
are errors. A failed write restores the previous profile and cleans its own
unpublished temporary files. If the filesystem also refuses restoration, the
recoverable `.failforge-profile-backup-*.tmp` copy is retained for recovery.

Generation uses the existing installation layout selected by `--installation PATH`.
The legacy `--python PROGRAM` option is accepted for compatibility and has no
effect on generation. Analysis preserves the source, package metadata, dependencies, SDK and
adapter inputs and cleans its temporary workspaces. It does not execute author
tests; template mode also skips discovery and static validation.
After writing the file, the command prints its path and a suggested
`failforge mutate --config ...` command including the selected installation.
The campaign still requires its Python controller. Run that command separately after
preparing the target and adapter as described above.

## Explicit CLI configuration

```sh
failforge mutate --project ./my_package --family classic
failforge mutate --project ./my_package --family failure_mode
failforge mutate --project ./my_package --family language_specific --language dart
```

`--project` defaults to the invocation directory. `--family` is required without
`--config`. The CLI does not discover or read `failforge.yml` automatically.
`classic` enables the adapter's 31 default classical operators. `failure_mode`
enables `EXCEPTION_AT_ENTRY` for Dart, plus `METHOD_CHANNEL_ERROR` for Flutter.
An explicitly selected operator must belong to the chosen family. Method-channel
mutation requires a prepared Flutter project.

`language_specific` selects the Dart semantic catalogue, with 19 operators in
six categories. `--language dart` names the subtype explicitly. See the
[Dart semantic mutation guide](dart-semantic-mutations.md) for operator IDs
and YAML selection. This family's report uses
`languageSpecificScore`, and the terminal summary includes `Language: dart`.

Optional filters and execution settings:

```sh
failforge mutate --project ./my_package --family classic \
  --operator BOOLEAN_LITERAL_FLIP --include 'lib/**.dart' \
  --method enabled --workers 2 \
  --test-timeout 30s --command-timeout 120s \
  --output /absolute/new-run-directory
```

`--operator`, `--include`, `--exclude`, `--class` and `--method` are repeatable.
The adapter combines different filter categories with AND, and entries inside a
category with OR. The visible default scope for both families is `lib/**.dart`;
normal generated/test/dependency exclusions still apply. Explicit empty lists in
YAML preserve empty selection. The legacy `CONDITIONAL_NEGATION` operator remains
available through explicit selection.

## Explicit YAML configuration

```sh
failforge mutate --config failforge.yml
```

One file is one profile. The only supported YAML format uses explicit family
and operator switches. There is no `version` or `project` field. The closest
ancestor containing a readable regular `pubspec.yaml`, starting at the profile's
directory, identifies the project. A missing pubspec is a configuration error;
the invocation directory does not select a different project.

Prepared profiles are inside the Flutter fixture:
[classic](../examples/flutter_fixture/profiles/failforge.classic.yml),
[failure mode](../examples/flutter_fixture/profiles/failforge.failure-mode.yml),
[Dart semantic](../examples/flutter_fixture/profiles/failforge.dart-semantic.yml),
and [one source file](../examples/flutter_fixture/profiles/subject.yml).
Prepare the fixture's dependencies before using them.

For example, place this file at `my_package/profiles/calculator.yml`:

```yaml
workers: 2
timeouts:
  test: 30s
  command: 120s
rules:
  include: ['lib/src/calculator.dart']
  exclude: ['**/*.g.dart', '**/*.freezed.dart']
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
      SWM-001: true
      SWM-002: false
limits:
  max_mutants: 1000
```

Run it from any directory with `failforge mutate --config /path/to/my_package/profiles/calculator.yml`.
`rules.include`/`exclude` paths are relative to the project root. Other YAML paths
(`dart_sdk`, `flutter_sdk`, `adapter_root`, `output`) are relative to the profile.
CLI paths remain relative to the invocation directory. Only one of `dart_sdk`
and `flutter_sdk` may be supplied. `rules.classes` and `rules.methods` filter
mutations; they do not restrict the project's selected author tests.

Selection rules:

- Absent families, absent `enabled`, absent operators and explicit `false` are
  off. Both a family's `enabled` and an operator must be `true` to run it.
- `language_specific` requires explicit `language: dart` whenever that section
  exists, even when disabled. No language is inferred for an absent section.
- Unknown fields and operator IDs, wrong-family operators and non-boolean
  switches are rejected even in disabled sections. Use unquoted lower-case
  `true`/`false`; strings, numbers and YAML's `yes`/`on` spellings are rejected.
- A profile without any active operators is a configuration error. An active
  family with no enabled operators does not create a family campaign.
- `rules.operators`, `rules.components`, old top-level `family`/`language`, and
  old `mutations.dart_semantic` category settings are rejected. Select individual
  operators under their family. Upgrades do not enable new operators silently.
- A literal include naming a nonexistent file fails before output or workspace
  creation. An existing file with no applicable mutations completes with
  `no_mutants`, without running author tests or establishing a score.

Duplicate keys, YAML tags/anchors/aliases, multiple documents and files larger
than 1 MiB are rejected. YAML settings cannot be mixed with semantic CLI settings,
including `--project` and `--output`; interpreter and installation options remain
available in either mode. Old YAML formats are not supported.

Defaults are one worker, `test: 30s`, `command: 120s` and 1,000 mutants. Workers
range from 1 to 4; `max_mutants` ranges from 0 to 10,000. A profile discovers and
statically validates every selected family before any author tests. The limit
applies to their total candidate count, including statically invalid candidates.
Exceeding it fails the profile before baselines or mutants run; no partial subset
is silently selected. Each family also retains the adapter's 10,000-candidate
hard ceiling. Families execute in order: `classic`, `failure_mode`, then
`language_specific`; `workers` controls concurrent mutants within that family.
Both durations use positive integer `ms`, `s` or `m` units. The command budget
must exceed the test default by at least five seconds. It bounds the entire test
process; metadata, multiple timeouts and teardown can still reach the outer limit.

## Results and correctness

The controller requests a native snapshot of the prepared project, runs fresh discovery and static
validation, establishes a passing baseline, executes each valid mutant in a
fresh temporary copy and checks an original control afterward. Framework-name
collisions such as `pool` use the unchanged snapshot for the test framework and a
separate subject alias. Unsupported alias rewrites fail explicitly. Ordinary
packages retain their self package imports. Originals, snapshots, dependencies,
SDK and adapter inputs are checked for changes.

A failing `setUpAll` can prevent some tests and child groups from starting, so
their names never appear in the JSON reporter. When the baseline and observed
group metadata explain this suppression, a separately observed ordinary test
can still establish `KILLED`: its suite, name, group ancestry, source location and
metadata must uniquely match a passing baseline test, with a verified author
failure. The result retains the actual observed catalogue, missing-entry counts
and `catalogueIdentityProven: false`. It never invents unstarted test results.
Without that witness, the controller records `UNCLASSIFIED` with
`author_test_catalogue_incomplete_after_setup_failure` and excludes it from the
score. `SURVIVED` requires the complete expected catalogue. Unexplained selection
changes and execution-input changes still fail the guard.

An author null assertion during registration can be caused by a subject method
that has already returned, so its frame need not appear in the exception stack.
The native policy can use a narrowly checked author-runtime signature to request
fresh original/mutant/original controls. The signature alone never
establishes a kill. Both originals must pass the full expected catalogue; the
mutant replay must reproduce the initial error and stack after replacing only
the generated workspace path. Both raw streams, native cleanup, suite selection
and source/dependency/SDK bindings remain mandatory.

### Framework namespace isolation

When the selected package is also a test-framework dependency, author tests use
the subject alias while the framework keeps its unchanged library. This applies
to every mutation family. The header rewriter recognizes URI token ranges with
comments, metadata such as `@TestOn('vm')`, named or unnamed `library` directives,
import prefixes (`as`), `deferred`, `show`/`hide`, conditional import/export URI
branches and URI-bearing `part`/`part of` directives. It preserves those constructs
and replaces only a canonical `package:<selected-package>/` prefix inside a
supported URI literal. Comments and other header text are not reformatted.

Headers alone need no analyzer subprocess. Rust scans and validates all source
files before applying byte edits to the disposable copy. If author test code
contains the identifier `resolvePackageUri` or `resolvePackageUriSync`, the
controller requests one read-only namespace plan for that frozen project from
the native owner. The owner starts the Dart tool directly through the Rust Runner.
The Dart tool resolves SDK bindings without executing author code, running
`pub get` or changing dependency metadata. It accepts a direct call to the actual `dart:isolate.Isolate`
resolver whose argument is an actual `dart:core.Uri.parse` call with one canonical
simple string literal. Only an owning-package prefix is changed; canonical URI
literals for other packages are left alone. Prefixed SDK imports are supported,
and a same-named custom class or method cannot authorize an edit.

The internal plan binds the original package configuration, complete author-test
file inventory and each file's SHA-256. Each attempt rechecks these bindings and
the exact expected bytes before applying non-overlapping prefix edits to its
copy. Reversing those precise edits must reproduce every original byte and hash;
bytes outside the namespace ranges remain unchanged. Runtime URI edits are
recorded separately from header edits, with the planner receipt and raw captures
under `namespace-plan/`. The plan is produced internally, not accepted as a user
configuration or persisted as a source-hash-only cache.

The native owner verifies unchanged source inputs after the planner exits,
including on cancellation, and checks capture hashes against the Runner result.
Integrity failures take precedence over cancellation. `result.json` records the
actual Dart child command, the Runner summary and the bound plan hash;
`capture/stdout.bin` contains the tool's response. The private protocol uses the
campaign's workspace lease, so the copy remains protected until its native owner
finishes. A native namespace error never falls back to the Python implementation.

SDK resolver tear-offs, indirect/computed/interpolated URI arguments, escaped or
encoded URI spellings and unrecognized owning-package references are rejected
explicitly. The implementation does not rewrite arbitrary assertion/body
strings. Self-package URI references in the subject's `lib/` remain unsupported
when framework isolation is required: rewriting that library would change the
mutation's bound source and offsets. The selected package's `packageUri` must
resolve to the verified `lib/` tree. An entry pointing to another library tree is
rejected because that tree is not covered by the isolation proof.
A namespace preparation failure stops the
campaign before its test process launches; it is not a killed mutant or a successful
baseline, and a failed campaign has no score. Packages without a framework-name
collision do not require these alias rewrites.

By default a run creates `.failforge/runs/<run-id>/` in the selected project:

```text
manifest.json          effective configuration and original input hashes
input-bindings.json    dependency, adapter, SDK and executable bindings
events.jsonl           ordered stage/progress events and process intervals
progress.json          compact live status and processed/discovered counts
summary.json           initial empty report, then authoritative terminal report
discovery/             fresh adapter transcript and static-validation evidence
baseline/              original test results and exact author-test catalogue
original-after/        final original control
mutants/<id>/          patch metadata, result and raw captures
```

A multi-family profile has one common snapshot, input bindings, event journal
and report directory. Family-tagged events share one ordered sequence. Its layout
is:

```text
manifest.json          resolved project, explicit switches and enabled families
input-bindings.json    common frozen execution inputs
events.jsonl           shared stages and family-tagged progress
progress.json          compact live profile/family status and counts
summary.json           initial empty report, then terminal family summaries
families/
  classic/             discovery, baseline, mutants and original-after control
  failure_mode/        independent family results
  language_specific/   independent Dart results
```

Disabled families remain visible in the root summary. The profile has no combined
score: each family retains its own denominator and score. A failed or cancelled
profile clears final scores for all its families, including ones executed before
the failure. Later families are marked `not_started`; no passing score is claimed
for an incomplete profile. The terminal prints a family/status/mutants/score table
and counts for all eight outcomes.

The root and family `summary.json` paths are symbolic links through one private
commit pointer. Final reports become visible together only after all of them
have been written and synchronized. Preserve the hidden `.profile-reports/`
directory and `.profile-reports-current` link when copying the report directory,
or dereference the public `summary.json` paths when exporting individual files.
An interrupted publication leaves the public reports incomplete and unscored.
The journal's `campaign_finalizing` event records an intended status, not a
committed result; use the root `summary.json` to determine completion.

### Live progress and terminal reports

Both direct CLI campaigns and YAML profiles update `progress.json` as results
arrive or status changes. This compact file contains counts and status, without
the accumulated test catalogues, mutation records or scores. Counters include
only processed results; a discovered total can remain unknown until discovery
finishes. A profile also shows each selected family's progress. Read this file
for live monitoring, and use `events.jsonl` for the ordered event history.

`progress.json` always has `complete: false`. Its status is `running` or
`finalizing`; an `intendedStatus` during finalization describes the controller's
intention and does not prove that the final report was published. This remains
true even when every discovered mutant has a processed result. Input checks,
original controls, workspace cleanup or report publication can still fail.

A normal run writes the full `summary.json` once as an empty, incomplete initial
report, then once at terminal publication. A failure during discovery or baseline
setup can produce only an incomplete terminal summary. The full report is not a
live counter or a growing checkpoint. Each mutant's `mutants/<id>/result.json`
is retained independently when that result is produced, including its evidence
and actual test catalogue;
profile results use `families/<family>/mutants/<id>/result.json`. Live progress
does not replace those files or the journal. Avoid repeatedly loading the full
summary to monitor a large campaign.

A terminal report for a failed or gracefully cancelled campaign contains the
available partial results and remains incomplete and unscored. A profile clears
the scores of every selected family, including families that finished earlier.
The existing atomic profile publication rule still applies: root and family
reports must agree before a final successful profile is visible.

SIGKILL or a host crash can prevent terminal publication. In that case the
initial summary can remain visible while `progress.json`, per-mutant results
and the journal retain the last successfully written evidence. None of those
files alone establishes a completed campaign or a final mutation score. This
does not add automatic continuation of an interrupted campaign.

An explicit output directory must not already exist. Output inside the target
project must be below `.failforge/`, which is excluded from copies and source
bindings. Temporary copies are removed after each attempt. The CLI prints the
report path. Captures retain the Runner's private file permissions.

The CLI prints a final ASCII table to stdout, followed by the report path.
Progress messages go to stderr. For example:

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

`Mutants` shows processed/discovered counts; `?` means discovery did not establish
the expected total. All eight outcomes appear, including zero counts. `In score`
marks outcome eligibility. The score line is `Failure mode score` for that family.
An incomplete campaign displays `N/A (campaign incomplete)`; a complete campaign
with no killed or surviving mutants displays `N/A (no scored mutants)`. Wall time
is elapsed campaign time. The final `Report:` line points to the JSON evidence.

Only `KILLED` and `SURVIVED` enter the score denominator. `TIMEOUT`, `INVALID` and
inconclusive results are excluded explicitly. Completed inner test timeouts retain
`KILLED / test_timeout`; externally interrupted suites retain `TIMEOUT` even if
earlier inner errors were observed. Runtime load failures require fresh causal
A/B/A confirmation; a nonzero exit alone cannot kill a mutant. Classical results
use `mutationScore`; failure-mode results use `failureModeScore`. They are never
combined. Failed or cancelled campaigns have no final score.

Exit codes: `0` for a completed campaign (including survivors or bounded mutant
timeouts), `2` for invalid configuration, `125` for failed or inconclusive
execution, `130` for Ctrl+C and `143` for SIGTERM. Cancellation requests the active
Rust CLI owners to stop and waits for their cleanup; it does not kill them. The
adapter helper drains its own Runner after a closed consumer pipe. The retained
report is marked incomplete, with no final score.

## Recovery of abandoned working copies

Before allocating working copies, each valid `mutate` campaign asks Rust to scan FailForge's
private per-user registry, `<system-temporary-directory>/failforge-workspaces-<uid>`.
Snapshots and attempts are created inside a registered campaign directory. The
registry must be outside the target project, dependencies, SDK and controller
inputs; a `TMPDIR` inside those trees is rejected before creating the registry.
The controller and its native Rust cleanup owners hold an operating-system file lock
on that campaign. A new campaign removes a registered directory only after it can
acquire the lock, so killing the controller does not make a live Runner's working
copies eligible for deletion. This also permits recovery after a reboot once all
owners are gone, if the operating system has retained the temporary directory.
The native creator transfers already locked descriptors through a private Unix
socket, so there is no unlocked gap between native creation and controller
ownership. Cleanup requests still run after campaign cancellation. Closing a
controller reference never unlocks the references held by surviving owners.

Recovery validates directory ownership, private permissions, protocol metadata
and lock files. Deletion stays relative to opened directory descriptors and never
follows payload symlinks. Active and unrecognized entries are retained; unsafe
registry roots are rejected. Recovery does not search arbitrary temporary folders
by prefix or age, and does not delete working copies from older FailForge versions
that have no registry metadata. Failed directory removal is not reported as a
successful recovery. Payload trees deeper than 128 levels are retained with the
explicit reason `workspace_depth_limit`; they cannot prevent other campaigns
from starting. Control files are opened without blocking on special files.

The event journal records recovery and the newly allocated workspace directory;
the final summary includes `workspaceRecovery`, `workspaceDirectory` and
`workspaceCleanupDeferred`. Reports and captures remain in the configured output
directory and are not deleted by recovery. Restarting starts a fresh campaign;
recovery does not reuse mutation results or resume a previous campaign.

The lease protects the controller and its native cleanup owners. It is not a
system sandbox or a replacement for Runner process supervision. The private
lease descriptor is closed on exec into the tested program. Force-killing a Rust
cleanup owner itself bypasses that owner's cleanup protocol.

## Verify

```sh
PYTHONPATH=python:scripts python3 -B -m unittest discover -s python/tests -v
python3 -B -m unittest discover -s scripts -p 'test_*.py'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
# Requires the prepared tooling and Flutter example described above:
python3 scripts/mutate_cli_e2e.py --output /absolute/new-evidence-directory --flutter-sdk /absolute/flutter
python3 scripts/workspace_recovery_e2e.py --output /absolute/new-crash-evidence-directory
```

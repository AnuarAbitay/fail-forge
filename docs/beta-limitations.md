# Beta support and limitations

This document describes the current implementation, not a promise of support for
every Dart project. Use it with the [mutation command guide](mutation-cli.md).
Beta interfaces and report formats may change between releases.

## Platforms and toolchains

| Area | Current boundary |
| --- | --- |
| Host operating systems | Linux and macOS have process backends. Windows is unsupported. |
| Local verification | Recorded native and real Dart/Flutter regression runs were performed on macOS arm64. |
| CI | Linux/macOS jobs are configured. The current anonymous source download requires a public repository; it cannot fetch a private repository without an approved authentication change. Check actual workflow results for the release commit rather than treating configuration as a passed run. |
| Rust source builds | Rust 1.85 or newer is declared; the recorded minimum-version checks use 1.85.1. Build with the committed `Cargo.lock`. |
| Dart tooling SDK | The adapter requires `>=3.13.0 <3.14.0`; the locally exercised version is 3.13.5. |
| Flutter | The recorded SDK is Flutter 3.47.7 with Dart 3.13.5. Other Flutter/SDK combinations need their own validation. |

Support for Dart language constructs does not imply compatibility with every
Dart 3 SDK. Use a compatible SDK that can resolve both the adapter and the target
package; do not upgrade an application's SDK constraint merely to match an
example. The SDK and package dependencies are separate installation requirements.

## Installation is more than one binary

Install from a native archive or build from the source repository:

```sh
cargo build --workspace --locked
dart --suppress-analytics pub get --enforce-lockfile --directory adapters/dart
```

Cargo and the platform's native build tools are required only to build from source.
The ordinary build is optimized by the workspace configuration and places
executables in `target/debug`.

Mutation testing also requires the prepared `adapters/dart` package. Applicability
analysis for `profile generate` requires `failforge-adapter-campaign` beside the
`failforge` executable. Copying only `failforge` is insufficient. The Dart/Flutter
SDK is supplied separately; no Python interpreter is needed. A native archive
also excludes prepared Pub dependencies: run `pub get --enforce-lockfile` for
the adapter at its final location.

The [local packaging command](packaging.md) builds a host-specific `.tar.gz`
with both executables, adapter sources, user documentation and license material.
It does not publish a GitHub release. The initial local archive target is macOS
arm64; this is not evidence of successful Linux packaging or CI. Archives are
not universal executables, signed installers or notarized macOS applications.

If arranging a relocated installation, preserve these relationships:

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
      .dart_tool/package_config.json  # generated locally by pub get
```

This tree describes the runtime layout; it does not imply that a GitHub release
asset has been published. `installation/bin/failforge` finds the installation from its own real
location; the source layouts `<checkout>/target/debug/failforge` and
`<checkout>/target/release/failforge` find that checkout. Relocation does not
depend on the checkout used to build the binary.

An explicit `--installation PATH` takes priority. Relative values are resolved
from the invocation directory. Other layouts, including custom Cargo output
directories, need this option instead of a build-time path fallback. Keep
`failforge-adapter-campaign` beside the actual `failforge` executable even when
overriding the installation.

Supply `--dart-sdk` or `--flutter-sdk` for an SDK outside the installation's
default `.toolchain` location in direct CLI mode. With `mutate --config`, set
`dart_sdk` or `flutter_sdk` in the YAML profile instead. A separately prepared
adapter remains selectable with `--adapter-root` or YAML `adapter_root`.
Prepare adapter dependencies at their final location rather than shipping
another machine's generated package configuration. See
[relocation instructions](installation.md#relocate-an-installation) and the
[distribution license requirements](dependency-licenses.md#distribution-boundaries).

## Supported target projects and isolation

- A campaign accepts one prepared standalone Dart or Flutter package using
  `test` or `flutter_test`. Pub workspaces are outside the supported scope.
- The target must already contain `pubspec.yaml`,
  `.dart_tool/package_config.json` and `.dart_tool/package_graph.json`, with
  existing dependency roots. Prepare its dependencies yourself before running
  FailForge. Campaigns do not run `pub get`, fetch projects or update dependencies.
- Source symlinks are rejected. Copied source and retained tool state must use
  regular files and directories; linked configuration and special files are
  rejected rather than followed.
- Fresh copies preserve source bytes, ordinary permission bits and modification
  times, with deliberate package-configuration rebasing. Extended attributes and
  ACLs are outside the copy contract.
- Keep the project, SDK, adapter and dependencies unchanged during a run.
  Integrity checks detect changes and can invalidate the campaign.

Every executed mutant gets a fresh working copy. Recognized abandoned workspaces
can be cleaned on a later run after a crash. There is no persistent resume from
the last completed mutant. `replay` can rerun a verified retained **classic**
transcript in fresh copies; it does not resume an interrupted campaign.

Isolation protects the original from FailForge's own mutation patches. It is
**not a security sandbox**: author tests run with the executing user's filesystem,
network and environment access. Run only projects whose code you trust, or provide
your own OS/container isolation.

## Selection and execution limits

- Direct `mutate` arguments select one family. YAML can select `classic`,
  `failure_mode` and `language_specific` together. Dart is the only implemented
  `language_specific` subtype, and its language must be explicit.
- One YAML file is one profile. Profiles live inside the target package and find
  it through the nearest ancestor `pubspec.yaml`; there are no `version` or
  `project` YAML fields, named-profile collections or legacy format support.
  Omitted families/operators are disabled. Unknown settings remain errors even
  inside a disabled section.
- `profile generate` targets one existing file under `lib/`. Its default analysis
  enables operators with at least one statically valid candidate; it does not
  execute author tests or prove that a mutant changes observable behavior.
  `--template` skips this applicability analysis.
- Mutation scope selects library code. Class/method filters select mutation
  locations, not a reduced test suite. Generated files and test/dependency sources
  are excluded from mutation discovery. Operators have explicit supported AST/type
  subsets; see [classical applicability](mutation-operators.md#classical-applicability) and
  [Dart semantic catalogue](dart-semantic-mutations.md).
- The default is one worker; the supported range is **1–4**. Families execute in
  order, and workers parallelize mutants within the current family. More workers
  increase concurrent test processes and working-copy resource use.
- The default candidate cap is **1,000**, configurable from **0 to 10,000**.
  All selected families are discovered before author tests, and their combined
  count includes statically invalid candidates. Exceeding the cap fails preparation;
  it does not sample or silently truncate the run.
- Discovery has bounded input/output sizes, including a **4 MiB** source-file
  limit and a **64 MiB** adapter transcript limit. Very large projects or verbose
  executions can exceed the relevant evidence limits.

Static validation cannot eliminate every equivalent mutant. A surviving mutant
can expose a missing assertion, an unexecuted location or a behaviorally equivalent
change. Inspect the patch and evidence before treating it as a test defect.

## Timeouts and result interpretation

The default inner test limit is **30 seconds**, and the outer command limit is
**120 seconds**. The command limit must exceed the inner limit by at least five
seconds. It covers loading, the full suite and teardown; five seconds of headroom
does not guarantee that a large suite or multiple sequential test timeouts fit.
Per-test timeout metadata and disabled inner timeouts can also affect behavior.

| Outcome | Interpretation |
| --- | --- |
| `KILLED` | Verified test evidence demonstrates a failure. A completed inner test timeout can qualify as `KILLED` with reason `test_timeout`. |
| `SURVIVED` | The mutant completed the required test catalogue without an established failure. |
| `INVALID` | Static validation or the test compiler rejected the mutant; it is excluded from the score. |
| `TIMEOUT` | The outer Runner interrupted execution. Partial counters do not turn it into a kill. |
| `INFRA_ERROR` | Execution, input integrity, capture or cleanup failed. This is not evidence that the tests killed the mutant. |
| `UNCLASSIFIED` | Available evidence does not justify a kill or survival, for example an unconfirmed loading failure. |

An outer timeout's exact cause is not established from the capture alone: slow
loading, a loop, disabled inner limits and teardown can look similar. If an
unmodified control times out, adjust the budgets and rerun into a new output
directory. Invalid working-copy package configuration is checked before launch
and reported as an infrastructure error, rather than inferred from exception text.

Each family has its own score:

```text
KILLED / (KILLED + SURVIVED)
```

`INVALID`, `TIMEOUT`, `INFRA_ERROR` and `UNCLASSIFIED` are excluded. There is no
combined score across families and no numeric score when the denominator is zero.
Cancelled, failed or otherwise incomplete campaigns have no final score; an
incomplete profile suppresses final scores even for a family that finished.
Inspect outcome counts alongside the percentage, since excluded results reduce
what that percentage says about the selected scope.

Final `summary.json` is authoritative. Completed evidence is retained where
possible after cancellation or publication failure, but it is not a successful
complete campaign. Atomic publication does not guarantee durability of the final
rename after sudden power loss.

## What the existing checks establish

The repository contains unit, process, real-SDK and public-project regression
checks. Recorded local results and their scope are in
[known issues and verification history in the source repository](https://github.com/AnuarAbitay/fail-forge/blob/main/docs/known-issues.md). That document also
records historical defects that have been fixed; they should not be read as a
list of currently open bugs.

The recorded public-project matrix covers selected mutation scopes. It is not an
exhaustive run of every archived candidate, every classic operator on every
project, every filesystem race or every signal timing. A release should identify
its exact commit, executed platforms/toolchains and test results separately from
checks that merely exist in the repository.

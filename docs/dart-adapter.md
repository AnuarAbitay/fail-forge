# Dart / Flutter tooling adapter

The adapter is a development package under `adapters/dart`; application code never
imports it. Rust launches its JSON Lines process through `failforge-runner`.
The adapter reads an explicitly selected package, resolves ASTs and symbols,
returns patches, and validates patches in an in-memory analyzer overlay. It does
not run tests, run `pub get`, execute JSON-supplied commands, or write project
sources/configuration. Rust performs exact applicability checks before applying a
patch to an isolated copy.

## Toolchain and preparation

The recorded environment is Flutter **3.47.7**, Dart **3.13.5**, analyzer **14.5.0**,
macOS arm64, Rust stable **1.99.0** and verification MSRV **1.85.1**. The adapter's
SDK constraint is `>=3.13.0 <3.14.0`; only 3.13.5 has been executed locally. Older
package language versions are determined by the selected package configuration
and per-file language directives. Other SDK combinations need their own checks.

Use your installed compatible tooling SDK; do not upgrade a target application's
SDK to match this example. The local `.toolchain/flutter` installation is ignored
by Git. The SDK came from the [official archive](https://docs.flutter.dev/install/archive),
with archive SHA256 verified. Analyzer's public API and version constraints are
published in its [package](https://pub.dev/packages/analyzer/versions/14.5.0).
There are no direct imports of `package:analyzer/src/...`.

```sh
# Prepare the adapter's own dependencies, separately from the target package.
dart --suppress-analytics pub get --enforce-lockfile --directory adapters/dart
# Explicitly prepare only the example; the adapter never does this operation.
(cd examples/flutter_fixture && flutter --suppress-analytics pub get --enforce-lockfile)
cargo build --workspace --examples --locked
```

A target must already have a readable `pubspec.yaml` and valid version-2
`.dart_tool/package_config.json` (in the package or an ancestor inside the supplied
workspace). Dependency package roots must exist. Missing SDKs/config/dependencies
return errors. Generated code can still be read for type resolution. The adapter
never changes `pubspec.yaml`, lock files or the package configuration.

The example below launches a real persistent session through the existing Runner,
performs handshake/discovery/validation and checks a patch in Rust without writing
it. Supply absolute paths; `packageRoot` is relative to `workspaceRoot`.

```sh
cargo run -p failforge-dart --example session -- \
  /absolute/flutter/bin/cache/dart-sdk/bin/dart \
  /absolute/FailForge/adapters/dart \
  /absolute/FailForge examples/flutter_fixture \
  /absolute/flutter/bin/cache/dart-sdk /absolute/flutter
```

Optional final arguments are a normalized rules JSON file and a zero-based index
of one discovered mutant. They belong to the Rust example, not the adapter's
command protocol. Direct VM launch uses the adapter's explicit package config,
so startup does not automatically resolve dependencies or mix pub output into
protocol stdout.

## Wire protocol

The machine-readable contract is
[`schema.json`](../adapters/dart/lib/src/protocol/schema.json), with a shared
Unicode/BOM/CRLF patch fixture in [`protocol/fixtures`](../protocol/fixtures).
Each input/output record is one UTF-8 JSON line. Requests are sequential:

```json
{"protocolVersion":1,"requestId":"1","command":"handshake","payload":{}}
```

Successful responses contain `protocolVersion`, the same `requestId`, `kind`
(`handshake`, `discover`, or `validate`) and `payload`. Failures have `kind:error`
and `error:{code,message,details?}`. Unknown versions/commands/fields are rejected. Request IDs are nonempty, contain
no NUL and are bounded to 256 UTF-8 bytes; component/symbol names to 512 bytes.
Stdout contains protocol records only. Empty EOF exits cleanly; a final complete
JSON record without LF is accepted. Input/output records are bounded to 8 MiB of
UTF-8 bytes excluding LF. Oversized responses return `response_too_large`, never
a truncated successful result. Reduce the selected scope or `maxMutants` and retry.

A discover payload:

```json
{
  "workspaceRoot": "/absolute/snapshot",
  "packageRoot": "my_package",
  "dartSdkPath": "/absolute/flutter/bin/cache/dart-sdk",
  "flutterSdkPath": "/absolute/flutter",
  "rules": {
    "includePaths": ["lib/**.dart"],
    "excludePaths": ["lib/generated/**"],
    "components": [{
      "name": "platform-channel",
      "classes": ["Gateway"],
      "methods": ["read"],
      "operators": ["BOOLEAN_LITERAL_FLIP", "CONDITIONAL_NEGATION"]
    }]
  },
  "limits": {"maxMutants": 1000}
}
```

`files`, if supplied, is an array of package-relative Dart targets. Otherwise the
adapter enumerates `lib/**/*.dart`. A validate payload has the same project/rules
fields plus `mutant` from discovery. Validation checks canonical ID/metadata,
exact applicability and that the edit really corresponds to a supported AST
operator in the selected scope; a self-consistent forged arbitrary patch is not
accepted. `valid` means static analysis passed, **not** that compilation/tests have
run: `compileChecked` remains false.

Analysis contexts and original library results are reused within a request.
Validation resolves the whole containing library, including parts, changes only
the overlay, and restores/invalidate its original content in `finally`. Baseline
analysis errors are reported separately and block discovery/validation. Warnings
and informational diagnostics are not themselves compiler errors. A request gets
fresh contexts, so SDK, package configuration, analysis options and dependencies
cannot be hidden by a persistent source-hash-only cache. This deliberately spends
context setup time on each request; performance evidence includes that cost.

## Scope semantics

Within path/class/method lists, matches are OR; between categories, AND. An absent
category is unrestricted; an explicit empty list matches nothing. An explicit
empty `operators` list enables no operators. Global exclusions take priority.
Matched components union their operator sets, subject to any explicitly supplied
global operator restriction. Metadata includes **all** matched component names,
even when another matching component enables that operator. Names are arbitrary
labels. A top-level function does not match a class filter. Nested closures inherit
the nearest named enclosing function/method scope. Class filters currently refer
to `class` declarations; mixin/enum/extension names are not mapped to class names.

Without components, unspecified operators enable the 31 default classical operators.
With components, at least one matching component must enable the operator. An
unspecified component operator set inherits global operator selection, whose
default is classic operators. Explicit empty sets enable nothing. Empty components
behave like no component restriction. There is no guessed component-to-test map.

Default exclusions include `test`, `integration_test`, `.dart_tool`, `build`,
`.git`, `.pub-cache`, `*.g.dart`, `*.freezed.dart` and the conventional generated
header. `generatedPatterns`/`generatedHeaders` extend the defaults. Paths use `/` and package-relative globs. The test/integration_test/build
directory defaults are anchored at the package root; metadata/cache directories
are excluded at any depth. All mutation-path symlinks are rejected, including
internal aliases. SDK/pub-cache/dependency files cannot become mutation targets.
Absolute service roots for the workspace and SDK are separate from mutation paths.

`maxMutants` (0–10,000, default 1,000) is applied after filtering, patch deduplication
and deterministic sorting. Discovery reports `truncated`, `totalCandidates`,
`returnedCandidates`, and `maxMutants`. Hard safety budgets produce explicit
errors: 2,000 targets, 4 MiB per source, 64 MiB aggregate source, 100,000 enumerated
entries, 50,000 candidate edits and 16 Mi UTF-16 units of aggregate edit content.
Operator scans also bound their candidates, diagnostics and replacement content.
These bounds do not claim to sandbox analyzer resource consumption on hostile
source/dependency graphs.

## Operators and patch contract

The hierarchy is **family → category → operator → concrete mutant**:

```text
Mutation Operators
├── Classical (wire family: classic)
│   ├── Relational
│   ├── Logical
│   ├── Arithmetic
│   ├── Boolean
│   ├── Constants
│   ├── Return
│   ├── ControlFlow
│   ├── Calls
│   ├── Variables
│   ├── Bitwise
│   ├── Exceptions
│   └── Collections
└── FailureMode (wire family: failure_mode)
    ├── EXCEPTION_AT_ENTRY
    └── METHOD_CHANNEL_ERROR
        ├── PlatformException candidate
        └── MissingPluginException candidate
```

The 31 default classical operators implement the supplied practical catalogue.
`CONDITIONAL_NEGATION` remains a separately advertised legacy compatibility
operator, available through explicit selection; `BOOLEAN_NEGATION` covers the
broader add/remove-negation transformation. Existing operator IDs are preserved.

| Category | Operator IDs | Transformations |
| --- | --- | --- |
| Relational | `RELATIONAL_BOUNDARY`, `COMPARISON_NEGATION`, `COMPARISON_OPERATOR_REPLACEMENT` | Boundary changes, inverse comparisons, other type-compatible comparison operators |
| Logical | `LOGICAL_CONNECTOR_SWAP`, `LOGICAL_OPERAND_REMOVAL` | `&&` / `||` replacement; select either operand |
| Arithmetic | `ARITHMETIC_OPERATOR_REPLACEMENT`, `ARITHMETIC_OPERAND_REMOVAL`, `NUMERIC_NEGATION`, `INCREMENT_DECREMENT_REPLACEMENT`, `COMPOUND_ASSIGNMENT_REPLACEMENT` | Arithmetic replacement/removal, sign changes, prefix/postfix increment/decrement, compound assignments |
| Boolean | `BOOLEAN_LITERAL_FLIP`, `BOOLEAN_NEGATION` | Boolean literal replacement; add/remove negation |
| Constants | `NUMERIC_LITERAL_REPLACEMENT`, `STRING_LITERAL_REPLACEMENT` | Computed numeric values `0`, `1`, `-1`, `c+1`, `c-1`, `-c`; empty/nonempty simple strings |
| Return | `RETURN_VALUE_REPLACEMENT` | Typed primitive/null/empty-collection return values, including arrow and async bodies |
| ControlFlow | `CONDITION_FORCING`, `BRANCH_BODY_REMOVAL`, `CONDITIONAL_EXPRESSION_MUTATION`, `STATEMENT_DELETION`, `BREAK_CONTINUE_MUTATION`, `SWITCH_CASE_MUTATION` | Force conditions; remove branch/statement/case contents; select/swap ternary branches; change loop control and constant cases |
| Calls | `FUNCTION_CALL_REMOVAL`, `ARGUMENT_REPLACEMENT`, `ARGUMENT_PERMUTATION` | Remove function/method calls with typed result defaults; replace or exchange compatible arguments |
| Variables | `VARIABLE_REPLACEMENT` | Replace reads with visible, initialized, type-compatible parameters/locals |
| Bitwise | `BITWISE_OPERATOR_REPLACEMENT`, `BITWISE_NEGATION`, `SHIFT_OPERATOR_REPLACEMENT` | Bitwise replacement/negation and supported shift replacements |
| Exceptions | `THROW_REMOVAL`, `EXCEPTION_REPLACEMENT` | Remove throw statements; replace resolved SDK StateError/ArgumentError/Exception or empty their messages |
| Collections | `COLLECTION_LITERAL_MUTATION` | Empty a List/Set/Map literal or remove individual elements/entries |

`MutationFamily` identifies the broad kind. `ClassicalCategory` identifies the
category of an operator. The registry exposes `operatorsByFamily` and
`operatorsByClassicalCategory`; every classic implementation explicitly declares
its category. Implementations live in matching `operators/classic/` directories.
The handshake retains its flat `operators` list, adds category/legacy metadata,
and exposes `operatorTree`. Categories are derived from the operator catalogue,
not trusted as extra mutant fields. Existing mutant JSON and canonical ID format
remain unchanged; protocol version is still 1. Rule selection uses operator IDs.

Each variant produces a separate candidate. Discovery excludes unchanged variants
and merges candidates producing identical final bytes within the same family,
even if their AST edit ranges differ. Merging precedes `maxMutants`, keeps a
deterministic representative (original operator IDs have preference) and unions
component names. `rawCandidates` and `deduplicatedCandidates` expose this step.
No attempt is made to prove arbitrary semantic equivalence between programs.
Deduplication preflights a 256 MiB aggregate hashing-work limit before hashing
changed files. The handshake advertises `maxDedupWorkBytes`; discovery metrics
include `dedupHashedBytes`. A larger request fails with
`candidate_work_limit_exceeded` and budget details; narrowing file/operator
selection keeps the request within the bound. Disabled and file-excluded
operators are filtered before scans and consume no candidate/diagnostic budget.

The fixture report retains chronological `records` and separately exposes
`resultsByFamily`. Those groups contain mutation results; baseline and diagnostic
control runs are kept outside them. There is no combined score for the families.

Operators use resolved AST nodes, semantic SDK bindings and types. They exclude
metadata, defaults, nonexecutable initializers and constant contexts; switch
constant replacement is the deliberate exception for a constant-case target.
Pattern conditions/guards and unsupported switch expressions are diagnosed.
Arithmetic/bitwise operators exclude user overloads and unresolved/dynamic
operations. Expression rewrites preserve AST precedence, and compound assignment
keeps its lvalue evaluation semantics. No whole-file formatting is performed.
Type/promotion/control-flow errors in an otherwise authentic candidate produce
`INVALID` through whole-library static validation, never `KILLED`.

The catalogue has explicit supported subsets. Variable substitution supports
initialized non-late locals and parameters of the nearest function, respecting
shadowing and declaration order; fields/getters, captures and loop/catch/pattern
bindings are excluded. Argument replacement uses the instantiated declared
parameter type, preserves named labels, and permutations exchange compatible
positional arguments with effect-free reads/literals. Call removal excludes
cascades, unresolved calls and effectful receiver/argument evaluation. String
replacement excludes interpolation. Return defaults support SDK primitives,
nullable values and nameable generic List/Set/Map types; unsupported types are
skipped. Collection spread/if/for elements, labelled control transfers, guarded
cases and switch expressions are excluded. Exception replacement adds no imports
and skips shadowed or unavailable SDK names. Diagnostics describe supported-boundary exclusions; unrelated AST nodes are
not reported as mutation sites.

Failure-mode operators are disabled by default. Each needs explicit operator
selection **and** a positive path/class/method selection in the global rule or an enabled matching
component. Operator and selector may be composed across these two levels. They add no imports. Unavailable, ambiguous or shadowed SDK
exception names cause diagnosed skips. Deferred-only exception imports are
unsupported; inherited part-library prefixes are recognized. Constructors, external/abstract bodies and
generators are unsupported for entry injection. Async entry remains async.
Channel injection is limited to a directly awaited (optional parentheses),
semantically resolved real Flutter MethodChannel call. Its statically nonnullable
receiver and arguments must be parameter/non-late local reads or primitive
literals. Calls, getters, interpolation, collection literals and subclasses are
skipped so their evaluation effects are not silently discarded. OptionalMethodChannel,
EventChannel, Pigeon and user classes with similar names are unsupported. A
surviving failure-mode mutant can mean that the application handles the error or
that the location was not executed; it is not mixed with a classic mutation score.

A mutant includes operator/version/family, package-relative `file`, exact raw-source
SHA256, all matched components, one edit, enclosing symbol metadata and validation
state. An edit has `offsetBytes`, `lengthBytes`, `expected`, `replacement`.
Offsets are **UTF-8 bytes from the original file start**, including any BOM;
expected/replacement are valid Unicode strings. A one-pass index converts UTF-16
analyzer positions to bytes, rejecting split surrogates. Analyzer removes one
leading BOM from its source view; the conversion restores the three-byte prefix.
Untouched Unicode, LF/CRLF and BOM bytes are preserved.

Stable ID = lowercase SHA256 of compact UTF-8 JSON encoding of this exact array:

```
[1, file, sourceHash, operatorId, operatorVersion, family,
 offsetBytes, lengthBytes, expected, replacement]
```

Absolute roots, timestamps, traversal order, worker number, components and
validation state do not enter the ID. Patches are sorted by file, offset, length,
operator ID, replacement. Identical resulting source bytes are deduplicated within each family and their
component names unioned. Rust checks canonical paths, SHA256, checked bounds,
UTF-8 boundaries and exact expected bytes; it never fuzzy-matches. Its file reader
uses directory descriptors and `openat/O_NOFOLLOW`, returns bytes, and never writes
the original. The owning core must provide a consistent, immutable snapshot;
these checks do not turn arbitrary concurrent file writes into a transaction.

## Integration boundary and verification

Current Rust Core contains the Runner and run CLI. The new `failforge-dart` crate
adds protocol models, persistent `AdapterClient`, safe in-memory patching and a
small process-plus-test-evidence classification extension point. It reuses the
same Runner; only optional caller-owned stdin and observable stream-loss counters
were added to the Runner. It does not add another process supervisor.

A production mutation engine, YAML/profiles, snapshot/workspace manager, mutation
scheduler, complete machine-event ingestion and score/report orchestration are
**not implemented**. `session` is a Rust example, not a new mutation CLI command.
`scripts/dart_adapter_e2e.py` is an executable fixture harness demonstrating
snapshot/baseline/discover/validate/copy/patch/test/cleanup with explicit test files.
Its Flutter-3.47.7 reporter extraction distinguishes assertion/test failures,
compile failures, missing-input infrastructure failures, timeouts and no tests.
Nonzero status alone does not establish KILLED. Production callers must supply
validated `TestEvidence`; the Rust extension does not assign mutation scores.

```sh
(cd adapters/dart && dart --suppress-analytics analyze && dart --suppress-analytics test)
cargo test --workspace --all-targets --locked -- --test-threads=1
python3 -m unittest discover -s scripts -p 'test_*.py'
python3 scripts/dart_adapter_e2e.py --flutter-sdk /absolute/flutter \
  --output /absolute/new-evidence-directory --p1
```

Use a fresh evidence directory each time; raw captures are created exclusively.
The harness creates and removes its temporary copies, records commands and hashes,
and asserts original source bytes remain identical. Discovery and validation
measurements are separate from Flutter test durations. Record the executed
commands, toolchain versions, results, and remaining platform limits alongside
the generated evidence.

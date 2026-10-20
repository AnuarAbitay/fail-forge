# Dart language-specific mutations

These operators live below `operators/language_specific/dart/`, independently of
the `classic` and `failure_mode` families. The catalogue hierarchy is
`language_specific -> dart -> category -> operator`. A campaign selects one family
and one language; this implementation supports the Dart subtype.

## Integration and compatibility

The Dart adapter resolves ASTs and types using the pinned analyzer. Rust owns
configuration, discovery orchestration, exact byte patching, fresh attempt
copies, test processes, evidence classification and report publication. Semantic
operators use the same native scheduler and reporting path as the other families;
there is no Python controller or runtime dependency.

The candidate wire format remains protocol version 1: `operatorId`,
`operatorVersion`, `family`, `file`, `sourceHash`, `edit`, `symbol`, `components`
and `validation`. The canonical SHA-256 ID still binds the original source hash,
operator, family and UTF-8 byte edit. Existing classical IDs are unchanged.
The specification's `source_range` is represented by `edit.offsetBytes` and
`edit.lengthBytes`; `original` is `edit.expected`; pending candidates use the
existing `unvalidated` state. Language and category are catalogue metadata, not
additional unrecognized fields in a mutant DTO. Old clients must be upgraded
alongside the adapter before selecting the new family.

The existing source snapshot is reused for discovery. Original files are never
patched by discovery or validation. Execution materializes one patch in a fresh
attempt copy; removing that copy completes the rollback. No reusable workspace
or undo/redo backend is introduced.

The existing headless analyzer API supplies resolved units and an in-memory
overlay. An analysis-server plugin would introduce another deployment protocol
without supplying a needed capability here: [analyzer plugins](https://dart.dev/tools/analyzer-plugins)
are intended to extend editor/analysis-server diagnostics. No analyzer internals
are imported by these operators.

## Catalogue

| ID | Category | Transformation |
|---|---|---|
| NSM-001 | null_safety | Remove the fallback of `value ?? fallback` where the resulting program remains valid. |
| NSM-002 | null_safety | Replace null-aware access with direct access; never synthesize `!`. |
| NSM-003 | null_safety | Replace a value in a nullable variable/assignment context with `null`. |
| NSM-004 | null_safety | Replace `??=` with ordinary assignment when applicable. |
| NSM-005 | null_safety | Remove one null-aware spread element. |
| DPM-001 | parameters | Replace a supported scalar default with a compatible constant. |
| DPM-002 | parameters | Invert a boolean parameter default. |
| DPM-003 | parameters | Replace an explicit nullable parameter default with `null`. |
| SWM-001 | switch | Replace a switch statement branch result with another compatible branch result. |
| SWM-002 | switch | Replace a switch expression branch result with another compatible branch result. |
| SWM-003 | switch | Invert a switch guard. |
| PMM-001 | patterns | Change a relational pattern constraint. |
| PMM-002 | patterns | Invert an if-case pattern guard. |
| PMM-003 | patterns | Exchange logical pattern connectors where bindings remain valid. |
| CLM-001 | collections | Invert a collection-if condition. |
| CLM-002 | collections | Remove one ordinary spread element. |
| CLM-003 | collections | Remove one collection-if element. |
| CLM-004 | collections | Remove one collection-for element. |
| ENM-001 | enums | Replace a resolved enum value with a compatible value of that enum. |

Operators use dedicated AST applicability checks. They do not relax the
classical operators' restrictions on defaults, constants, patterns or guards.
Unsupported or unsafe contexts are skipped. Declaring an operator does not mean
every Dart construct of its general category is a supported mutation site.

The current applicability limits are:

- **Null safety.** NSM-001 requires a potentially nullable left operand; removing
  a redundant fallback from a non-nullable operand is skipped. The changed
  assignment, return or enclosing expression must still pass overlay validation.
  NSM-002 currently supports only `dynamic` receivers for property, method and
  index access. Typed nullable receivers, non-nullable receivers and cascades are
  excluded; the operator never inserts `!` to make them valid. NSM-003 targets
  variable initializers and the right side of ordinary `=` assignments whose
  resolved storage type accepts `null`, including `dynamic`; it does not replace
  arbitrary nullable expressions. Values already known to be null are omitted.
  NSM-004 changes only the `??=` token to `=` when the assigned value is compatible
  with the resolved write type. NSM-005 follows the collection-element limits
  below and is separate from ordinary spread removal.

- **Parameters.** DPM operators require an explicit, resolved compile-time
  constant default on a named or optional positional parameter. They do not add
  defaults to required parameters or replace an implicit null default. DPM-001
  offers `0`, `1`, `-1`, their double forms, `''` and `'mutated'`, retaining only
  alternatives assignable to the parameter type and omitting known equal values.
  Boolean inversion belongs to DPM-002; it also handles boolean constant aliases.
  DPM-003 requires a non-null explicit default and a parameter type that accepts
  `null`. No arbitrary constructor or collection default is synthesized.

- **Switches.** SWM-001 handles unlabelled statement arms containing exactly one
  direct `return expression;`. Arms with extra statements, blocks, labels or bare
  returns are not donors. SWM-002 exchanges result expressions within the same
  non-constant switch expression. Both operators require compatible resolved
  result types; `dynamic`, `void`, `Never` and unresolved result types are
  excluded. Donors containing closures, branch-local bindings or branch-specific
  promotions are skipped. Outer parameters and locals may be copied when used
  at their declared type. Each switch is limited to 128 result expressions.
  SWM-003 inverts resolved non-nullable `bool` guards on switch cases. Known
  boolean constants/aliases and guards without a runtime variable read or
  invocation are skipped conservatively. Remaining guards are checked for
  promotion errors and analyzer reachability diagnostics after inversion; this
  does not prove that an arbitrary runtime predicate can take both values.

- **Patterns.** PMM-001 complements the six relational tokens only when their
  operators resolve to supported SDK scalar/Object operations; custom overloads
  and arbitrary pattern rewrites are excluded. PMM-002 owns non-nullable `bool`
  guards on if-case statements and collection if-case elements, while switch
  guards belong to SWM-003. It leaves the pattern declarations unchanged and
  applies the same conservative constant/runtime guard restrictions.
  PMM-003 exchanges `&&` and `||` only for resolved patterns without variable
  binding/assignment patterns. This deliberately excludes even some Dart-valid
  binding combinations. Wildcards and binding-free structural patterns remain
  eligible; ordinary boolean expressions are outside this operator.

- **Collections.** CLM-001 inverts ordinary non-nullable `bool` collection-if
  conditions, not if-case patterns. A condition whose inversion loses a required
  promotion is rejected by the publication overlay. CLM-002 removes ordinary
  `...` spreads; NSM-005 removes only `...?` spreads. CLM-003 removes the complete
  if/else or if-case element, and CLM-004 removes the complete collection-for
  element including its body. Deletions require a direct child of a list, set or
  map literal. Literals nested inside other elements are supported, but deleting
  the sole body element of an enclosing if/for without its own literal is
  skipped. Removal retains surrounding comments and handles the adjacent comma.
  Untyped set/map literals receive their original inferred type arguments so an
  emptied set does not become a map. If those types cannot be named without
  shadowing or losing their binding, the candidate is skipped. Constant literals
  and nameable generic/nullable element types are supported.

- **Enums.** ENM-001 changes a resolved enum-constant use to another accessible
  constant of that same enum. It preserves import prefixes, qualifiers, dot
  shorthand and comments by replacing only the constant name. Constant value use
  sites are supported; enum-constant declarations, annotations and patterns are
  excluded, and enum definitions/order are never rewritten. Unqualified donors
  shadowed by a local declaration are skipped. For enhanced generic enums the
  donor must be assignable to the original value's resolved static type, which is
  deliberately stricter than merely sharing the enum declaration. `Enum.values`,
  unrelated static constants and arbitrary enum-valued expressions are not
  replacement sites.

The analyzer's original type and binding information is checked again against
the changed library in an overlay before a language-specific candidate is
published. New compile errors or unreachable-branch diagnostics reject that
candidate. Pre-existing warnings are not globally promoted to errors. The normal
authenticated `validate` request repeats validation before Rust accepts a patch.
Static validation is not a claim that a compiler or author tests have run.

Logical pattern changes must preserve Dart's binding rules; these differ between
logical-and and logical-or patterns. Guards can also affect type promotion and
branch reachability. See [Dart pattern types](https://dart.dev/language/pattern-types)
and [guard clauses](https://dart.dev/language/branches#guard-clause).

### NSM-001 equivalent comparison filter

NSM-001 omits a narrowly proven equivalent removal when the coalescing expression
is the left operand of `==` or `!=`, its receiver has static type `dynamic` or
SDK `bool?`, and the fallback and comparison value are opposite, pure boolean
constants. Parentheses do not change this rule. For example,
`(value ?? false) == true` and `value == true` have the same result: a null
receiver compares false in both, while a non-null receiver keeps its evaluation
and equality dispatch. The corresponding inequality and opposite boolean pair
are also covered. Discovery records `equivalent_null_coalescing_comparison`.

Recognized constants include boolean literals, resolved const aliases and static
const fields with inert type or non-deferred import qualifiers. Calls, getters,
non-const values and deferred accesses are excluded from this proof because
evaluation can be observable. Reversed comparisons, equal fallback/comparison
values, non-boolean comparisons, extension types, other static receiver types
and type parameters are not filtered by this rule. This is not a general
equivalence detector; an unfiltered survivor is not classified as equivalent.

## Configuration

```sh
failforge mutate --project /path/to/package --family language_specific --language dart
failforge mutate --project /path/to/package --family language_specific --language dart --operator NSM-001
```

In direct CLI mode, `dart` is the default subtype for this family. Other languages are rejected.
The family must be selected explicitly; a classical campaign does not silently
enable semantic mutations. Direct mode supports the existing file, class and
method filters and worker/time limits.

```yaml
workers: 2
mutations:
  language_specific:
    enabled: true
    language: dart
    operators:
      NSM-001: true
      NSM-004: true
      DPM-001: false
rules:
  include: ['lib/network/**']
  exclude: ['**/*.g.dart', '**/*.freezed.dart']
  methods: [load]
timeouts: {test: 30s, command: 120s}
```

The profile's nearest ancestor `pubspec.yaml` identifies the project. YAML has no
`version` or `project` field. `language: dart` is mandatory inside a present
`language_specific` section, including disabled sections. Only individually
enabled operators run; omitted operators and families are off. The full
[Dart profile](../examples/flutter_fixture/profiles/failforge.dart-semantic.yml)
lists all 19 switches, grouped with comments. A profile can also select classical
and failure-mode operators; their results stay separate. See the
[profile rules](mutation-cli.md#explicit-yaml-configuration).

Generated sources retain the adapter's normal exclusions. Unknown settings,
unsupported or wrong-family IDs and non-boolean switches are rejected even when
disabled. A profile with no active operators is an error. The adapter protocol's
existing category/component rules remain internal; YAML profiles use explicit
operator switches.

## Results, limits and performance

The family uses `languageSpecificScore`, separately from `mutationScore` and
`failureModeScore`. Only `KILLED` and `SURVIVED` contribute. `INVALID`, `TIMEOUT`
and inconclusive outcomes are excluded; a failed or cancelled campaign has no
final score. Runtime exceptions from author tests retain the established
evidence-based classification; they are not automatically engine errors.

The canonical infrastructure outcome remains `INFRA_ERROR`. The specification's
generic `ERROR` does not introduce a second conflicting outcome. `NOT_COVERED`
is not emitted without coverage evidence: neither a survivor nor `NO_TESTS`
establishes that a mutated expression was unexecuted. All original author tests
are selected, using the baseline catalogue checks.

Tooling SDK support remains the pinned Dart `>=3.13.0 <3.14.0`, locally checked
with Dart 3.13.5. Supporting Dart 3 syntax does not establish compatibility with
every Dart 3 SDK. Pub workspaces remain outside the campaign's supported scope.

Discovery reuses resolved original libraries across operators and materializes
no filesystem copy per candidate. Exact duplicate resulting bytes are removed
within the family before semantic overlay checks. Identical mutations from
different operators can therefore share one representative in a combined run.
Each emitted mutant still contains a single edit; conflicting edits are never
applied together. Parallel execution uses independently isolated attempts.

Existing candidate, response, memory and deduplication budgets remain enforced.
At most 10,000 semantic candidates are overlay-checked per discovery request;
exceeding that bound fails explicitly and requires narrowing the scope.

Persistent candidate caching is not enabled. A future semantic cache must bind
source bytes, normalized selection, operator versions, language versions,
analysis options, dependency configuration/content and SDK/analyzer identity.
A source-hash-only cache would miss changes that alter types or resolution.

## Verification

The Rust developer tool exercises the complete 19-operator fixture catalogue.
It checks handshake metadata, discovery transcripts and validated source-bound
candidates, applies exact patches, then runs independent Dart VM assertions.
Each operator must have a passing original and at least one observable mutant.
The fixture labels `UNCHANGED` and `OBSERVED` describe those VM assertions; they
are separate from production campaign outcomes and mutation scores.

Prepare the adapter dependencies with the supported SDK, then build before
starting the sequential run:

```sh
cargo build --workspace --bins --examples --locked
target/debug/failforge-tools semantic-e2e \
  --dart-sdk /absolute/flutter/bin/cache/dart-sdk \
  --output /absolute/new-semantic-evidence
target/debug/failforge-tools regression --suite smoke \
  --dart-sdk /absolute/flutter/bin/cache/dart-sdk \
  --output /absolute/new-campaign-evidence
```

`semantic-e2e` checks the operator catalogue; `regression --suite smoke` exercises
the public campaign path, including a language-specific selection. Profile
generation and multi-family configuration have separate `profiles` and
`applicability` suites. Use a fresh output directory for each command and keep
source/toolchain inputs unchanged while it runs. The generated evidence records
the actual outcome; listing a command here does not establish that it passed on
another SDK, package or platform.

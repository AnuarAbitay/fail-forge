# Mutation operators

FailForge checks whether your tests notice deliberate changes to your Dart or
Flutter code. Each mutant contains one change. A passing original test suite is
the starting point; FailForge then runs the tests against each eligible mutant
in a separate temporary copy.

An **operator** describes a kind of change, such as replacing `<` with `<=`.
One operator can produce many mutants at different locations, or none if the
selected code has no supported location.

| Family | Purpose | Catalogue |
|---|---|---|
| `classic` | Check ordinary logic, calculations, control flow and returned values. | 31 default operators in 12 categories; one additional legacy operator is available explicitly. |
| `failure_mode` | Check how callers handle a function or platform operation failing. | 2 operators; MethodChannel injection requires Flutter. |
| `language_specific` with `language: dart` | Check Dart null safety, parameter defaults, switches, patterns, collections and enums. | 19 operators in 6 categories. |

The IDs below are case-sensitive. Use them with `--operator` or as keys under
`mutations.<family>.operators` in a YAML profile. See the
[campaign guide](mutation-cli.md) for complete commands and profile settings.

The command examples assume you have followed the
[installation guide](installation.md), prepared the target package, and set
`FAILFORGE_HOME` and `DART_SDK`. Run them from the target package root.
For Flutter, replace `--dart-sdk "$DART_SDK"` with
`--flutter-sdk "$FLUTTER_SDK"`.

## Classical mutations

The 31 default operators cover these categories:

| Category | Operator ID | What changes |
|---|---|---|
| Relational | `RELATIONAL_BOUNDARY` | Includes or excludes a numeric boundary: `<` becomes `<=`, `>` becomes `>=`, and vice versa. |
| Relational | `COMPARISON_NEGATION` | Reverses a comparison, such as `==` to `!=` or `>` to `<=`. |
| Relational | `COMPARISON_OPERATOR_REPLACEMENT` | Replaces a comparison with another supported comparison; numeric values allow more alternatives than equality-only types. |
| Logical | `LOGICAL_CONNECTOR_SWAP` | Exchanges `&&` and `\|\|` while preserving expression grouping. |
| Logical | `LOGICAL_OPERAND_REMOVAL` | Replaces `a && b` or `a \|\| b` with just `a` or just `b`. |
| Arithmetic | `ARITHMETIC_OPERATOR_REPLACEMENT` | Exchanges supported numeric `+`, `-`, `*`, `/` and `%` operations. |
| Arithmetic | `ARITHMETIC_OPERAND_REMOVAL` | Replaces an arithmetic expression with one of its operands. |
| Arithmetic | `NUMERIC_NEGATION` | Adds or removes numeric negation, such as `value` to `-value`. |
| Arithmetic | `INCREMENT_DECREMENT_REPLACEMENT` | Exchanges `++` and `--`, retaining prefix or postfix form. |
| Arithmetic | `COMPOUND_ASSIGNMENT_REPLACEMENT` | Changes an arithmetic assignment, such as `+=` to `-=` or `*=` to `/=`. |
| Boolean | `BOOLEAN_LITERAL_FLIP` | Exchanges `true` and `false`. |
| Boolean | `BOOLEAN_NEGATION` | Adds or removes `!` around a boolean expression. |
| Constants | `NUMERIC_LITERAL_REPLACEMENT` | Replaces a numeric literal with alternatives such as zero, one, minus one, an adjacent value or its negation. |
| Constants | `STRING_LITERAL_REPLACEMENT` | Replaces a nonempty simple string with `''`, or an empty string with `'mutated'`. |
| Return | `RETURN_VALUE_REPLACEMENT` | Replaces a returned value with supported type-compatible alternatives, such as a scalar value, `null` or an empty collection. |
| Control flow | `CONDITION_FORCING` | Forces an ordinary `if` or loop condition to `true` or `false`. |
| Control flow | `BRANCH_BODY_REMOVAL` | Replaces an `if` or `else` body with an empty block. |
| Control flow | `CONDITIONAL_EXPRESSION_MUTATION` | Selects one arm of `condition ? a : b` unconditionally, or exchanges the two arms. |
| Control flow | `STATEMENT_DELETION` | Removes a supported call, simple assignment or numeric increment/decrement statement. |
| Control flow | `BREAK_CONTINUE_MUTATION` | Removes an unlabelled `break` or `continue`, or exchanges them where the control-flow target permits it. |
| Control flow | `SWITCH_CASE_MUTATION` | Removes a supported switch statement case or its body, or changes a simple case constant. |
| Calls | `FUNCTION_CALL_REMOVAL` | Removes a supported standalone call, or replaces its result with a type-compatible default. |
| Calls | `ARGUMENT_REPLACEMENT` | Replaces an argument with a compatible default value or an available local variable/parameter. |
| Calls | `ARGUMENT_PERMUTATION` | Exchanges two compatible positional argument values, or two named argument values while keeping their labels. |
| Variables | `VARIABLE_REPLACEMENT` | Replaces a local variable or parameter read with another compatible value available in that scope. |
| Bitwise | `BITWISE_OPERATOR_REPLACEMENT` | Changes supported integer `&`, `\|` and `^` operations. |
| Bitwise | `BITWISE_NEGATION` | Adds or removes integer bitwise negation, `~`. |
| Bitwise | `SHIFT_OPERATOR_REPLACEMENT` | Exchanges integer shifts `<<`, `>>` and `>>>`. |
| Exceptions | `THROW_REMOVAL` | Removes a standalone `throw` statement. |
| Exceptions | `EXCEPTION_REPLACEMENT` | Replaces a directly constructed SDK `StateError`, `ArgumentError` or `Exception` with one of those constructors and an empty message. |
| Collections | `COLLECTION_LITERAL_MUTATION` | Empties a supported list, set or map literal, or removes one of its elements/entries. |

The additional legacy ID `CONDITIONAL_NEGATION` negates an ordinary `if`
condition. It remains accepted when selected explicitly, but is disabled by
default because `BOOLEAN_NEGATION` covers that use case.

### Example: a boundary test

Original code:

```dart
bool canEnter(int age) => age >= 18;
```

One `RELATIONAL_BOUNDARY` mutant:

```dart
bool canEnter(int age) => age > 18;
```

A test expecting `canEnter(18)` to return `true` detects this mutant. Tests that
check only `17` and `19` do not distinguish these two versions.

### Classical applicability

The catalogue describes supported transformations, not a promise to change
every matching token:

- Arithmetic, comparison and bitwise changes require supported resolved SDK
  types and operations. Arbitrary user-defined operator overloads are excluded.
- Classical expression mutations generally target executable function bodies.
  Parameter defaults, annotations, patterns and constant contexts are excluded
  from those expression operators; use the Dart-specific operators where
  supported.
- Call and argument transformations check type compatibility and restrict
  receivers/arguments whose evaluation could have side effects. Argument
  permutation does not mix named and positional arguments.
- String replacement skips interpolation and adjacent string literals.
- Classical collection mutation handles ordinary elements and map entries.
  Spreads and collection `if`/`for` elements have dedicated Dart operators.
- Classical switch mutation supports unlabelled constant cases and `default`.
  Switch expressions and guarded patterns have dedicated Dart operators.
- Known unchanged replacements and duplicate edits are filtered. Static
  validation rejects candidates that introduce errors. This does not prove
  that every remaining mutant changes observable behavior.

## Failure-mode mutations

These mutations ask whether tests exercise failure handling, such as a caller
recovering from an exception or a platform operation failing.

| Operator ID | What changes | Applicability |
|---|---|---|
| `EXCEPTION_AT_ENTRY` | Replaces a function or method body with an immediate `StateError`. An `async` body remains asynchronous. | Named top-level functions and methods with supported concrete bodies. Constructors, local functions, abstract/external bodies and generators are excluded. |
| `METHOD_CHANNEL_ERROR` | Replaces a directly awaited `MethodChannel.invokeMethod` call with an asynchronous `PlatformException` or `MissingPluginException`. | Flutter only. Requires a resolved, non-nullable exact `MethodChannel` receiver, supported receiver/argument reads and accessible exception types. Cascades and null-aware calls are excluded. |

Selecting `failure_mode` directly enables `EXCEPTION_AT_ENTRY` for Dart and
also `METHOD_CHANNEL_ERROR` for a prepared Flutter test project. Selecting
`METHOD_CHANNEL_ERROR` explicitly for a non-Flutter project is an error.
A Flutter dependency alone does not establish the Flutter test framework;
prepared metadata must identify `flutter_test`.

Scope filters still apply. For example, restrict failure injection to a service
you expect to recover from errors:

```sh
failforge mutate --project . --family failure_mode \
  --operator EXCEPTION_AT_ENTRY \
  --include 'lib/src/service.dart' --method fetch \
  --installation "$FAILFORGE_HOME" --dart-sdk "$DART_SDK"
```

## Dart-specific mutations

Select this family with `--family language_specific --language dart`, or include
`language: dart` in the YAML family section.

| Category | Operator ID | What changes |
|---|---|---|
| Null safety | `NSM-001` | Removes the fallback from `value ?? fallback`, where the result remains valid. |
| Null safety | `NSM-002` | Changes supported null-aware property, method or index access to direct access. |
| Null safety | `NSM-003` | Replaces a nullable variable initializer or ordinary assignment value with `null`. |
| Null safety | `NSM-004` | Replaces `??=` with ordinary `=` assignment. |
| Null safety | `NSM-005` | Removes one null-aware spread element, `...?values`. |
| Parameters | `DPM-001` | Replaces a supported explicit scalar parameter default with a compatible constant. |
| Parameters | `DPM-002` | Inverts an explicit boolean parameter default. |
| Parameters | `DPM-003` | Replaces a non-null explicit parameter default with `null` when the parameter type permits it. |
| Switch | `SWM-001` | Replaces a supported switch statement branch's returned expression with a compatible expression from another branch. |
| Switch | `SWM-002` | Replaces a switch expression arm's result with a compatible result from another arm. |
| Switch | `SWM-003` | Inverts a supported switch case guard. |
| Patterns | `PMM-001` | Reverses a supported relational pattern comparison, such as `>=` to `<`. |
| Patterns | `PMM-002` | Inverts a supported if-case guard, including collection if-case guards. |
| Patterns | `PMM-003` | Exchanges logical pattern `&&` and `\|\|` when the supported binding rules permit it. |
| Collections | `CLM-001` | Inverts an ordinary collection-if condition. |
| Collections | `CLM-002` | Removes one ordinary spread element, `...values`. |
| Collections | `CLM-003` | Removes a complete collection-if element, including its `else` branch if present. |
| Collections | `CLM-004` | Removes a complete collection-for element and its body. |
| Enums | `ENM-001` | Replaces a resolved enum constant use with another accessible constant of the same enum. |

### Example: a parameter default

Original code:

```dart
bool canRead({bool enabled = true}) => enabled;
```

One `DPM-002` mutant:

```dart
bool canRead({bool enabled = false}) => enabled;
```

A test calling `canRead()` and expecting `true` detects this change. A test that
always passes `enabled: true` does not exercise the default.

### Dart-specific applicability

Some deliberately narrow rules matter when interpreting an operator marked
`false` by profile generation:

- `NSM-002` currently supports `dynamic` receivers. It does not change a typed
  nullable receiver by adding `!`, or mutate cascades.
- `NSM-003` targets variable initializers and ordinary assignment values;
  it does not replace arbitrary expressions with `null`.
- Parameter operators require explicit, resolved constant defaults on optional
  positional or named parameters. They do not add defaults to required
  parameters.
- `SWM-001` uses unlabelled arms containing one direct `return expression;`.
  Switch result replacements require compatible types and skip donors that
  depend on branch-local bindings or promotions. Switch expressions must be
  non-constant.
- Guard inversion skips known constant guards and checks the result for invalid
  promotions and reachability. Logical pattern mutation excludes patterns that
  bind or assign variables.
- Collection deletion requires an element directly inside a list, set or map
  literal. The mutation preserves the collection's type; it is skipped if that
  type cannot be represented safely at the mutation site.
- Enum replacement uses type-compatible members of the same resolved enum.
  Enum declarations, annotations and patterns are excluded, as are names that
  would bind to a different declaration at the replacement site.

See the [Dart semantic guide](dart-semantic-mutations.md) for the complete
applicability details.

## Selecting operators and files

Direct command arguments select one family. Repeat `--operator` to narrow it,
and repeat `--include`, `--exclude`, `--class` or `--method` as needed:

```sh
failforge mutate --project . --family classic \
  --operator RELATIONAL_BOUNDARY \
  --operator BOOLEAN_LITERAL_FLIP \
  --include 'lib/src/**.dart' --exclude '**/*.g.dart' \
  --installation "$FAILFORGE_HOME" --dart-sdk "$DART_SDK"
```

Different positive filter categories combine with **AND**. Entries within one
category combine with **OR**. Exclusions take priority. Class and method filters
choose mutation locations; they do not select individual tests to execute.
Discovery is limited to library sources under `lib/`; tests, generated files
and dependency/tool directories are excluded.

A YAML profile can select more than one family. For example:

```yaml
dart_sdk: /absolute/path/to/dart-sdk
rules:
  include: ['lib/src/calculator.dart']
mutations:
  classic:
    enabled: true
    operators:
      RELATIONAL_BOUNDARY: true
      BOOLEAN_LITERAL_FLIP: false
  language_specific:
    enabled: true
    language: dart
    operators:
      DPM-002: true
```

Both the family and the individual operator must be enabled. Omitted families
and operators are disabled. An existing `language_specific` section must state
`language: dart`, even when disabled. Unknown IDs, wrong-family IDs and invalid
boolean switches are errors, including in disabled sections. A profile needs
at least one active operator.

To generate switches from a particular file instead of selecting them manually:

```sh
failforge profile generate --file lib/src/calculator.dart \
  --family classic --family language_specific --language dart \
  --output profiles/calculator.yml \
  --installation "$FAILFORGE_HOME" --dart-sdk "$DART_SDK"
```

By default, generation enables an operator only if it finds at least one
statically valid candidate in that file. It does not run your tests.
`--template` writes catalogue selections without checking applicability.

## Interpreting a result

A **KILLED** mutant caused a verified test failure. A **SURVIVED** mutant
completed the expected tests successfully and deserves investigation: a missing
assertion, untested behavior or an equivalent change can all explain it.

An **INVALID** candidate is rejected by static validation or by the test compiler.
Candidates rejected statically do not execute tests. A process
**TIMEOUT** means the outer execution budget expired; it is not automatically a
kill. A timeout reported as a failure by an identified Dart test can establish
**KILLED**. Infrastructure and unclassified failures stay separate from
detected behavioral changes.

Each family's score uses only `KILLED / (KILLED + SURVIVED)`. Other outcomes do
not increase the score. An incomplete campaign has no final score, and an empty
denominator has no percentage. A high score measures the tests against the
selected mutations; it is not proof that the project has no defects.

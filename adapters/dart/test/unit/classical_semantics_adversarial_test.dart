import 'dart:convert';
import 'dart:io';

import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/diagnostic/diagnostic.dart';
import 'package:failforge_dart_adapter/src/operators/common.dart';
import 'package:failforge_dart_adapter/src/operators/classic/calls/argument_permutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/calls/argument_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/calls/function_call_removal.dart';
import 'package:failforge_dart_adapter/src/operators/classic/collections/collection_literal_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/constants/string_literal_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/break_continue_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/condition_forcing.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/switch_case_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/statement_deletion.dart';
import 'package:failforge_dart_adapter/src/operators/classic/exceptions/exception_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/returns/return_value_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/variables/variable_replacement.dart';
import 'package:test/test.dart';

import '../support/algebra_fixture.dart';

void main() {
  test(
    'forcing parenthesized literal conditions does not emit semantic no-ops',
    () async {
      const source = 'int f() { if ((true)) return 1; return 2; }\n';
      final fixture = await open(source);
      final scan = const ConditionForcing().discover(
        await fixture.resolve(source),
        source,
      );
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'false',
      ]);
      await validate(fixture, source, scan.candidates);
    },
  );

  test('switch constants do not acquire no-op hexadecimal or parenthesis rewrites', () async {
    const source =
        'int f(int x) { switch(x) { case (0x1): return 5; default: return 6; } }\n';
    final fixture = await open(source);
    final scan = const SwitchCaseMutation().discover(
      await fixture.resolve(source),
      source,
    );
    final start = source.indexOf('(0x1)');
    final constants = scan.candidates
        .where(
          (candidate) =>
              candidate.offset == start && candidate.length == '(0x1)'.length,
        )
        .toList();
    expect(
      constants.map((candidate) => candidate.replacement),
      isNot(contains('1')),
    );
    await validate(fixture, source, constants);
  });

  test('argument permutations reject the same element and equivalent literal values', () async {
    const source =
        "void accept(Object a, Object b) {} void f(int a) { accept((a), a); accept(0x1, 1); accept(r'', ''); }\n";
    final fixture = await open(source);
    final scan = const ArgumentPermutation().discover(
      await fixture.resolve(source),
      source,
    );
    expect(scan.candidates, isEmpty);
  });

  test('empty SDK exception messages do not create spelling-only replacements', () async {
    const source = "void f() { throw (StateError(r'')); }\n";
    final fixture = await open(source);
    final scan = const ExceptionReplacement().discover(
      await fixture.resolve(source),
      source,
    );
    expect(scan.candidates.map((candidate) => candidate.replacement), [
      "ArgumentError('')",
      "Exception('')",
    ]);
    await validate(fixture, source, scan.candidates);

    const namedSource =
        "void main() { try { throw ArgumentError.value('', 'name', ''); } catch(error) { print(error); } }\n";
    final namedFixture = await open(namedSource);
    final named = const ExceptionReplacement().discover(
      await namedFixture.resolve(namedSource),
      namedSource,
    );
    final unnamedReplacement = named.candidates.singleWhere(
      (candidate) => candidate.replacement == "ArgumentError('')",
    );
    expect(
      await namedFixture.execute(apply(namedSource, unnamedReplacement)),
      isNot(await namedFixture.execute(namedSource)),
    );
    await validate(namedFixture, namedSource, named.candidates);
  });

  test('generic collection types cannot silently bind to an inner shadowing type parameter', () async {
    const source = '''
String outer<T>(T value) {
 String inner<T>() {
  final collection = [value, value];
  return collection.runtimeType.toString();
 }
 return inner<int>();
}
void main() { print(outer<String>('value')); }
''';
    final fixture = await open(source);
    expect(await fixture.execute(source), ['List<String>']);
    final scan = const CollectionLiteralMutation().discover(
      await fixture.resolve(source),
      source,
    );
    final candidates = scan.candidates
        .where((candidate) => original(source, candidate) == '[value, value]')
        .toList();
    final incorrectlyBound = candidates
        .where((candidate) => candidate.replacement == '<T>[]')
        .toList();
    if (incorrectlyBound.isNotEmpty) {
      expect(await fixture.execute(apply(source, incorrectlyBound.single)), [
        'List<int>',
      ]);
    }
    expect(
      incorrectlyBound,
      isEmpty,
      reason: 'The original List<T> uses outer T; inner T is a different resolved element.',
    );
  });

  test(
    'double-context integer zero return keeps the sign-changing replacement',
    () async {
      const source = 'double f() => (-0);\nvoid main() { print(1 / f()); }\n';
      final fixture = await open(source);
      expect(await fixture.execute(source), ['-Infinity']);
      final scan = const ReturnValueReplacement().discover(
        await fixture.resolve(source),
        source,
      );
      final values = scan.candidates
          .where((candidate) => original(source, candidate) == '(-0)')
          .toList();
      expect(values.map((candidate) => candidate.replacement), contains('0.0'));
      final positive = values.singleWhere(
        (candidate) => candidate.replacement == '0.0',
      );
      expect(await fixture.execute(apply(source, positive)), ['Infinity']);
      await validate(fixture, source, values);
    },
  );

  test('parenthesized empty return collections remain nonmutations', () async {
    const source =
        'List<int> list() => (<int>[]);\nSet<int> set() => (<int>{});\nMap<String, int> map() => (<String, int>{});\n';
    final fixture = await open(source);
    final unit = await fixture.resolve(source);
    expect(
      const ReturnValueReplacement().discover(unit, source).candidates,
      isEmpty,
    );
    expect(
      const CollectionLiteralMutation().discover(unit, source).candidates,
      isEmpty,
    );
  });

  test('generic constructor argument mutations use instantiated parameter bindings', () async {
    const source = '''
class Box<T> { Box(T value, {required T named}); }
void f(int value, String text) { Box<int>(value, named: value); }
''';
    final fixture = await open(source);
    final scan = const ArgumentReplacement().discover(
      await fixture.resolve(source),
      source,
    );
    expect(scan.candidates, isNotEmpty);
    expect(
      scan.candidates.any(
        (candidate) =>
            candidate.replacement == 'text' ||
            candidate.replacement == 'null' ||
            candidate.replacement == "''",
      ),
      isFalse,
    );
    await validate(fixture, source, scan.candidates);
  });

  test('invocation-as-receiver defaults stay grouped and SDK update statements remain deletable', () async {
    const source =
        'int read() => 4;\nString value() => read().toString();\nvoid main() { print(value()); }\n';
    final fixture = await open(source);
    final scan = const FunctionCallRemoval().discover(
      await fixture.resolve(source),
      source,
    );
    final inner = scan.candidates
        .where((candidate) => original(source, candidate) == 'read()')
        .toList();
    expect(inner, isNotEmpty);
    await validate(fixture, source, inner);
    final negative = inner.singleWhere(
      (candidate) => candidate.replacement == '(-1)',
    );
    expect(await fixture.execute(apply(source, negative)), ['-1']);

    const updateSource =
        'void main() { num value = 1; value++; print(value); }\n';
    final updateFixture = await open(updateSource);
    final update = const StatementDeletion().discover(
      await updateFixture.resolve(updateSource),
      updateSource,
    );
    final deletedUpdate = update.candidates
        .where((candidate) => original(updateSource, candidate) == 'value++;')
        .toList();
    expect(
      deletedUpdate,
      hasLength(1),
      reason: 'Supported SDK increments must use the analyzer readType for an lvalue.',
    );
    expect(
      await updateFixture.execute(apply(updateSource, deletedUpdate.single)),
      ['1'],
    );
    await validate(updateFixture, updateSource, deletedUpdate);
  });

  test('shadowing local functions block outer variable replacements', () async {
    const source = 'int f(int a, int b) { { void b() {} return a; } }\n';
    final fixture = await open(source);
    final scan = const VariableReplacement().discover(
      await fixture.resolve(source),
      source,
    );
    expect(
      scan.candidates.where((candidate) => original(source, candidate) == 'a'),
      isEmpty,
    );
  });

  test('continue inside nested switch never turns into a break for a different target', () async {
    const source = '''
void main() {
 final out = <int>[];
 for (var i = 0; i < 3; i++) {
  switch (i) { case 1: continue; default: out.add(i); }
  out.add(10 + i);
 }
 print(out.join(','));
}
''';
    final fixture = await open(source);
    final scan = const BreakContinueMutation().discover(
      await fixture.resolve(source),
      source,
    );
    expect(scan.candidates.map((candidate) => candidate.replacement), [';']);
    expect(
      scan.skips.map((skip) => skip.code),
      contains('continue_inside_switch'),
    );
    expect(await fixture.execute(source), ['0,10,2,12']);
    expect(await fixture.execute(apply(source, scan.candidates.single)), [
      '0,10,11,2,12',
    ]);
    await validate(fixture, source, scan.candidates);
  });

  test('adjacent and interpolated strings stay excluded while independent strings remain mutable', () async {
    const source =
        "String adjacent() => 'a' 'b';\nString interpolated(String x) => 'value=\$x';\nString independent() => r'payload';\n";
    final fixture = await open(source);
    final scan = const StringLiteralReplacement().discover(
      await fixture.resolve(source),
      source,
    );
    expect(scan.candidates, hasLength(1));
    expect(original(source, scan.candidates.single), "r'payload'");
    await validate(fixture, source, scan.candidates);
  });
}

Future<AlgebraFixture> open(String source) async {
  final fixture = await AlgebraFixture.open(source);
  addTearDown(fixture.close);
  noErrors(await fixture.resolve(source));
  return fixture;
}

String original(String source, CandidateDraft candidate) =>
    source.substring(candidate.offset, candidate.offset + candidate.length);
String apply(String source, CandidateDraft candidate) => source.replaceRange(
  candidate.offset,
  candidate.offset + candidate.length,
  candidate.replacement,
);
void noErrors(ResolvedUnitResult unit) => expect(
  unit.diagnostics.where((diagnostic) => diagnostic.severity == Severity.error),
  isEmpty,
  reason: unit.diagnostics.map((diagnostic) => diagnostic.message).join('\n'),
);
Future<void> validate(
  AlgebraFixture fixture,
  String source,
  Iterable<CandidateDraft> candidates,
) async {
  for (final candidate in candidates) {
    noErrors(await fixture.resolve(apply(source, candidate)));
    noErrors(await fixture.resolve(source));
  }
  expect(await File(fixture.path).readAsBytes(), utf8.encode(source));
}

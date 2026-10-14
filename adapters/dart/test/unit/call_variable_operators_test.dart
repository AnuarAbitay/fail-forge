import 'package:failforge_dart_adapter/src/operators/classic/calls/argument_permutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/calls/argument_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/calls/function_call_removal.dart';
import 'package:failforge_dart_adapter/src/operators/classic/constants/string_literal_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/variables/variable_replacement.dart';
import 'package:test/test.dart';

import '../support/call_operator_fixture.dart';

void main() {
  group('string literal replacement', () {
    test('simple, raw, multiline and empty literals get one semantic replacement', () async {
      const source =
          "String f() { final a = r'raw\\n'; final b = '''multi\nline'''; final c = ''; return 'text'; }";
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const StringLiteralReplacement().discover(unit, source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        "''",
        "''",
        "'mutated'",
        "''",
      ]);
      await fixture.validateAll(scan.candidates);
    });

    test('metadata, defaults, fields, const, adjacent and interpolated strings are excluded', () async {
      const source = r'''
class Mark { const Mark(String value); }
@Mark('metadata')
class C { final field = 'field'; String f([String arg = 'default']) {
  const constant = 'constant'; final parts = 'left' 'right';
  final interpolation = 'prefix $arg'; return 'allowed';
} }
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const StringLiteralReplacement().discover(unit, source);
      expect(scan.candidates, hasLength(1));
      expect(fixture.before(scan.candidates.single), "'allowed'");
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll([
          'metadata_context',
          'default_parameter',
          'non_executable_context',
          'constant_context',
          'compound_string',
          'interpolated_string',
        ]),
      );
      await fixture.validateAll(scan.candidates);
    });
  });

  group('variable replacement', () {
    test('wildcards are not addressable and local functions shadow outer variable names', () async {
      const source = '''
int f(int a, int _, int usable) { final _ = 1; return a; }
int g(int a, int b) { { void b() {} return a; } }
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const VariableReplacement().discover(unit, source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'usable',
      ]);
      await fixture.validateAll(scan.candidates);
    });
    test('resolved parameters and preceding initialized locals are type-compatible', () async {
      const source =
          'int f(int a, int b, String text) { final first = 7; return a; }';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const VariableReplacement().discover(unit, source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'first',
        'b',
      ]);
      expect(
        scan.candidates.every((candidate) => fixture.before(candidate) == 'a'),
        isTrue,
      );
      await fixture.validateAll(scan.candidates);
    });

    test(
      'later, uninitialized, late and shadowed variables cannot be selected',
      () async {
        const source = '''
int f(int a, int b) {
  int missing; late int delayed = 1;
  { String b = 'shadow'; final result = a; int later = 2; return result; }
}
''';
        final fixture = CallOperatorFixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve();
        CallOperatorFixture.noErrors(unit);
        final scan = const VariableReplacement().discover(unit, source);
        final readA = scan.candidates
            .where((candidate) => fixture.before(candidate) == 'a')
            .toList();
        expect(
          readA,
          isEmpty,
          reason: 'The nearest b is a String; the outer int b is shadowed.',
        );
        expect(
          scan.candidates.map((candidate) => candidate.replacement),
          isNot(contains('missing')),
        );
        expect(
          scan.candidates.map((candidate) => candidate.replacement),
          isNot(contains('delayed')),
        );
        await fixture.validateAll(scan.candidates);
      },
    );

    test(
      'scopes, setters, fields, getters and qualified selectors are preserved',
      () async {
        const source = '''
class C { int field = 1; int get getter => 2;
  int f(int a, int b) { { final other = 3; }
    a = b; a++; return this.field + getter + a;
  }
}
''';
        final fixture = CallOperatorFixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve();
        CallOperatorFixture.noErrors(unit);
        final scan = const VariableReplacement().discover(unit, source);
        expect(scan.candidates.map((candidate) => fixture.before(candidate)), [
          'b',
          'a',
        ]);
        expect(scan.candidates.map((candidate) => candidate.replacement), [
          'a',
          'b',
        ]);
        await fixture.validateAll(scan.candidates);
      },
    );

    test('declaration identity handles multiple declarations and pattern shadowing', () async {
      const source = '''
int f(int a, int b) { final first = 1, second = a;
  { final (b, label) = ('shadow', 'text'); return a; }
}
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const VariableReplacement().discover(unit, source);
      final initializerOffset =
          source.indexOf('second = a') + 'second = '.length;
      final initializer = scan.candidates
          .where((candidate) => candidate.offset == initializerOffset)
          .toList();
      expect(initializer.map((candidate) => candidate.replacement), [
        'first',
        'b',
      ]);
      final returnOffset = source.indexOf('return a') + 'return '.length;
      expect(
        scan.candidates
            .where((candidate) => candidate.offset == returnOffset)
            .map((candidate) => candidate.replacement),
        ['first', 'second'],
      );
      await fixture.validateAll(scan.candidates);
    });

    test('nullable, dynamic and incompatible user types are not downcast implicitly', () async {
      const source =
          'int f(int value, int? nullable, dynamic unknown, String text) => value;';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      expect(
        const VariableReplacement().discover(unit, source).candidates,
        isEmpty,
      );
    });
  });

  group('argument replacement', () {
    test('same elements and semantic literal defaults are not source-only mutations', () async {
      const source =
          "void accept(Object? value) {} void f(int a) { accept((a)); accept(r''); accept(0.00); }";
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ArgumentReplacement().discover(unit, source);
      final parenthesized = source.indexOf('(a)', source.indexOf('void f'));
      final empty = source.indexOf("r''");
      final zero = source.indexOf('0.00');
      expect(
        scan.candidates
            .where((candidate) => candidate.offset == parenthesized)
            .map((candidate) => candidate.replacement),
        isNot(contains('a')),
      );
      expect(
        scan.candidates
            .where((candidate) => candidate.offset == empty)
            .map((candidate) => candidate.replacement),
        isNot(contains("''")),
      );
      expect(
        scan.candidates
            .where((candidate) => candidate.offset == zero)
            .map((candidate) => candidate.replacement),
        isNot(contains('0.0')),
      );
      await fixture.validateAll(scan.candidates);
    });
    test('uses declared Object parameter rather than original int expression type', () async {
      const source =
          'Object accept(Object value) => value; Object f(int a, String b) => accept(a);';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ArgumentReplacement().discover(unit, source);
      expect(
        scan.candidates.map((candidate) => candidate.replacement),
        containsAll(['b', "''", 'false', '0']),
      );
      expect(
        scan.candidates.map((candidate) => candidate.replacement),
        isNot(contains('null')),
      );
      await fixture.validateAll(scan.candidates);
    });

    test('named labels, optional parameters, constructors and generic binding are respected', () async {
      const source = '''
class Box { Box(Object value, {int? count}); }
T identity<T>(T value) => value;
Object accept(int first, [Object? second]) => second ?? first;
Object f(int a, String b) { Box(a, count: a); accept(a, b); return identity<int>(a); }
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ArgumentReplacement().discover(unit, source);
      final namedOffset = source.indexOf('count: a') + 'count: '.length;
      final named = scan.candidates
          .where((candidate) => candidate.offset == namedOffset)
          .toList();
      expect(named.map((candidate) => candidate.replacement), contains('null'));
      expect(
        named.map((candidate) => candidate.replacement),
        isNot(contains('b')),
      );
      for (final candidate in named) {
        expect(
          fixture.apply(candidate),
          contains('count: ${candidate.replacement}'),
        );
      }
      final genericOffset =
          source.indexOf('identity<int>(a)') + 'identity<int>('.length;
      expect(
        scan.candidates
            .where((candidate) => candidate.offset == genericOffset)
            .map((candidate) => candidate.replacement),
        isNot(contains('b')),
      );
      final constructorOffset = source.indexOf('Box(a,') + 'Box('.length;
      expect(
        scan.candidates
            .where((candidate) => candidate.offset == constructorOffset)
            .map((candidate) => candidate.replacement),
        contains('b'),
      );
      await fixture.validateAll(scan.candidates);
    });

    test(
      'effectful getters/calls and unresolved dynamic invocations are skipped',
      () async {
        const source = '''
class C { int get value => 1; }
int next() => 1; void accept(int value) {}
void f(C c, dynamic callable) { accept(c.value); accept(next()); callable(1); }
''';
        final fixture = CallOperatorFixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve();
        CallOperatorFixture.noErrors(unit);
        final scan = const ArgumentReplacement().discover(unit, source);
        expect(scan.candidates, isEmpty);
        expect(
          scan.skips.where((skip) => skip.code == 'unsupported_argument'),
          hasLength(3),
        );
      },
    );
  });

  group('argument permutation', () {
    test('format-only same values are skipped while distinct double sign-zero remains observable', () async {
      const source = r'''
void pair(Object? a, Object? b) {}
void f(int value) { pair(value, (value)); pair(0x1, 1); pair(r'', "");
  pair(1.00, 1.0); pair(-0.0, 0.0); }
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ArgumentPermutation().discover(unit, source);
      expect(scan.candidates, hasLength(1));
      expect(fixture.before(scan.candidates.single), '-0.0, 0.0');
      expect(scan.candidates.single.replacement, '0.0, -0.0');
      await fixture.validateAll(scan.candidates);
    });
    test('compatible positional arguments retain named labels, comments and middle values', () async {
      const source = '''
void accept(Object a, Object b, Object c, {int label = 0}) {}
void f(int x, String y, bool z) { accept(x /*first*/, y, z /*last*/, label: 9); }
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ArgumentPermutation().discover(unit, source);
      expect(scan.candidates, hasLength(3));
      expect(
        scan.candidates.map((candidate) => candidate.replacement),
        contains('z /*first*/, y, x'),
      );
      expect(
        scan.candidates.every(
          (candidate) =>
              fixture.apply(candidate).contains('/*last*/, label: 9'),
        ),
        isTrue,
      );
      await fixture.validateAll(scan.candidates);
    });

    test('named values exchange without labels; mismatches, effects and identical args are skipped', () async {
      const source = '''
void named({required int a, required int b}) {}
void mixed(int a, String b) {} void pair(int a, int b) {} int next() => 1;
void f(int x, String y) { named(a: x, b: 1); mixed(x, y); pair(next(), x); pair(x, x); }
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ArgumentPermutation().discover(unit, source);
      expect(scan.candidates, hasLength(1));
      expect(fixture.before(scan.candidates.single), 'x, b: 1');
      expect(scan.candidates.single.replacement, '1, b: x');
      expect(
        fixture.apply(scan.candidates.single),
        contains('named(a: 1, b: x)'),
      );
      await fixture.validateAll(scan.candidates);
    });

    test('optional positional and constructor arguments use their resolved signature', () async {
      const source = '''
class Pair { Pair(int a, [int b = 0]); }
void accept(int a, [int b = 0]) {}
void f(int a, int b) { Pair(a, b); accept(a, b); }
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ArgumentPermutation().discover(unit, source);
      expect(scan.candidates, hasLength(2));
      expect(
        scan.candidates.every((candidate) => candidate.replacement == 'b, a'),
        isTrue,
      );
      await fixture.validateAll(scan.candidates);
    });
  });

  group('function/method call removal', () {
    test(
      'every typed default remains grouped when the invocation is a receiver',
      () async {
        const source = 'int read() => 2; String f() => read().toString();';
        final fixture = CallOperatorFixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve();
        CallOperatorFixture.noErrors(unit);
        final scan = const FunctionCallRemoval().discover(unit, source);
        final numeric = scan.candidates
            .where((candidate) => fixture.before(candidate) == 'read()')
            .toList();
        expect(numeric, hasLength(3));
        await fixture.validateAll(numeric);
      },
    );
    test('standalone and branch calls become empty statements; methods and tear-offs are supported', () async {
      const source = '''
void notify(int value) {} class C { void run(int value) {} }
void f(bool flag, int value, C c, void Function(int) action) {
  notify(value); if (flag) c.run(value); action(value);
}
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const FunctionCallRemoval().discover(unit, source);
      expect(scan.candidates, hasLength(3));
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '',
        ';',
        '',
      ]);
      await fixture.validateAll(scan.candidates);
    });

    test('expressions receive primitive, nullable and void defaults with valid result types', () async {
      const source = '''
int number() => 7; String? text() => 'x'; void action() {}
int f() => number(); String? g() => text(); void h() => action();
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const FunctionCallRemoval().discover(unit, source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '(0)',
        '(1)',
        '(-1)',
        "('')",
        '(null)',
        '((() {})())',
      ]);
      await fixture.validateAll(scan.candidates);
    });

    test('unresolved dynamic, getters, late values, cascades and effectful receivers/args are excluded', () async {
      const source = '''
class C { int get value => 1; void call(int a) {} }
C make() => C(); int next() => 1; void accept(int a) {}
void f(C c, dynamic unknown) {
  make().call(1); accept(next()); accept(c.value); unknown.call(1);
  late int delayed = 1; accept(delayed); c..call(1); c?.call(1);
}
''';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const FunctionCallRemoval().discover(unit, source);
      // The nested calls next() and make() themselves are not removed: their
      // non-primitive C result has no default; next() is a safe int invocation.
      expect(
        scan.candidates.where(
          (candidate) => fixture.before(candidate).contains('accept('),
        ),
        isEmpty,
      );
      expect(
        scan.candidates.where(
          (candidate) => fixture.before(candidate).contains('.call('),
        ),
        isEmpty,
      );
      expect(
        scan.skips.map((skip) => skip.code),
        contains('effectful_or_unresolved_call'),
      );
      await fixture.validateAll(scan.candidates);
    });
  });

  test('finite execution observes call removal, variable/argument replacement and permutation changes', () async {
    const source = '''
int count = 0;
void notify() { count++; }
int subtract(int a, int b) => a - b;
void main() { int a = 8, b = 3; notify(); print(count); print(subtract(a, b)); }
''';
    final fixture = CallOperatorFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve();
    CallOperatorFixture.noErrors(unit);
    expect(await fixture.execute(source), '1\n5\n');
    final call = const FunctionCallRemoval()
        .discover(unit, source)
        .candidates
        .singleWhere((candidate) => fixture.before(candidate) == 'notify();');
    expect(await fixture.execute(fixture.apply(call)), '0\n5\n');
    final permutation = const ArgumentPermutation()
        .discover(unit, source)
        .candidates
        .single;
    expect(await fixture.execute(fixture.apply(permutation)), '1\n-5\n');
    final replacement = const ArgumentReplacement()
        .discover(unit, source)
        .candidates
        .singleWhere(
          (candidate) =>
              candidate.offset ==
                  source.indexOf('subtract(a, b)') + 'subtract('.length &&
              candidate.replacement == 'b',
        );
    expect(await fixture.execute(fixture.apply(replacement)), '1\n0\n');
    final variable = const VariableReplacement()
        .discover(unit, source)
        .candidates
        .singleWhere(
          (candidate) =>
              candidate.offset == source.indexOf('a - b') &&
              candidate.replacement == 'b',
        );
    expect(await fixture.execute(fixture.apply(variable)), '1\n0\n');
  });
}

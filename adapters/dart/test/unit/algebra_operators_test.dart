import 'dart:convert';
import 'dart:io';

import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/diagnostic/diagnostic.dart';
import 'package:failforge_dart_adapter/src/operators/common.dart';
import 'package:failforge_dart_adapter/src/operators/classic/relational/comparison_negation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/relational/comparison_operator_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/boolean/boolean_negation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/arithmetic/arithmetic_operator_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/arithmetic/arithmetic_operand_removal.dart';
import 'package:failforge_dart_adapter/src/operators/classic/arithmetic/numeric_negation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/arithmetic/increment_decrement_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/arithmetic/compound_assignment_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/constants/numeric_literal_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/logical/logical_operand_removal.dart';
import 'package:failforge_dart_adapter/src/operators/classic/bitwise/bitwise_operator_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/bitwise/bitwise_negation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/bitwise/shift_operator_replacement.dart';
import 'package:failforge_dart_adapter/src/project/source_file.dart';
import 'package:test/test.dart';

import '../support/algebra_fixture.dart';

const operators = <ClassicOperator>[
  ComparisonNegation(),
  ComparisonOperatorReplacement(),
  BooleanNegation(),
  ArithmeticOperatorReplacement(),
  ArithmeticOperandRemoval(),
  NumericNegation(),
  IncrementDecrementReplacement(),
  CompoundAssignmentReplacement(),
  NumericLiteralReplacement(),
  LogicalOperandRemoval(),
  BitwiseOperatorReplacement(),
  BitwiseNegation(),
  ShiftOperatorReplacement(),
];

void main() {
  test('all thirteen algebra operators explicitly declare classic categories and distinct IDs', () {
    expect(operators.map((operator) => operator.id).toSet(), hasLength(13));
    expect(
      operators.every((operator) => operator.family == MutationFamily.classic),
      isTrue,
    );
    expect(operators.map((operator) => operator.category), [
      ClassicalCategory.relational,
      ClassicalCategory.relational,
      ClassicalCategory.boolean,
      ClassicalCategory.arithmetic,
      ClassicalCategory.arithmetic,
      ClassicalCategory.arithmetic,
      ClassicalCategory.arithmetic,
      ClassicalCategory.arithmetic,
      ClassicalCategory.constants,
      ClassicalCategory.logical,
      ClassicalCategory.bitwise,
      ClassicalCategory.bitwise,
      ClassicalCategory.bitwise,
    ]);
  });

  test('comparison negation covers all six token replacements and keeps Unicode ranges exact', () async {
    const source =
        '// Кириллица 😀\r\nbool f(num a, num b) => a > b || a >= b || a < b || a <= b || a == b || a != b;\r\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const ComparisonNegation().discover(unit, source);
    expect(scan.candidates.map((candidate) => candidate.replacement), [
      '<=',
      '<',
      '>=',
      '>',
      '!=',
      '==',
    ]);
    expect(scan.candidates.map((candidate) => original(source, candidate)), [
      '>',
      '>=',
      '<',
      '<=',
      '==',
      '!=',
    ]);
    final snapshot = SourceFile(
      'lib/source.dart',
      fixture.path,
      utf8.encode(source),
    );
    for (final candidate in scan.candidates) {
      expect(
        snapshot
            .editFromAnalyzer(
              candidate.offset,
              candidate.length,
              candidate.replacement,
            )
            .apply(snapshot),
        utf8.encode(apply(source, candidate)),
      );
    }
    await validMutants(fixture, source, scan.candidates);
  });

  test('comparison replacement emits five numeric variants and only equality for other SDK types', () async {
    const source =
        'bool numeric(num a, num b) => a >= b;\nbool text(String a, String b) => a == b;\nbool nullable(Object? value) => value == null;\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const ComparisonOperatorReplacement().discover(unit, source);
    expect(scan.candidates, hasLength(7));
    expect(scan.candidates.take(5).map((candidate) => candidate.replacement), [
      '>',
      '<',
      '<=',
      '((a) == (b))',
      '((a) != (b))',
    ]);
    await validMutants(fixture, source, scan.candidates);
  });

  test('comparison operators exclude dynamic and user overloaded equality and ordering', () async {
    const source = '''
class Number {
 bool operator >(Number value) => true;
 bool operator ==(Object value) => true;
 int get hashCode => 0;
}
bool f(Number a, Number b) => a > b || a == b;
bool g(dynamic a, dynamic b) => a > b || a == b;
''';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    for (final operator in const [
      ComparisonNegation(),
      ComparisonOperatorReplacement(),
    ]) {
      final scan = operator.discover(unit, source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips, hasLength(4));
    }
  });

  test('user types inheriting SDK Object equality support equality-only mutations', () async {
    const source =
        'class Value {}\nbool f(Value a, Value b) => a == b;\nbool nullable(Value? value) => value != null;\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    for (final operator in const [
      ComparisonNegation(),
      ComparisonOperatorReplacement(),
    ]) {
      final scan = operator.discover(unit, source);
      expect(scan.candidates, hasLength(2));
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '!=',
        '==',
      ]);
      await validMutants(fixture, source, scan.candidates);
    }
  });

  test('arithmetic replacement covers all extended variants and preserves comments and precedence', () async {
    const source = 'num f(num a, num b, num c) => a /*a*/ + /*b*/ b * c;\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const ArithmeticOperatorReplacement().discover(unit, source);
    expect(scan.candidates, hasLength(8));
    expect(scan.candidates.first.replacement, '((a) /*a*/ - /*b*/ (b * c))');
    expect(
      scan.candidates
          .map(
            (candidate) =>
                (candidate.offset, candidate.length, candidate.replacement),
          )
          .toSet(),
      hasLength(8),
    );
    await validMutants(fixture, source, scan.candidates);
  });

  test('every arithmetic token has all four extended variants and both operand removals', () async {
    const tokens = ['+', '-', '*', '/', '%'];
    final source = [
      for (var index = 0; index < tokens.length; index++)
        'num f$index(num a, num b) => a ${tokens[index]} b;',
    ].join('\n');
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final replacements = const ArithmeticOperatorReplacement().discover(
      unit,
      source,
    );
    expect(replacements.candidates, hasLength(20));
    for (var index = 0; index < tokens.length; index++) {
      final actual = replacements.candidates
          .skip(index * 4)
          .take(4)
          .map((candidate) => candidate.replacement)
          .toSet();
      expect(actual, {
        for (final token in tokens)
          if (token != tokens[index]) '((a) $token (b))',
      });
    }
    await validMutants(fixture, source, replacements.candidates);
    final removals = const ArithmeticOperandRemoval().discover(unit, source);
    expect(removals.candidates, hasLength(10));
    await validMutants(fixture, source, removals.candidates);
  });

  test('arithmetic operand removal preserves the retained expression grouping and deduplicates equal operands', () async {
    const source =
        'num f(num a, num b, num c) => (a + b) * c;\nnum same(num x) => x + x;\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const ArithmeticOperandRemoval().discover(unit, source);
    expect(scan.candidates, hasLength(5));
    expect(scan.candidates.first.replacement, '((a + b))');
    await validMutants(fixture, source, scan.candidates);
  });

  test('arithmetic and bitwise guards exclude user operators, dynamic and wrong types', () async {
    const source = '''
class Number {
 Number operator +(Object value) => this;
 Number operator &(Number value) => this;
 Number operator <<(int value) => this;
 Number operator -() => this;
 Number operator ~() => this;
}
Number custom(Number a, Number b) { a += b; a++; return -(a + b) & (a << 1); }
dynamic dyn(dynamic a, dynamic b) => a + b;
double decimal(double a, double b) => a + b;
''';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    for (final operator in const [
      ArithmeticOperandRemoval(),
      ArithmeticOperatorReplacement(),
    ]) {
      final scan = operator.discover(unit, source);
      expect(
        scan.candidates.every(
          (candidate) => candidate.offset >= source.indexOf('double decimal'),
        ),
        isTrue,
      );
      expect(scan.skips, hasLength(2));
    }
    for (final operator in const [
      IncrementDecrementReplacement(),
      CompoundAssignmentReplacement(),
      BitwiseOperatorReplacement(),
      ShiftOperatorReplacement(),
    ]) {
      expect(operator.discover(unit, source).candidates, isEmpty);
    }
    for (final operator in const [NumericNegation(), BitwiseNegation()]) {
      expect(
        operator
            .discover(unit, source)
            .candidates
            .where(
              (candidate) =>
                  original(source, candidate).startsWith('-(') ||
                  original(source, candidate).startsWith('~'),
            ),
        isEmpty,
      );
    }
  });

  test('boolean, numeric and bitwise negations add/remove once and remain valid as property receivers', () async {
    const source = '''
String boolean(bool flag) => flag.toString();
bool inverted(bool flag) => !flag;
num numeric(num x) => -x;
int bits(int x) => ~x;
num receiver(num x) => x.abs();
class Holder { bool get flag => true; }
bool qualified(Holder holder) => holder.flag;
''';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    for (final operator in const [
      BooleanNegation(),
      NumericNegation(),
      BitwiseNegation(),
    ]) {
      final scan = operator.discover(unit, source);
      expect(scan.candidates, isNotEmpty);
      expect(
        scan.candidates.any(
          (candidate) =>
              candidate.replacement == '(flag)' ||
              candidate.replacement == '(x)',
        ),
        isTrue,
      );
      expect(
        scan.candidates.any(
          (candidate) =>
              original(source, candidate) == 'flag' &&
              candidate.offset == source.indexOf('flag;') &&
              source.substring(candidate.offset - 1, candidate.offset) == '.',
        ),
        isFalse,
      );
      await validMutants(fixture, source, scan.candidates);
    }
  });

  test(
    'increment replacement covers prefix/postfix forms with one token edit',
    () async {
      const source = 'num f(num x) { x++; x--; ++x; --x; return x; }\n';
      final fixture = await AlgebraFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      noErrors(unit);
      final scan = const IncrementDecrementReplacement().discover(unit, source);
      expect(scan.candidates.map((candidate) => original(source, candidate)), [
        '++',
        '--',
        '++',
        '--',
      ]);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '--',
        '++',
        '--',
        '++',
      ]);
      await validMutants(fixture, source, scan.candidates);
    },
  );

  test('compound assignment replacement covers all five declared mappings', () async {
    const source =
        'num f(num x, num y) { x += y; x -= y; x *= y; x /= y; x %= y; return x; }\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const CompoundAssignmentReplacement().discover(unit, source);
    expect(scan.candidates.map((candidate) => original(source, candidate)), [
      '+=',
      '-=',
      '*=',
      '/=',
      '%=',
    ]);
    expect(scan.candidates.map((candidate) => candidate.replacement), [
      '-=',
      '+=',
      '/=',
      '*=',
      '*=',
    ]);
    await validMutants(fixture, source, scan.candidates);
  });

  test('promoted nullable SDK values and prefixed SDK types retain algebra support', () async {
    const source = '''
import 'dart:core';
import 'dart:core' as sdk;
sdk.num f(sdk.num? x, sdk.num y) {
 if (x == null) return y;
 x += y;
 x++;
 return x + y;
}
sdk.int bits(sdk.int? x, sdk.int y) {
 if (x == null) return y;
 return x << y;
}
''';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    for (final operator in const [
      ArithmeticOperatorReplacement(),
      CompoundAssignmentReplacement(),
      IncrementDecrementReplacement(),
      ShiftOperatorReplacement(),
    ]) {
      final scan = operator.discover(unit, source);
      expect(scan.candidates, isNotEmpty, reason: operator.id);
      await validMutants(fixture, source, scan.candidates);
    }
  });

  test('result type changes remain detectable as invalid by real static validation', () async {
    const source =
        'int f(int a, int b) => a * b;\nint g(int x) { x *= 2; return x; }\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    for (final operator in const [
      ArithmeticOperatorReplacement(),
      CompoundAssignmentReplacement(),
    ]) {
      final candidate = operator.discover(unit, source).candidates.first;
      final changed = await fixture.resolve(apply(source, candidate));
      expect(
        changed.diagnostics.where(
          (diagnostic) => diagnostic.severity == Severity.error,
        ),
        isNotEmpty,
      );
      noErrors(await fixture.resolve(source));
    }
  });

  test('numeric literal replacements compute values and remove unchanged/duplicate values', () async {
    const source =
        'num f() => 10;\nnum negative() => -10;\nnum zero() => 0;\nnum one() => 1;\nnum decimal() => 2.5;\nnum hex() => 0xA;\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const NumericLiteralReplacement().discover(unit, source);
    final byOriginal = <String, List<String>>{};
    for (final candidate in scan.candidates)
      byOriginal
          .putIfAbsent(original(source, candidate), () => [])
          .add(candidate.replacement);
    expect(byOriginal['10'], ['(0)', '(1)', '(-1)', '(11)', '(9)', '(-10)']);
    expect(byOriginal['-10'], ['(0)', '(1)', '(-1)', '(-9)', '(-11)', '(10)']);
    expect(byOriginal['0'], ['(1)', '(-1)']);
    expect(byOriginal['1'], ['(0)', '(-1)', '(2)']);
    expect(byOriginal['2.5'], [
      '(0.0)',
      '(1.0)',
      '(-1.0)',
      '(3.5)',
      '(1.5)',
      '(-2.5)',
    ]);
    expect(byOriginal['0xA'], byOriginal['10']);
    await validMutants(fixture, source, scan.candidates);
  });

  test('numeric literal signed zero and finite magnitude variants preserve double types', () async {
    const source =
        'double zero() => 0.0;\ndouble negative() => -0.0;\ndouble large() => 1e300;\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const NumericLiteralReplacement().discover(unit, source);
    final positive = scan.candidates
        .where((candidate) => original(source, candidate) == '0.0')
        .toList();
    expect(positive.map((candidate) => candidate.replacement), [
      '(1.0)',
      '(-1.0)',
      '(-0.0)',
    ]);
    expect(
      scan.candidates.every(
        (candidate) =>
            !candidate.replacement.contains('Infinity') &&
            !candidate.replacement.contains('NaN'),
      ),
      isTrue,
    );
    await validMutants(fixture, source, scan.candidates);
    final sign = positive.singleWhere(
      (candidate) => candidate.replacement == '(-0.0)',
    );
    expect(
      await fixture.execute(
        '${apply(source, sign)}\nvoid main() { print(1 / zero()); }\n',
      ),
      ['-Infinity'],
    );
  });

  test('numeric literals use SDK parsers for int extrema and context-converted double signed zero', () async {
    const source = '''
int minimum() => -9223372036854775808;
int maximum() => 9223372036854775807;
double integerAsDouble() => 1;
double signedIntegerZero() => -0;
''';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const NumericLiteralReplacement().discover(unit, source);
    final minimum = scan.candidates
        .where(
          (candidate) => original(source, candidate) == '-9223372036854775808',
        )
        .toList();
    expect(minimum.map((candidate) => candidate.replacement), [
      '(0)',
      '(1)',
      '(-1)',
      '(-9223372036854775807)',
      '(9223372036854775807)',
    ]);
    final maximum = scan.candidates
        .where(
          (candidate) => original(source, candidate) == '9223372036854775807',
        )
        .toList();
    expect(
      maximum.map((candidate) => candidate.replacement),
      contains('(-9223372036854775808)'),
    );
    expect(
      scan.candidates
          .where((candidate) => original(source, candidate) == '1')
          .map((candidate) => candidate.replacement),
      ['(0.0)', '(-1.0)', '(2.0)'],
    );
    expect(
      scan.candidates
          .where((candidate) => original(source, candidate) == '-0')
          .map((candidate) => candidate.replacement),
      ['(0.0)', '(1.0)', '(-1.0)'],
    );
    await validMutants(fixture, source, scan.candidates);
  });

  test('logical operand removal covers && and ||, deduplicates equal operands and detects lost promotion', () async {
    const source =
        'bool f(bool a, bool b) => a && b;\nbool g(bool a, bool b) => a || b;\nbool same(bool a) => a && a;\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final scan = const LogicalOperandRemoval().discover(unit, source);
    expect(scan.candidates.map((candidate) => candidate.replacement), [
      '(a)',
      '(b)',
      '(a)',
      '(b)',
      '(a)',
    ]);
    await validMutants(fixture, source, scan.candidates);
    const promoted = 'bool f(String? x) => x != null && x.isNotEmpty;\n';
    final promotedUnit = await fixture.resolve(promoted);
    noErrors(promotedUnit);
    final last = const LogicalOperandRemoval()
        .discover(promotedUnit, promoted)
        .candidates
        .last;
    expect(
      (await fixture.resolve(apply(promoted, last))).diagnostics
          .where((diagnostic) => diagnostic.severity == Severity.error),
      isNotEmpty,
    );
    noErrors(await fixture.resolve(promoted));
  });

  test('bitwise and shift variants preserve mixed precedence and include Dart unsigned shift', () async {
    const source =
        'int bits(int a, int b, int c) => a & b | c ^ a;\nint shift(int a, int b) => (a << b) + (a >> b) + (a >>> b);\n';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final bits = const BitwiseOperatorReplacement().discover(unit, source);
    expect(bits.candidates, hasLength(4));
    expect(bits.candidates.first.replacement, '((a & b) & (c ^ a))');
    await validMutants(fixture, source, bits.candidates);
    final shift = const ShiftOperatorReplacement().discover(unit, source);
    expect(shift.candidates, hasLength(6));
    await validMutants(fixture, source, shift.candidates);
  });

  test(
    'all operators exclude const/default/metadata/pattern contexts',
    () async {
      const source = '''
class Mark { const Mark(num number, bool flag); }
@Mark(1 + 2, 1 < 2 && true)
class Holder { final field = 1 + 2; }
bool f(Object value, {num n = 10, bool flag = true}) {
 const number = 1 + 2;
 const bits = 2 << 1;
 const logical = true && false;
 if (value case int x when x > 1 && flag) return flag;
 return flag;
}
''';
      final fixture = await AlgebraFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      noErrors(unit);
      final guardStart = source.indexOf('if (');
      final guardEnd = source.indexOf('return flag;', guardStart);
      for (final operator in operators) {
        final scan = operator.discover(unit, source);
        expect(
          scan.candidates.every((candidate) => candidate.offset >= guardEnd),
          isTrue,
          reason: operator.id,
        );
      }
    },
  );

  test('runtime grouping distinguishes nonassociative arithmetic and evaluates operands once', () async {
    const source = '''
final trace = <String>[];
num read(String name, num value) { trace.add(name); return value; }
num subject() => read('a', 20) - read('b', 3) * read('c', 4);
void main() { print(subject()); print(trace.join(',')); }
''';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final candidates = const ArithmeticOperatorReplacement()
        .discover(unit, source)
        .candidates;
    final inner = candidates.firstWhere(
      (candidate) =>
          original(source, candidate) == "read('b', 3) * read('c', 4)" &&
          candidate.replacement.contains(' + '),
    );
    expect(await fixture.execute(apply(source, inner)), ['13', 'a,b,c']);
    final tokenOnly = source.replaceFirst(
      "read('b', 3) * read('c', 4)",
      "read('b', 3) + read('c', 4)",
    );
    expect(await fixture.execute(tokenOnly), ['21', 'a,b,c']);
    final removed = const ArithmeticOperandRemoval()
        .discover(unit, source)
        .candidates
        .firstWhere(
          (candidate) =>
              original(source, candidate).startsWith("read('a', 20) -") &&
              candidate.replacement == "(read('a', 20))",
        );
    expect(await fixture.execute(apply(source, removed)), ['20', 'a']);
  });

  test('compound assignment retains getter, right-hand side and setter evaluation exactly once', () async {
    const source = '''
final trace = <String>[];
class Box {
 num stored = 10;
 num get value { trace.add('get'); return stored; }
 set value(num next) { trace.add('set'); stored = next; }
}
num rhs() { trace.add('rhs'); return 3; }
void main() {
 final box = Box();
 box.value += rhs();
 print(box.stored);
 print(trace.join(','));
}
''';
    final fixture = await AlgebraFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    noErrors(unit);
    final candidate = const CompoundAssignmentReplacement()
        .discover(unit, source)
        .candidates
        .single;
    expect(await fixture.execute(apply(source, candidate)), [
      '7',
      'get,rhs,set',
    ]);
    await validMutants(fixture, source, [candidate]);
  });

  test('increment token swaps preserve postfix old-value and prefix new-value semantics', () async {
    for (final token in ['x++', '++x', 'x--', '--x']) {
      final source = 'void main() { num x = 3; print($token); print(x); }\n';
      final fixture = await AlgebraFixture.open(source);
      try {
        final unit = await fixture.resolve(source);
        noErrors(unit);
        final candidate = const IncrementDecrementReplacement()
            .discover(unit, source)
            .candidates
            .single;
        final next = token.contains('++') ? '2' : '4';
        expect(await fixture.execute(apply(source, candidate)), [
          token.startsWith('x') ? '3' : next,
          next,
        ]);
        await validMutants(fixture, source, [candidate]);
      } finally {
        await fixture.close();
      }
    }
  });

  test(
    'runtime logical removal keeps only the selected operand effects',
    () async {
      const source = '''
final trace = <String>[];
bool read(String name, bool value) { trace.add(name); return value; }
bool subject() => read('a', false) && read('b', true);
void main() { print(subject()); print(trace.join(',')); }
''';
      final fixture = await AlgebraFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      noErrors(unit);
      final candidates = const LogicalOperandRemoval()
          .discover(unit, source)
          .candidates;
      expect(await fixture.execute(apply(source, candidates[0])), [
        'false',
        'a',
      ]);
      expect(await fixture.execute(apply(source, candidates[1])), [
        'true',
        'b',
      ]);
    },
  );
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

Future<void> validMutants(
  AlgebraFixture fixture,
  String source,
  Iterable<CandidateDraft> candidates,
) async {
  for (final candidate in candidates) {
    expect(candidate.length, greaterThan(0));
    expect(candidate.family, MutationFamily.classic);
    final changed = apply(source, candidate);
    expect(
      changed.substring(0, candidate.offset),
      source.substring(0, candidate.offset),
    );
    expect(
      changed.substring(candidate.offset + candidate.replacement.length),
      source.substring(candidate.offset + candidate.length),
    );
    noErrors(await fixture.resolve(changed));
    noErrors(await fixture.resolve(source));
  }
  expect(await File(fixture.path).readAsBytes(), utf8.encode(source));
}

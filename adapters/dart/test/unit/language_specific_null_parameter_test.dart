import 'dart:io';

import 'package:failforge_dart_adapter/src/operators/language_specific/dart/common.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/null_safety/null_safety.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/parameters/parameters.dart';
import 'package:test/test.dart';

import '../support/call_operator_fixture.dart';

Future<(CallOperatorFixture, OperatorScan)> scan(
  String source,
  LanguageSpecificOperator operator,
) async {
  final fixture = CallOperatorFixture.open(source);
  addTearDown(fixture.close);
  final unit = await fixture.resolve();
  CallOperatorFixture.noErrors(unit);
  return (fixture, operator.discover(unit, source));
}

void main() {
  test('catalogue identities, language, family and categories are stable', () {
    expect(nullSafetyOperators.map((operator) => operator.id), [
      'NSM-001',
      'NSM-002',
      'NSM-003',
      'NSM-004',
    ]);
    expect(parameterOperators.map((operator) => operator.id), [
      'DPM-001',
      'DPM-002',
      'DPM-003',
    ]);
    for (final operator in [...nullSafetyOperators, ...parameterOperators]) {
      expect(operator.family, MutationFamily.languageSpecific);
      expect(operator.language, 'dart');
      expect(operator.version, 1);
    }
    expect(
      nullSafetyOperators.every(
        (operator) => operator.category == LanguageSpecificCategory.nullSafety,
      ),
      isTrue,
    );
    expect(
      parameterOperators.every(
        (operator) => operator.category == LanguageSpecificCategory.parameters,
      ),
      isTrue,
    );
  });

  group('NSM-001', () {
    test('removes the fallback without duplicating a call or changing outer trivia', () async {
      const source = '''
String? read() => null;
String? f() => read() /* receiver */ ?? /* fallback */ 'default';
''';
      final (fixture, result) = await scan(
        source,
        const NullCoalescingFallbackRemoval(),
      );
      expect(result.candidates, hasLength(1));
      final candidate = result.candidates.single;
      expect(
        fixture.before(candidate),
        " /* receiver */ ?? /* fallback */ 'default'",
      );
      expect(fixture.apply(candidate), contains('String? f() => read();'));
      expect(candidate.replacement, isEmpty);
      await fixture.validateAll(result.candidates);
    });

    test('handles nested coalescing and generic nullable values', () async {
      const source = 'T? f<T>(T? a, T? b, T c) => a ?? b ?? c;\n';
      final (fixture, result) = await scan(
        source,
        const NullCoalescingFallbackRemoval(),
      );
      expect(result.candidates, hasLength(2));
      await fixture.validateAll(result.candidates);
    });

    test('skips a statically redundant non-nullable fallback', () async {
      const source = 'int f(int a) => a ?? 9;\n';
      final (_, result) = await scan(
        source,
        const NullCoalescingFallbackRemoval(),
      );
      expect(result.candidates, isEmpty);
      expect(result.skips.single.code, 'nonnullable_coalescing_receiver');
    });

    test('nullable constant contexts remain legal', () async {
      const source = "const String? value = null ?? 'fallback';\n";
      final (fixture, result) = await scan(
        source,
        const NullCoalescingFallbackRemoval(),
      );
      expect(result.candidates, hasLength(1));
      await fixture.validateAll(result.candidates);
    });
  });

  group('NSM-002', () {
    test(
      'replaces dynamic property and method access tokens independently',
      () async {
        const source = '''
dynamic f(dynamic value) => value /* left */ ?. /* right */ name;
dynamic g(dynamic value) => value?.call(1);
''';
        final (fixture, result) = await scan(
          source,
          const NullAwareAccessReplacement(),
        );
        expect(result.candidates, hasLength(2));
        expect(result.candidates.map(fixture.before), ['?.', '?.']);
        expect(result.candidates.map((candidate) => candidate.replacement), [
          '.',
          '.',
        ]);
        expect(
          fixture.apply(result.candidates.first),
          contains('/* left */ . /* right */'),
        );
        await fixture.validateAll(result.candidates);
      },
    );

    test(
      'supports dynamic null-aware index without adding an assertion',
      () async {
        const source = 'dynamic f(dynamic value) => value?[0];\n';
        final (fixture, result) = await scan(
          source,
          const NullAwareAccessReplacement(),
        );
        expect(result.candidates, hasLength(1));
        expect(fixture.before(result.candidates.single), '?');
        expect(fixture.apply(result.candidates.single), contains('value[0]'));
        expect(fixture.apply(result.candidates.single), isNot(contains('!')));
        await fixture.validateAll(result.candidates);
      },
    );

    test(
      'skips nullable typed access, redundant access and cascades',
      () async {
        const source = '''
int? f(String? value) => value?.length;
int? g(String value) => value?.length;
dynamic h(dynamic value) => value?..call();
int? i(List<int>? value) => value?[0];
''';
        final (_, result) = await scan(
          source,
          const NullAwareAccessReplacement(),
        );
        expect(result.candidates, isEmpty);
        expect(result.skips, hasLength(4));
      },
    );

    test(
      'finds dynamic accesses nested in closures and expression chains',
      () async {
        const source =
            'dynamic f(dynamic value) => (() => value?.child?.name)();\n';
        final (fixture, result) = await scan(
          source,
          const NullAwareAccessReplacement(),
        );
        expect(result.candidates, hasLength(2));
        await fixture.validateAll(result.candidates);
      },
    );
  });

  group('NSM-003', () {
    test(
      'does not replace a resolved null-valued alias with null again',
      () async {
        const source = '''
const nothing = null;
String? f() { String? value = nothing; value = nothing; return value; }
''';
        final (_, result) = await scan(
          source,
          const NullableValueReplacement(),
        );
        expect(result.candidates, isEmpty);
      },
    );

    test(
      'mutates nullable initializers and assignments, including field setters',
      () async {
        const source = '''
class Holder { String? value = 'field'; }
String? f(Holder h) {
  String? value = 'initial';
  value = 'second';
  h.value = 'third';
  return value;
}
''';
        final (fixture, result) = await scan(
          source,
          const NullableValueReplacement(),
        );
        expect(result.candidates, hasLength(4));
        expect(result.candidates.map(fixture.before), [
          "'field'",
          "'initial'",
          "'second'",
          "'third'",
        ]);
        expect(
          result.candidates.every(
            (candidate) => candidate.replacement == 'null',
          ),
          isTrue,
        );
        await fixture.validateAll(result.candidates);
      },
    );

    test('uses inferred storage types, not the initializer spelling', () async {
      const source = '''
String? f(String? source) {
  var nullable = source;
  var nonnullable = 'value';
  return nullable ?? nonnullable;
}
''';
      final (fixture, result) = await scan(
        source,
        const NullableValueReplacement(),
      );
      expect(result.candidates, hasLength(1));
      expect(fixture.before(result.candidates.single), 'source');
      await fixture.validateAll(result.candidates);
    });

    test(
      'supports generics, nullable function values and const fields',
      () async {
        const source = '''
const String? constant = 'constant';
T? f<T>(T input) { T? value = input; return value; }
int? Function()? callback = () => 7;
''';
        final (fixture, result) = await scan(
          source,
          const NullableValueReplacement(),
        );
        expect(result.candidates, hasLength(3));
        await fixture.validateAll(result.candidates);
      },
    );

    test(
      'does not replace null, non-null storage or compound assignment values',
      () async {
        const source = '''
void f() {
  String? nullable = (null);
  String value = 'text';
  value = 'again';
  int? number = null;
  number ??= 1;
}
''';
        final (_, result) = await scan(
          source,
          const NullableValueReplacement(),
        );
        expect(result.candidates, isEmpty);
      },
    );
  });

  group('NSM-004', () {
    test('changes only the assignment token and retains comments', () async {
      const source = '''
int? f(int? value) {
  value /* before */ ??= /* after */ 7;
  return value;
}
''';
      final (fixture, result) = await scan(
        source,
        const NullAwareAssignmentMutation(),
      );
      expect(result.candidates, hasLength(1));
      expect(fixture.before(result.candidates.single), '??=');
      expect(
        fixture.apply(result.candidates.single),
        contains('/* before */ = /* after */'),
      );
      await fixture.validateAll(result.candidates);
    });

    test('works in nested functions and generic write contexts', () async {
      const source = '''
T? f<T>(T? value, T fallback) {
  void update() { value ??= fallback; }
  update();
  return value;
}
''';
      final (fixture, result) = await scan(
        source,
        const NullAwareAssignmentMutation(),
      );
      expect(result.candidates, hasLength(1));
      await fixture.validateAll(result.candidates);
    });

    test('does not mutate ordinary or other compound assignments', () async {
      const source = 'void f(int value) { value = 7; value += 1; }\n';
      final (_, result) = await scan(
        source,
        const NullAwareAssignmentMutation(),
      );
      expect(result.candidates, isEmpty);
    });
  });

  group('DPM-001', () {
    test('changes typed named and positional constants without touching separators', () async {
      const source = '''
void f({int timeout = /* keep */ 30, double factor = 2.5, String label = 'hello'}) {}
void g([num count = 7]) {}
''';
      final (fixture, result) = await scan(
        source,
        const DefaultParameterReplacement(),
      );
      expect(result.candidates, isNotEmpty);
      expect(result.candidates.map(fixture.before).toSet(), {
        '30',
        '2.5',
        "'hello'",
        '7',
      });
      expect(
        result.candidates.any(
          (candidate) =>
              fixture.before(candidate) == '30' && candidate.replacement == '0',
        ),
        isTrue,
      );
      expect(
        result.candidates.any(
          (candidate) =>
              fixture.before(candidate) == '2.5' &&
              candidate.replacement == '0.0',
        ),
        isTrue,
      );
      expect(fixture.apply(result.candidates.first), contains('= /* keep */'));
      await fixture.validateAll(result.candidates);
    });

    test(
      'supports field and super formals using their resolved types',
      () async {
        const source = '''
class Base { Base({this.count = 7}); final int count; }
class Child extends Base { Child({super.count = 9}); }
''';
        final (fixture, result) = await scan(
          source,
          const DefaultParameterReplacement(),
        );
        expect(result.candidates.map(fixture.before).toSet(), {'7', '9'});
        await fixture.validateAll(result.candidates);
      },
    );

    test('constant aliases and alternate literal spellings do not create known no-ops', () async {
      const source = '''
const zero = 0;
void f({int a = zero, int b = (0x0), String c = r'', double d = 0.0}) {}
''';
      final (fixture, result) = await scan(
        source,
        const DefaultParameterReplacement(),
      );
      for (final candidate in result.candidates) {
        final original = fixture.before(candidate);
        if (original == 'zero' || original == '(0x0)') {
          expect(candidate.replacement, isNot('0'));
        }
        if (original == "r''") expect(candidate.replacement, isNot("''"));
        if (original == '0.0') expect(candidate.replacement, isNot('0.0'));
      }
      await fixture.validateAll(result.candidates);
    });

    test(
      'does not invent constants for enum, function or generic types',
      () async {
        const source = '''
enum State { ready, failed }
void action() {}
void f<T>({State state = State.ready, void Function() callback = action, T? value = null}) {}
''';
        final (_, result) = await scan(
          source,
          const DefaultParameterReplacement(),
        );
        expect(result.candidates, isEmpty);
      },
    );
  });

  group('DPM-002', () {
    test(
      'inverts boolean defaults including constants and nullable booleans',
      () async {
        const source = '''
const enabled = true;
void f({bool first = true, bool second = false, bool? third = (enabled)}) {}
''';
        final (fixture, result) = await scan(
          source,
          const BooleanDefaultInversion(),
        );
        expect(result.candidates.map((candidate) => candidate.replacement), [
          'false',
          'true',
          'false',
        ]);
        await fixture.validateAll(result.candidates);
      },
    );

    test('does not mutate body booleans, implicit defaults or null defaults', () async {
      const source =
          'void f({bool? value, bool? other = null}) { final flag = true; }\n';
      final (_, result) = await scan(source, const BooleanDefaultInversion());
      expect(result.candidates, isEmpty);
    });
  });

  group('DPM-003', () {
    test(
      'replaces only defaults whose resolved parameter accepts null',
      () async {
        const source = '''
void action() {}
void f<T>({String? label = 'hi', List<int>? values = const [1],
    void Function()? callback = action, int nonnullable = 7, T? generic = null}) {}
''';
        final (fixture, result) = await scan(
          source,
          const NullableDefaultReplacement(),
        );
        expect(result.candidates.map(fixture.before), [
          "'hi'",
          'const [1]',
          'action',
        ]);
        expect(
          result.candidates.every(
            (candidate) => candidate.replacement == 'null',
          ),
          isTrue,
        );
        await fixture.validateAll(result.candidates);
      },
    );

    test('does not emit an edit for implicit or parenthesized null', () async {
      const source =
          'void f({String? first, Object? second = (null), int value = 1}) {}\n';
      final (_, result) = await scan(
        source,
        const NullableDefaultReplacement(),
      );
      expect(result.candidates, isEmpty);
    });
  });

  test('discovery preserves disk bytes and repeating a scan produces the same exact edits', () async {
    const source =
        "void f({int? count = 30}) { String? value = 'hello'; value ??= 'fallback'; }\n";
    final fixture = CallOperatorFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve();
    CallOperatorFixture.noErrors(unit);
    for (final operator in [...nullSafetyOperators, ...parameterOperators]) {
      List<(int, int, String)> edits() => operator
          .discover(unit, source)
          .candidates
          .map(
            (candidate) =>
                (candidate.offset, candidate.length, candidate.replacement),
          )
          .toList();
      expect(edits(), edits());
    }
    expect(await File(fixture.path).readAsString(), source);
  });
}

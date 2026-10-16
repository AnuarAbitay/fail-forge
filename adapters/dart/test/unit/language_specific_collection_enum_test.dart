import 'dart:io';

import 'package:failforge_dart_adapter/src/operators/language_specific/dart/collections/collections.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/common.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/enums/enums.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/null_safety/null_aware_spread_removal.dart';
import 'package:test/test.dart';

import '../support/control_flow_fixture.dart';

void main() {
  Future<ControlFlowFixture> fixture(String source) async {
    final value = await ControlFlowFixture.open(source);
    addTearDown(value.close);
    return value;
  }

  Future<void> valid(
    ControlFlowFixture f,
    String source,
    Iterable<CandidateDraft> candidates,
  ) async {
    for (final candidate in candidates) {
      expect(candidate.family, MutationFamily.languageSpecific);
      expect(candidate.length, greaterThan(0));
      expect(original(source, candidate), isNot(candidate.replacement));
      final changed = apply(source, candidate);
      expect(
        changed.substring(0, candidate.offset),
        source.substring(0, candidate.offset),
      );
      expect(
        changed.substring(candidate.offset + candidate.replacement.length),
        source.substring(candidate.offset + candidate.length),
      );
      expectNoErrors(await f.resolve(changed));
      expectNoErrors(await f.resolve(source));
    }
    expect(await File(f.path).readAsString(), source);
  }

  test('six Dart operators declare exact family, IDs and categories', () {
    const operators = [
      ...collectionOperators,
      ...enumOperators,
      ...nullAwareSpreadOperators,
    ];
    expect(operators.map((operator) => operator.id), [
      'CLM-001',
      'CLM-002',
      'CLM-003',
      'CLM-004',
      'ENM-001',
      'NSM-005',
    ]);
    expect(
      operators.every(
        (operator) =>
            operator.family == MutationFamily.languageSpecific &&
            operator.language == 'dart',
      ),
      isTrue,
    );
    expect(operators.map((operator) => operator.category), [
      ...List.filled(4, LanguageSpecificCategory.collections),
      LanguageSpecificCategory.enums,
      LanguageSpecificCategory.nullSafety,
    ]);
  });

  group('CLM-001', () {
    test('inverts only condition while preserving nested elements and comments', () async {
      const source =
          'List<List<int>> f(bool a, bool b) => [if (a /*keep*/) [if (b) 1 else 2]];';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionIfInversion(), source);
      expect(scan.candidates.map((candidate) => original(source, candidate)), [
        'a',
        'b',
      ]);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '!(a)',
        '!(b)',
      ]);
      expect(apply(source, scan.candidates.first), contains('!(a) /*keep*/'));
      await valid(f, source, scan.candidates);
    });

    test('supports constant lists, sets and maps with ordinary conditions', () async {
      const source =
          'const a = [if (true) 1]; const b = {if (false) 2}; const c = {if (true) 1: 2};';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionIfInversion(), source);
      expect(scan.candidates, hasLength(3));
      await valid(f, source, scan.candidates);
    });

    test('dynamic and pattern conditions do not become boolean negations', () async {
      const source =
          'List<int> f(dynamic gate, Object value) => [if (gate) 1, if (value case int n) n];';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionIfInversion(), source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips.map((skip) => skip.code), [
        'unsupported_condition_type',
        'pattern_condition',
      ]);
    });

    test(
      'semantic overlay must reject inversion that loses flow promotion',
      () async {
        const source =
            'List<int> f(String? value) => [if (value != null) value.length];';
        final f = await fixture(source);
        final scan = await f.scan(const CollectionIfInversion(), source);
        expect(scan.candidates, hasLength(1));
        expect(
          errors(await f.resolve(apply(source, scan.candidates.single))),
          isNotEmpty,
        );
      },
    );
  });

  group('CLM-002', () {
    test('removes first, middle, last and sole spreads without deleting trivia', () async {
      const source =
          'List<List<int>> f(List<int> xs) => [[/*before*/...xs/*after*/,/*next*/1], [1,...xs,2], [1,...xs], [...xs,]];';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionSpreadRemoval(), source);
      expect(scan.candidates, hasLength(4));
      expect(
        apply(source, scan.candidates.first),
        contains('[/*before*//*after*//*next*/1]'),
      );
      expect(
        scan.candidates.every(
          (candidate) =>
              candidate.replacement.isEmpty ||
              candidate.replacement == '/*after*/',
        ),
        isTrue,
      );
      await valid(f, source, scan.candidates);
    });

    test('preserves set versus map and inferred nullable generic type arguments', () async {
      const source =
          'Set<T?> f<T>(Set<T?> values) => {...values}; Map<String,T?> g<T>(Map<String,T?> values) => {...values}; const a = {...<int>{1}}; const b = {...<String,int>{"a": 1}};';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionSpreadRemoval(), source);
      expect(scan.candidates, hasLength(4));
      final changed = scan.candidates
          .map((candidate) => apply(source, candidate))
          .toList();
      expect(changed[0], contains('=> <T?>{}'));
      expect(changed[1], contains('=> <String, T?>{}'));
      expect(changed[2], contains('const a = <int>{}'));
      expect(changed[3], contains('const b = <String, int>{}'));
      await valid(f, source, scan.candidates);
    });

    test('never invents a type name shadowed by a local value', () async {
      const source =
          'Set<T> f<T>(Set<T> values) { final T = 1; print(T); return {...values}; }';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionSpreadRemoval(), source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips.single.code, 'collection_type_unavailable');
    });

    test('skips wrapped spread bodies and handles spreads in nested literals', () async {
      const source =
          'List<int> f(bool b, List<int> xs) => [if (b) ...xs]; List<List<int>> g(List<int> xs) => [[...xs]]; List<int> h(List<int>? xs) => [...?xs];';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionSpreadRemoval(), source);
      expect(scan.candidates, hasLength(1));
      expect(scan.skips.single.code, 'nested_collection_element');
      await valid(f, source, scan.candidates);
    });
  });

  group('CLM-003 and CLM-004', () {
    test('deletes the entire if/else or if-case element including local bindings', () async {
      const source =
          'List<int> f(bool a, bool b, Object value) => [0, if (a) 1 else 2, if (value case int n) n, if (a) if (b) 3];';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionIfElementRemoval(), source);
      expect(scan.candidates, hasLength(3));
      expect(
        scan.candidates.map((candidate) => original(source, candidate)),
        contains('if (a) 1 else 2,'),
      );
      expect(scan.skips.single.code, 'nested_collection_element');
      await valid(f, source, scan.candidates);
    });

    test('deletes for-in and classic for including complete nested bodies', () async {
      const source =
          'List<int> f(List<int> xs) => [for (final x in xs) if (x > 0) x, for (var i=0; i<2; i++) i]; Set<int> g() => {for (var i=0; i<2; i++) i}; Map<int,int> h() => {for (var i=0; i<2; i++) i:i};';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionForRemoval(), source);
      expect(scan.candidates, hasLength(4));
      await valid(f, source, scan.candidates);
    });

    test('ordinary if and for statements are outside collection operators', () async {
      const source =
          'void f(bool b) { if (b) print(1); for (var i=0; i<2; i++) print(i); }';
      final f = await fixture(source);
      for (final operator in collectionOperators) {
        expect((await f.scan(operator, source)).candidates, isEmpty);
      }
    });
  });

  group('NSM-005', () {
    test('removes only nullable spread, preserving regular spread and siblings', () async {
      const source =
          'List<int> f(List<int>? optional, List<int> required) => [1, ...?optional /*keep*/, ...required, 2]; Set<T?> g<T>(Set<T?>? xs) => {...?xs}; Map<String,int> h(Map<String,int>? xs) => {...?xs};';
      final f = await fixture(source);
      final scan = await f.scan(const NullAwareSpreadRemoval(), source);
      expect(scan.candidates, hasLength(3));
      expect(
        apply(source, scan.candidates.first),
        contains('[1,  /*keep*/ ...required, 2]'),
      );
      await valid(f, source, scan.candidates);
      final classical = await f.scan(const CollectionSpreadRemoval(), source);
      expect(classical.candidates, hasLength(1));
      expect(original(source, classical.candidates.single), '...required,');
    });

    test(
      'nullable spread with a nonnullable source still uses the correct ID',
      () async {
        const source =
            'const a = [...?<int>[1],]; List<int> f(List<int> xs) => [...?xs];';
        final f = await fixture(source);
        final scan = await f.scan(const NullAwareSpreadRemoval(), source);
        expect(scan.candidates, hasLength(2));
        await valid(f, source, scan.candidates);
      },
    );
  });

  group('ENM-001', () {
    test('uses only constants of the resolved enum and preserves qualifiers and trivia', () async {
      const source =
          'enum State { ready, failed, closed } enum Other { ready, failed } State f() => State./*keep*/ready; Other g() => Other.ready;';
      final f = await fixture(source);
      final scan = await f.scan(const EnumValueReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'failed',
        'closed',
        'failed',
      ]);
      expect(
        scan.candidates.every(
          (candidate) => original(source, candidate) == 'ready',
        ),
        isTrue,
      );
      expect(
        apply(source, scan.candidates.first),
        contains('State./*keep*/failed'),
      );
      await valid(f, source, scan.candidates);
    });

    test('keeps imported prefixes and excludes inaccessible private constants', () async {
      const source =
          "import 'remote.dart' as remote; remote.State f() => remote.State.ready;";
      final f = await fixture(source);
      await File('${f.directory.path}/lib/remote.dart')
          .writeAsString('enum State { ready, failed, _hidden }');
      final scan = await f.scan(const EnumValueReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'failed',
      ]);
      expect(
        apply(source, scan.candidates.single),
        contains('remote.State.failed'),
      );
      await valid(f, source, scan.candidates);
    });

    test('generic enhanced enums reject donors incompatible with the original value type', () async {
      const source =
          'enum Box<T> { a<int>(), b<int>(), c<String>(); const Box(); } Box<int> f() => Box.a;';
      final f = await fixture(source);
      final scan = await f.scan(const EnumValueReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), ['b']);
      await valid(f, source, scan.candidates);
    });

    test('unqualified enum use cannot be replaced by a shadowing local', () async {
      const source =
          'enum State { a, b, c; State pick() { final b = 1; print(b); return a; } }';
      final f = await fixture(source);
      final scan = await f.scan(const EnumValueReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), ['c']);
      expect(scan.skips.single.code, 'shadowed_enum_value');
      await valid(f, source, scan.candidates);
    });

    test('supports constant contexts and dot shorthand without modifying definitions', () async {
      const source =
          'enum State { a, b } const State value = State.a; State f() => .a;';
      final f = await fixture(source);
      final scan = await f.scan(const EnumValueReplacement(), source);
      expect(scan.candidates, hasLength(2));
      for (final candidate in scan.candidates) {
        expect(apply(source, candidate), startsWith('enum State { a, b }'));
      }
      await valid(f, source, scan.candidates);
    });

    test('patterns, enum values list and non-enum static constants are skipped', () async {
      const source =
          'enum State { a, b } class Fake { static const a=1, b=2; } int f(State s) => switch(s) { State.a => Fake.a, State.b => State.values.length };';
      final f = await fixture(source);
      final scan = await f.scan(const EnumValueReplacement(), source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips.map((skip) => skip.code), [
        'enum_declaration_or_pattern',
        'enum_declaration_or_pattern',
      ]);
    });

    test('single-value enums produce no identical replacement', () async {
      const source = 'enum Single { value } Single f() => Single.value;';
      final f = await fixture(source);
      expect(
        (await f.scan(const EnumValueReplacement(), source)).candidates,
        isEmpty,
      );
    });
  });
}

import 'dart:io';

import 'package:failforge_dart_adapter/src/operators/language_specific/dart/common.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/patterns/patterns.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/switches/switches.dart';
import 'package:test/test.dart';

import '../support/control_flow_fixture.dart';

void main() {
  Future<ControlFlowFixture> fixture(String source) async {
    final result = await ControlFlowFixture.open(source);
    addTearDown(result.close);
    return result;
  }

  Future<void> valid(
    ControlFlowFixture f,
    String source,
    OperatorScan scan,
  ) async {
    for (final candidate in scan.candidates) {
      expect(candidate.family, MutationFamily.languageSpecific);
      expect(original(source, candidate), isNot(candidate.replacement));
      expectNoErrors(await f.resolve(apply(source, candidate)));
      expectNoErrors(await f.resolve(source));
    }
    expect(await File(f.path).readAsString(), source);
  }

  test('all six operators have stable IDs, Dart language and categories', () {
    final operators = [...switchOperators, ...patternOperators];
    expect(operators.map((op) => op.id), [
      'SWM-001',
      'SWM-002',
      'SWM-003',
      'PMM-001',
      'PMM-002',
      'PMM-003',
    ]);
    expect(
      operators.every(
        (op) =>
            op.family == MutationFamily.languageSpecific &&
            op.language == 'dart',
      ),
      isTrue,
    );
    expect(
      switchOperators.every(
        (op) => op.category == LanguageSpecificCategory.switches,
      ),
      isTrue,
    );
    expect(
      patternOperators.every(
        (op) => op.category == LanguageSpecificCategory.patterns,
      ),
      isTrue,
    );
  });

  group('SWM-001', () {
    test('exchanges direct return results without changing cases or comments', () async {
      const source =
          'enum E{a,b} int f(E x){switch(x){case E.a:return /*keep*/ 1;case E.b:return 2;}}';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchBranchResultReplacement(), source);
      expect(scan.candidates.map((c) => c.replacement), ['(2)', '(1)']);
      expect(scan.candidates.map((c) => original(source, c)), ['1', '2']);
      expect(
        apply(source, scan.candidates.first),
        contains('return /*keep*/ (2)'),
      );
      await valid(f, source, scan);
    });

    test('does not copy branch-local pattern bindings into another arm', () async {
      const source =
          "int f(Object x){switch(x){case [int a]:return a;case {'n':int b}:return b;default:return 0;}}";
      final f = await fixture(source);
      final scan = await f.scan(const SwitchBranchResultReplacement(), source);
      expect(scan.candidates, hasLength(2));
      expect(scan.candidates.every((c) => c.replacement == '(0)'), isTrue);
      expect(scan.skips.map((s) => s.code), contains('branch_local_donor'));
      await valid(f, source, scan);
    });

    test('supports outer generic donors and nested switch ownership', () async {
      const source =
          'List<T> f<T>(bool x,List<T> a,List<T> b){switch(x){case true:return a;case false:return b;}} int g(bool x,bool y){switch(x){case true:switch(y){case true:return 1;case false:return 2;}case false:return 3;}}';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchBranchResultReplacement(), source);
      expect(scan.candidates.map((c) => c.replacement), [
        '(b)',
        '(a)',
        '(2)',
        '(1)',
      ]);
      await valid(f, source, scan);
    });

    test('omits equal literal values and incompatible result types', () async {
      const source =
          "int a(int x){switch(x){case 0:return 0x1;default:return (1);}} Object b(bool x){switch(x){case true:return 1;case false:return 'no';}}";
      final f = await fixture(source);
      expect(
        (await f.scan(
          const SwitchBranchResultReplacement(),
          source,
        )).candidates,
        isEmpty,
      );
    });

    test('rejects closures and nontrivial statement bodies as donors', () async {
      const source =
          'Object f(bool x){switch(x){case true:return (int n)=>n;case false:return (int n)=>n+1;}} int g(bool x){switch(x){case true:print(x);return 1;case false:return 2;}}';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchBranchResultReplacement(), source);
      expect(scan.candidates, isEmpty);
      expect(
        scan.skips.map((s) => s.code),
        containsAll(['branch_local_donor', 'unsupported_switch_result']),
      );
    });
  });

  group('SWM-002', () {
    test('replaces one expression result while preserving patterns and exhaustiveness', () async {
      const source = 'int f(bool x)=>switch(x){true=>1,false=>2};';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchExpressionReplacement(), source);
      expect(scan.candidates.map((c) => c.replacement), ['(2)', '(1)']);
      await valid(f, source, scan);
    });

    test('keeps donor expression grouping, comments and generic types', () async {
      const source =
          'List<T> f<T>(bool x,List<T> a,List<T> b)=>switch(x){true=>a,false=>(/*donor*/b)}; int g(bool x,int a,int b)=>switch(x){true=>a+1,false=>b*2};';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchExpressionReplacement(), source);
      expect(scan.candidates.map((c) => c.replacement), [
        '((/*donor*/b))',
        '(a)',
        '(b*2)',
        '(a+1)',
      ]);
      await valid(f, source, scan);
    });

    test(
      'does not leak arm bindings even when their names and types coincide',
      () async {
        const source =
            'int f(Object x)=>switch(x){[int n]=>n,{"n":int n}=>n,_=>0};';
        final f = await fixture(source);
        final scan = await f.scan(const SwitchExpressionReplacement(), source);
        expect(scan.candidates, hasLength(2));
        expect(scan.candidates.every((c) => c.replacement == '(0)'), isTrue);
        await valid(f, source, scan);
      },
    );

    test('does not transplant branch-specific promotion of an outer value', () async {
      const source =
          'int f(bool x,int? n)=>switch(x){true=>n==null?0:n,false=>n==null?1:n+1};';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchExpressionReplacement(), source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips.map((s) => s.code), contains('branch_local_donor'));
    });
    test('allows null only for an assignable nullable result', () async {
      const source =
          'String? f(bool x,String? y)=>switch(x){true=>y,false=>null};';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchExpressionReplacement(), source);
      expect(scan.candidates.single.replacement, '(null)');
      await valid(f, source, scan);
    });
  });

  group('SWM-003 and PMM-002', () {
    test('constant literal guards cannot make a later branch unreachable', () async {
      const source =
          'int f(int x)=>switch(x){int n when false=>1,_=>2}; bool g(Object x){if(x case int n when ((true)))return n>0;return false;}';
      final f = await fixture(source);
      for (final operator in const [
        SwitchGuardMutation(),
        PatternGuardInversion(),
      ]) {
        final scan = await f.scan(operator, source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips.single.code, 'constant_guard');
      }
    });

    test('const aliases, static fields and nested NOT guards are excluded', () async {
      const source =
          'const flag=false;class Settings{static const enabled=true;} int f(int x)=>switch(x){int n when !(!((flag)))=>1,_=>2}; int h(int x)=>switch(x){int n when !(Settings.enabled)=>1,_=>2}; bool g(Object x){const local=flag;if(x case int n when !(!(local)))return n>0;return false;} bool j(Object x){if(x case int n when !(!Settings.enabled))return n>0;return false;}';
      final f = await fixture(source);
      for (final operator in const [
        SwitchGuardMutation(),
        PatternGuardInversion(),
      ]) {
        final scan = await f.scan(operator, source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips, hasLength(2));
        expect(
          scan.skips.every((skip) => skip.code == 'constant_guard'),
          isTrue,
        );
      }
    });

    test('known Boolean combinations are excluded but data-dependent guards remain', () async {
      const source =
          'int f(int x,bool flag)=>switch(x){int n when (false && flag)=>1,_=>2}; bool g(Object x,bool flag){if(x case int n when (flag || true))return n>0;return false;}';
      final f = await fixture(source);
      for (final operator in const [
        SwitchGuardMutation(),
        PatternGuardInversion(),
      ]) {
        final scan = await f.scan(operator, source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips.single.code, 'constant_guard');
      }
    });

    test('literal comparisons and arithmetic cannot bypass the constant guard exclusion', () async {
      const source =
          'int f(int x)=>switch(x){int n when 1==1=>1,_=>2}; int h(int x)=>switch(x){int n when 1>0=>1,_=>2}; bool g(Object x){if(x case int n when (2+3)==5)return n>0;return false;} bool j(Object x){if(x case int n when 1>0)return n>0;return false;}';
      final f = await fixture(source);
      for (final operator in const [
        SwitchGuardMutation(),
        PatternGuardInversion(),
      ]) {
        final scan = await f.scan(operator, source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips, hasLength(2));
        expect(
          scan.skips.every((skip) => skip.code == 'runtime_independent_guard'),
          isTrue,
        );
      }
    });

    test('runtime invocations and nonconstant local reads remain supported', () async {
      const source =
          'bool check(int n)=>n>0; int f(int x)=>switch(x){int n when check(n)=>1,_=>2}; bool g(Object x){final flag=x is int;if(x case int n when flag)return n>0;return false;}';
      final f = await fixture(source);
      for (final operator in const [
        SwitchGuardMutation(),
        PatternGuardInversion(),
      ]) {
        final scan = await f.scan(operator, source);
        expect(scan.candidates, hasLength(1));
        await valid(f, source, scan);
      }
    });

    test('switch and if-case guards are owned by different operators', () async {
      const source =
          'int f(Object x)=>switch(x){int n when n>0=>n,_=>0}; bool g(Object x){if(x case int n when n>0)return true;return false;}';
      final f = await fixture(source);
      final sw = await f.scan(const SwitchGuardMutation(), source);
      final pattern = await f.scan(const PatternGuardInversion(), source);
      expect(sw.candidates.single.replacement, '!(n>0)');
      expect(pattern.candidates.single.replacement, '!(n>0)');
      expect(
        sw.candidates.single.offset,
        lessThan(pattern.candidates.single.offset),
      );
      await valid(f, source, sw);
      await valid(f, source, pattern);
    });

    test('negates the complete bool guard with comments and nested expressions', () async {
      const source =
          'int f(Object x,bool flag)=>switch(x){int n when /*outside*/ n>0 && (flag || n==10)=>n,_=>0};';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchGuardMutation(), source);
      expect(scan.candidates.single.replacement, '!(n>0 && (flag || n==10))');
      expect(
        apply(source, scan.candidates.single),
        contains('when /*outside*/ !('),
      );
      await valid(f, source, scan);
    });

    test('supports guarded statement cases and collection if-case elements', () async {
      const source =
          'int f(Object x){switch(x){case int n when n>0:return n;default:return 0;}} List<T> g<T>(Object x,T item)=>[if(x case int n when n>0)item];';
      final f = await fixture(source);
      final sw = await f.scan(const SwitchGuardMutation(), source);
      final pattern = await f.scan(const PatternGuardInversion(), source);
      expect(sw.candidates, hasLength(1));
      expect(pattern.candidates, hasLength(1));
      await valid(f, source, sw);
      await valid(f, source, pattern);
    });

    test('unguarded patterns and ordinary conditions are excluded', () async {
      const source =
          'int f(Object x)=>switch(x){int n=>n,_=>0}; bool g(Object x){if(x case int n)return n>0; if(x==null)return false;return true;}';
      final f = await fixture(source);
      expect(
        (await f.scan(const SwitchGuardMutation(), source)).candidates,
        isEmpty,
      );
      expect(
        (await f.scan(const PatternGuardInversion(), source)).candidates,
        isEmpty,
      );
    });
    test('dynamic guard conditions are skipped rather than assumed bool', () async {
      const source =
          'int f(Object x,dynamic flag)=>switch(x){int n when flag=>n,_=>0}; bool g(Object x,dynamic flag){if(x case int n when flag)return n>0;return false;}';
      final f = await fixture(source);
      for (final operator in const [
        SwitchGuardMutation(),
        PatternGuardInversion(),
      ]) {
        final scan = await f.scan(operator, source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips.single.code, 'unsupported_guard_type');
      }
    });
  });

  group('PMM-001', () {
    test('complements all six SDK relational operators by exact token range', () async {
      const source =
          'bool f(int x){if(x case >0)return true;if(x case >=1)return true;if(x case <2)return true;if(x case <=3)return true;if(x case ==4)return true;if(x case !=5)return true;return false;}';
      final f = await fixture(source);
      final scan = await f.scan(const PatternConstraintMutation(), source);
      expect(scan.candidates.map((c) => original(source, c)), [
        '>',
        '>=',
        '<',
        '<=',
        '==',
        '!=',
      ]);
      expect(scan.candidates.map((c) => c.replacement), [
        '<=',
        '<',
        '>=',
        '>',
        '!=',
        '==',
      ]);
      await valid(f, source, scan);
    });

    test('handles nullable inputs, generic structures and nested commented patterns', () async {
      const source =
          'bool f<T>(Object? x)=>switch(x){[int() && > /*bound*/ 0,T()]=>true,_=>false};';
      final f = await fixture(source);
      final scan = await f.scan(const PatternConstraintMutation(), source);
      expect(scan.candidates, hasLength(1));
      expect(apply(source, scan.candidates.single), contains('<= /*bound*/ 0'));
      await valid(f, source, scan);
    });

    test('does not rewrite ordinary comparisons or custom relational overloads', () async {
      const source =
          'class C{const C();bool operator >(C other)=>true;} const c=C(); bool f(C x){if(x case >c)return true;return false;} bool g(int x)=>x>0;';
      final f = await fixture(source);
      final scan = await f.scan(const PatternConstraintMutation(), source);
      expect(scan.candidates, isEmpty);
      expect(
        scan.skips.map((s) => s.code),
        contains('unsupported_pattern_operator'),
      );
    });
  });

  group('PMM-003', () {
    test('switches AND/OR without bindings and preserves original grouping/comments', () async {
      const source =
          'bool f(List<int> x){if(x case [>0,_] /*left*/ && /*right*/ [_,>0])return true;return false;} bool g(List<int> x){if(x case [>0,_] || [_,>0])return true;return false;}';
      final f = await fixture(source);
      final scan = await f.scan(const LogicalPatternMutation(), source);
      expect(scan.candidates, hasLength(2));
      expect(
        scan.candidates.first.replacement,
        '(([>0,_]) /*left*/ || /*right*/ ([_,>0]))',
      );
      expect(scan.candidates.last.replacement, '(([>0,_]) && ([_,>0]))');
      await valid(f, source, scan);
    });

    test('skips OR bindings that AND would declare twice', () async {
      const source =
          'int f(Object x)=>switch(x){[int n] || {"n":int n}=>n,_=>0};';
      final f = await fixture(source);
      final scan = await f.scan(const LogicalPatternMutation(), source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips.single.code, 'logical_pattern_bindings');
    });

    test(
      'skips AND bindings that OR would fail to define on both alternatives',
      () async {
        const source = 'int f(Object x)=>switch(x){int n && >0=>n,_=>0};';
        final f = await fixture(source);
        final scan = await f.scan(const LogicalPatternMutation(), source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips.single.code, 'logical_pattern_bindings');
      },
    );

    test('does not mutate boolean expressions or single patterns', () async {
      const source =
          'bool f(bool a,bool b)=>a&&b; bool g(Object x)=>switch(x){int()=>true,_=>false};';
      final f = await fixture(source);
      expect(
        (await f.scan(const LogicalPatternMutation(), source)).candidates,
        isEmpty,
      );
    });
  });
}

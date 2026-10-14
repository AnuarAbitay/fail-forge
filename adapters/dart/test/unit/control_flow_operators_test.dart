import 'package:failforge_dart_adapter/src/operators/common.dart';
import 'package:failforge_dart_adapter/src/operators/classic/returns/return_value_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/condition_forcing.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/branch_body_removal.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/conditional_expression_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/statement_deletion.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/break_continue_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/switch_case_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/exceptions/throw_removal.dart';
import 'package:failforge_dart_adapter/src/operators/classic/exceptions/exception_replacement.dart';
import 'package:failforge_dart_adapter/src/operators/classic/collections/collection_literal_mutation.dart';
import 'package:test/test.dart';

import '../support/control_flow_fixture.dart';

void main() {
  Future<ControlFlowFixture> fixture(String source) async {
    final value = await ControlFlowFixture.open(source);
    addTearDown(value.close);
    return value;
  }

  test('ten operators have distinct classic IDs and the intended category', () {
    const operators = <ClassicOperator>[
      ReturnValueReplacement(),
      ConditionForcing(),
      BranchBodyRemoval(),
      ConditionalExpressionMutation(),
      StatementDeletion(),
      BreakContinueMutation(),
      SwitchCaseMutation(),
      ThrowRemoval(),
      ExceptionReplacement(),
      CollectionLiteralMutation(),
    ];
    expect(operators.map((operator) => operator.id).toSet(), hasLength(10));
    expect(
      operators.every((operator) => operator.family == MutationFamily.classic),
      isTrue,
    );
    expect(operators.map((operator) => operator.category), [
      ClassicalCategory.returns,
      ...List.filled(6, ClassicalCategory.controlFlow),
      ClassicalCategory.exceptions,
      ClassicalCategory.exceptions,
      ClassicalCategory.collections,
    ]);
  });

  group('RETURN_VALUE_REPLACEMENT', () {
    test('literal equivalents and already empty collections are not mutations', () async {
      const source =
          "bool a()=> (/*keep*/true);int b()=> (0x0);double c()=> (0e0);String d()=> (r'');List<int> e()=> (<int>[]);Set<String> f()=> <String>{};Map<String,int> g()=> <String,int>{};List<int>? h()=> <int>[];";
      final f = await fixture(source);
      final scan = await f.scan(const ReturnValueReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'false',
        '1',
        '-1',
        '1.0',
        '-1.0',
        'null',
      ]);
      expect(
        scan.skips.where((skip) => skip.code == 'unchanged_return_value'),
        hasLength(8),
      );
      await f.validMutants(source, scan.candidates);
    });
    test(
      'typed empty replacement rejects a generic parameter shadowed by a value',
      () async {
        const source =
            'List<T> f<T>(List<T> input){final T=1;print(T);return input;}';
        final f = await fixture(source);
        final scan = await f.scan(const ReturnValueReplacement(), source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips.single.code, 'unsupported_return_type');
        expect(
          errors(
            await f.resolve(
              source.replaceFirst('return input', 'return <T>[]'),
            ),
          ),
          isNotEmpty,
        );
      },
    );
    test('chooses values by declared scalar return type and nullability', () async {
      const source =
          'int n(int x) => x; double d(double x) => x; num number(num x) => x; bool flag(bool x) => x; String text(String x) => x; int? nullable(int? x) => x; Object? object(Object? x) => x;';
      final f = await fixture(source);
      final scan = await f.scan(const ReturnValueReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '0',
        '1',
        '-1',
        '0.0',
        '1.0',
        '-1.0',
        '0',
        '1',
        '-1',
        'true',
        'false',
        "''",
        'null',
        '0',
        '1',
        '-1',
        'null',
      ]);
      await f.validMutants(source, scan.candidates);
    });
    test('unwraps async return types and renders typed empty collections including prefixes', () async {
      const source =
          "import 'dart:core' as core; import 'dart:async' as async; async.Future<core.int?> read(core.int? x) async { return x; } core.List<core.int> list(core.List<core.int> x) => x; core.Set<core.String> set(core.Set<core.String> x) => x; core.Map<core.String, core.int> map(core.Map<core.String, core.int> x) => x;";
      final f = await fixture(source);
      final scan = await f.scan(const ReturnValueReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'null',
        '0',
        '1',
        '-1',
        '<core.int>[]',
        '<core.String>{}',
        '<core.String, core.int>{}',
      ]);
      await f.validMutants(source, scan.candidates);
    });
    test('bare return, dynamic, void, custom returns and generators are diagnosed', () async {
      const source =
          'class C {} void done() { return; } dynamic any(dynamic x) => x; void ignored() => print(1); C custom(C x) => x; Iterable<int> values() sync* { yield 1; return; }';
      final f = await fixture(source);
      final scan = await f.scan(const ReturnValueReplacement(), source);
      expect(scan.candidates, isEmpty);
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll(['bare_return', 'unsupported_return_type']),
      );
    });
    test('generic collection results preserve element type and actual async execution', () async {
      const source =
          'List<T> values<T>(List<T> x) => x; Future<int> read() async => 9; Future<void> main() async { print(await read()); }';
      final f = await fixture(source);
      final scan = await f.scan(const ReturnValueReplacement(), source);
      expect(scan.candidates.first.replacement, '<T>[]');
      final zero = scan.candidates.singleWhere(
        (candidate) =>
            original(source, candidate) == '9' && candidate.replacement == '0',
      );
      expect(await f.execute(apply(source, zero)), ['0']);
      await f.validMutants(source, scan.candidates);
    });
  });

  group('CONDITION_FORCING', () {
    test('dynamic conditions and unconditional loops are diagnosed, literal noops omitted', () async {
      const source =
          'void f(dynamic value){if(value)print(1);if(true)print(2);for(;;){break;}}';
      final f = await fixture(source);
      final scan = await f.scan(const ConditionForcing(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'false',
      ]);
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll([
          'unsupported_condition_type',
          'no_boolean_loop_condition',
          'unchanged_condition_value',
        ]),
      );
      await f.validMutants(source, scan.candidates);
    });
    test('supports if, while, do and classic for conditions and skips patterns/for-in', () async {
      const source =
          'void f(bool gate, Object value) { if(gate) print(1); while(gate) { break; } do { print(2); } while(gate); for(var i=0; i<2; i++) {} for(final x in [1]) { print(x); } if(value case int n) print(n); }';
      final f = await fixture(source);
      final scan = await f.scan(const ConditionForcing(), source);
      expect(scan.candidates, hasLength(8));
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll(['pattern_condition', 'no_boolean_loop_condition']),
      );
      await f.validMutants(source, scan.candidates);
    });
    test('promotion-breaking forced conditions produce semantic invalid rather than success', () async {
      const source =
          'int size(String? value) { if(value != null) return value.length; return 0; }';
      final f = await fixture(source);
      final scan = await f.scan(const ConditionForcing(), source);
      final forced = scan.candidates.singleWhere(
        (candidate) => candidate.replacement == 'true',
      );
      expect(errors(await f.resolve(apply(source, forced))), isNotEmpty);
      expectNoErrors(await f.resolve(source));
    });
  });

  group('BRANCH_BODY_REMOVAL', () {
    test('removes both branch bodies but keeps the condition evaluated exactly once', () async {
      const source =
          "final trace=<String>[]; bool condition(){trace.add('condition');return true;} void f(){if(condition()){trace.add('then');}else{trace.add('else');}} void main(){f();print(trace.join(','));}";
      final f = await fixture(source);
      final scan = await f.scan(const BranchBodyRemoval(), source);
      expect(scan.candidates, hasLength(2));
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '{}',
        '{}',
      ]);
      expect(await f.execute(apply(source, scan.candidates.first)), [
        'condition',
      ]);
      await f.validMutants(source, scan.candidates);
    });
    test('already empty branches are skipped and lost required return is invalid', () async {
      const source =
          'int f(bool b){if(b){return 1;}else{return 2;}} void empty(bool b){if(b){}else;}';
      final f = await fixture(source);
      final scan = await f.scan(const BranchBodyRemoval(), source);
      expect(scan.candidates, hasLength(2));
      expect(scan.skips, hasLength(2));
      expect(
        errors(await f.resolve(apply(source, scan.candidates.first))),
        isNotEmpty,
      );
    });
  });

  group('CONDITIONAL_EXPRESSION_MUTATION', () {
    test('each branch choice and swap preserves grouping and avoids double evaluation', () async {
      const source =
          "final trace=<String>[]; bool condition(){trace.add('condition');return true;} int value(String s,int v){trace.add(s);return v;} int f()=>1+(condition()?value('a',2):value('b',3)); void main(){print(f());print(trace.join(','));}";
      final f = await fixture(source);
      final scan = await f.scan(const ConditionalExpressionMutation(), source);
      expect(scan.candidates, hasLength(3));
      expect(await f.execute(apply(source, scan.candidates[0])), ['3', 'a']);
      expect(await f.execute(apply(source, scan.candidates[1])), ['4', 'b']);
      expect(await f.execute(apply(source, scan.candidates[2])), [
        '4',
        'condition,b',
      ]);
      await f.validMutants(source, scan.candidates);
    });
    test('dynamic and const ternaries are diagnosed and invalid promotions are visible', () async {
      const source =
          'dynamic any(bool b,dynamic x,dynamic y)=>b?x:y; int size(String? x)=>x!=null?x.length:0; void c(){const x=true?1:2;print(x);}';
      final f = await fixture(source);
      final scan = await f.scan(const ConditionalExpressionMutation(), source);
      expect(scan.candidates, hasLength(3));
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll(['unsupported_conditional_type', 'constant_context']),
      );
      expect(
        errors(await f.resolve(apply(source, scan.candidates[0]))),
        isNotEmpty,
      );
    });
  });

  group('STATEMENT_DELETION', () {
    test('prefix and postfix numeric increments use resolved read types and SDK bindings', () async {
      const source = 'void f(int x,double y){x++;--x;++y;y--;}';
      final f = await fixture(source);
      final scan = await f.scan(const StatementDeletion(), source);
      expect(scan.candidates, hasLength(4));
      await f.validMutants(source, scan.candidates);
      const custom =
          'class C{C operator +(int x)=>this;}void f(C c,dynamic x){c++;x++;}';
      final bad = await f.scan(const StatementDeletion(), custom);
      expect(bad.candidates, isEmpty);
      expect(bad.skips, hasLength(2));
    });
    test('deletes an awaited statement in an async body and preserves later execution', () async {
      const source =
          'int value=0;Future<void> work()async{value=1;}Future<void> main()async{await work();print(value);}';
      final f = await fixture(source);
      final scan = await f.scan(const StatementDeletion(), source);
      final candidate = scan.candidates.singleWhere(
        (candidate) => original(source, candidate) == 'await work();',
      );
      expect(await f.execute(apply(source, candidate)), ['0']);
      await f.validMutants(source, scan.candidates);
    });
    test('block deletion is exact while unbraced and labelled statements retain valid empty bodies', () async {
      const source =
          'void action(){} void f(bool c){action();if(c)action();else action();label:action();}';
      final f = await fixture(source);
      final scan = await f.scan(const StatementDeletion(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '',
        ';',
        ';',
        ';',
      ]);
      await f.validMutants(source, scan.candidates);
      const duplicate = 'void action(){} void f(bool c){if(c){action();}}';
      final deletion = await f.scan(const StatementDeletion(), duplicate);
      final branch = await f.scan(const BranchBodyRemoval(), duplicate);
      expect(
        apply(duplicate, deletion.candidates.single),
        apply(duplicate, branch.candidates.single),
      );
      expect(
        deletion.candidates.single.offset,
        isNot(branch.candidates.single.offset),
      );
    });
    test('deletes assignments and calls without breaking if-else grammar', () async {
      const source =
          "int x=0; void action(){x+=10;} void f(bool gate){x=1;if(gate)action();else x=2;} void main(){f(true);print(x);}";
      final f = await fixture(source);
      final scan = await f.scan(const StatementDeletion(), source);
      final action = scan.candidates.singleWhere(
        (candidate) => original(source, candidate) == 'action();',
      );
      expect(await f.execute(apply(source, action)), ['1']);
      await f.validMutants(source, scan.candidates);
    });
    test('dynamic calls and overloaded arithmetic statements are diagnosed', () async {
      const source =
          'class C{C operator +(int x)=>this;} void f(dynamic d,C c){d.run();c+=1;}';
      final f = await fixture(source);
      final scan = await f.scan(const StatementDeletion(), source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips, hasLength(2));
    });
  });

  group('BREAK_CONTINUE_MUTATION', () {
    test('continue inside switch is not replaced with a break targeting the wrong construct', () async {
      const source =
          'void f(){for(var i=0;i<3;i++){switch(i){case 1:continue;default:print(i);}}}';
      final f = await fixture(source);
      final scan = await f.scan(const BreakContinueMutation(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [';']);
      expect(scan.skips.single.code, 'continue_inside_switch');
      await f.validMutants(source, scan.candidates);
    });
    test('loop continue removal and break replacement have finite observable effects', () async {
      const source =
          "void main(){final out=<int>[];for(var i=0;i<3;i++){if(i==1)continue;out.add(i);}print(out.join(','));}";
      final f = await fixture(source);
      final scan = await f.scan(const BreakContinueMutation(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        ';',
        'break;',
      ]);
      expect(await f.execute(apply(source, scan.candidates[0])), ['0,1,2']);
      expect(await f.execute(apply(source, scan.candidates[1])), ['0']);
      await f.validMutants(source, scan.candidates);
    });
    test('break mutation stops at loop/function boundaries and labelled transfers are unsupported', () async {
      const source =
          'void f(int x){outer:for(var i=0;i<3;i++){if(i==1)break outer;} while(x>0){break;} switch(x){case 1:break;default:break;} }';
      final f = await fixture(source);
      final scan = await f.scan(const BreakContinueMutation(), source);
      expect(scan.candidates, hasLength(4));
      expect(scan.skips.single.code, 'labelled_control_flow');
      await f.validMutants(source, scan.candidates);
    });
  });

  group('SWITCH_CASE_MUTATION', () {
    test('legacy switch cases and bool/string constant cases use resolved AST nodes', () async {
      for (final source in [
        '// @dart=2.19\nvoid f(int x){switch(x){case 1:print(1);break;default:print(0);break;}}',
        "void f(bool x,String y){switch(x){case true:print(1);case false:print(0);default:print(2);}switch(y){case 'a':print(1);default:print(0);}}",
      ]) {
        final f = await fixture(source);
        final scan = await f.scan(const SwitchCaseMutation(), source);
        expect(scan.candidates, isNotEmpty);
        if (source.startsWith('//')) {
          expect(
            scan.candidates.any((candidate) => candidate.replacement == '-1'),
            isTrue,
          );
        } else {
          expect(
            scan.candidates.any(
              (candidate) => candidate.replacement == "'mutated'",
            ),
            isTrue,
          );
          expect(
            scan.candidates.any(
              (candidate) => candidate.replacement == 'false',
            ),
            isTrue,
          );
        }
        await f.validMutants(source, scan.candidates);
      }
    });
    test('removing a required exhaustive bool case is statically invalid', () async {
      const source =
          'void f(bool x){switch(x){case true:print(1);case false:print(0);}}';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchCaseMutation(), source);
      final deleted = scan.candidates.singleWhere(
        (candidate) =>
            candidate.replacement.isEmpty &&
            original(source, candidate).startsWith('case true:'),
      );
      expect(errors(await f.resolve(apply(source, deleted))), isNotEmpty);
      expectNoErrors(await f.resolve(source));
    });
    test('supports case/default branch deletion, body deletion and constant replacements', () async {
      const source =
          'void f(int x){switch(x){case 1:print(1);break;case 2:print(2);break;default:print(3);break;}}';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchCaseMutation(), source);
      expect(scan.candidates, hasLength(11));
      expect(
        scan.candidates.any(
          (candidate) =>
              original(source, candidate).startsWith('default:') &&
              candidate.replacement.isEmpty,
        ),
        isTrue,
      );
      await f.validMutants(source, scan.candidates);
    });
    test('guarded patterns and switch expressions are diagnosed', () async {
      const source =
          'int g(Object x)=>switch(x){int n=>n,_=>0};void f(Object x){switch(x){case int n when n>0:print(n);case _:print(x);}}';
      final f = await fixture(source);
      final scan = await f.scan(const SwitchCaseMutation(), source);
      expect(scan.candidates, isEmpty);
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll([
          'unsupported_switch_expression',
          'unsupported_switch_pattern',
        ]),
      );
    });
  });

  group('THROW_REMOVAL and EXCEPTION_REPLACEMENT', () {
    test('standalone throw deletion is executable while required return removal is invalid', () async {
      const source =
          "void main(){try{throw StateError('broken');}catch(e){print('caught');}print('done');} int required()=>throw StateError('broken');";
      final f = await fixture(source);
      final scan = await f.scan(const ThrowRemoval(), source);
      expect(scan.candidates, hasLength(1));
      expect(scan.skips.single.code, 'expression_throw');
      expect(await f.execute(apply(source, scan.candidates.single)), ['done']);
      await f.validMutants(source, scan.candidates);
      const missing = "int f(){throw StateError('broken');}";
      final scanMissing = await f.scan(const ThrowRemoval(), missing);
      expect(
        errors(await f.resolve(apply(missing, scanMissing.candidates.single))),
        isNotEmpty,
      );
    });
    test('SDK exception class and message variants resolve and change actual caught type', () async {
      const source =
          "void main(){try{throw StateError('broken');}catch(e){print(e.runtimeType);print(e.toString());}}";
      final f = await fixture(source);
      final scan = await f.scan(const ExceptionReplacement(), source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        "StateError('')",
        "ArgumentError('')",
        "Exception('')",
      ]);
      expect(
        (await f.execute(apply(source, scan.candidates[1]))).first,
        'ArgumentError',
      );
      await f.validMutants(source, scan.candidates);
    });
    test('prefix fallback is safe and user exceptions or bound exception names are skipped', () async {
      const source =
          "import 'dart:core';import 'dart:core' as core;class StateError{StateError(String x);} void f(Object Exception,Object ArgumentError){throw core.StateError('x');} void user(){throw StateError('x');}";
      final f = await fixture(source);
      final scan = await f.scan(const ExceptionReplacement(), source);
      expect(scan.candidates, hasLength(3));
      expect(
        scan.candidates.every(
          (candidate) => candidate.replacement.startsWith('core.'),
        ),
        isTrue,
      );
      expect(scan.skips.single.code, 'unsupported_exception_expression');
      await f.validMutants(source, scan.candidates);
    });
  });

  group('COLLECTION_LITERAL_MUTATION', () {
    test('list/set/map empty and each item removal preserve typed delimiters and trailing comma', () async {
      const source =
          "void f(){final list=<int>[1,2,3,];final set=<int>{1,2,3,};final map=<String,int>{'a':1,'b':2,'c':3,};print([list,set,map]);}";
      final f = await fixture(source);
      final scan = await f.scan(const CollectionLiteralMutation(), source);
      final target = scan.candidates
          .where((candidate) => original(source, candidate).startsWith('<'))
          .toList();
      expect(target, hasLength(12));
      expect(
        target.map((candidate) => candidate.replacement),
        containsAll(['<int>[]', '<int>{}', '<String, int>{}']),
      );
      await f.validMutants(source, target);
    });
    test('inferred set retains Set type when emptied, while spreads/if/for/const are skipped', () async {
      const source =
          'void f(bool gate,List<int> xs){final set={1,2};final spread=[...xs];final conditional=[if(gate)1];final loop=[for(final x in xs)x];const constant=[1,2];print(set);print(spread);print(conditional);print(loop);print(constant);}';
      final f = await fixture(source);
      final scan = await f.scan(const CollectionLiteralMutation(), source);
      expect(scan.candidates, hasLength(3));
      expect(scan.candidates.first.replacement, '<int>{}');
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll(['unsupported_collection_element', 'constant_context']),
      );
      await f.validMutants(source, scan.candidates);
    });
    test(
      'single item produces one empty variant, empty literals are diagnosed',
      () async {
        const source = 'List<int> one()=>[1];List<int> empty()=>[];';
        final f = await fixture(source);
        final scan = await f.scan(const CollectionLiteralMutation(), source);
        expect(scan.candidates, hasLength(1));
        expect(scan.candidates.single.replacement, '<int>[]');
        expect(scan.skips.single.code, 'empty_collection');
        await f.validMutants(source, scan.candidates);
      },
    );
  });
}

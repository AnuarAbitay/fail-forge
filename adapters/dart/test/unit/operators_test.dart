import 'dart:convert';
import 'dart:io';

import 'package:analyzer/dart/analysis/analysis_context_collection.dart';
import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/diagnostic/diagnostic.dart';
import 'package:analyzer/error/error.dart';
import 'package:analyzer/file_system/overlay_file_system.dart';
import 'package:analyzer/file_system/physical_file_system.dart';
import 'package:failforge_dart_adapter/src/operators/classic/boolean_literal_flip.dart';
import 'package:failforge_dart_adapter/src/operators/classic/conditional_negation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/logical_connector_swap.dart';
import 'package:failforge_dart_adapter/src/operators/classic/relational_boundary.dart';
import 'package:failforge_dart_adapter/src/operators/common.dart';
import 'package:failforge_dart_adapter/src/operators/failure_modes/exception_at_entry.dart';
import 'package:failforge_dart_adapter/src/operators/failure_modes/method_channel_error.dart';
import 'package:failforge_dart_adapter/src/operators/registry.dart';
import 'package:failforge_dart_adapter/src/project/source_file.dart';
import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

import '../support/semantic_output.dart';

void main() {
  test(
    'classic registry preserves original IDs and adds the complete catalogue',
    () {
      expect(classicOperators, hasLength(32));
      expect(defaultClassicOperators, hasLength(31));
      expect(
        classicOperators.map((operator) => operator.id),
        containsAll([
          'BOOLEAN_LITERAL_FLIP',
          'CONDITIONAL_NEGATION',
          'LOGICAL_CONNECTOR_SWAP',
          'RELATIONAL_BOUNDARY',
        ]),
      );
      expect(
        classicOperators.every((operator) => operator.version == 1),
        isTrue,
      );
      expect(
        classicOperators.every(
          (operator) => operator.family == MutationFamily.classic,
        ),
        isTrue,
      );
    },
  );

  test(
    'P1 registry is separate and labels both operators as failure modes',
    () {
      expect(failureModeOperators.map((operator) => operator.id), [
        'EXCEPTION_AT_ENTRY',
        'METHOD_CHANNEL_ERROR',
      ]);
      expect(
        failureModeOperators.every(
          (operator) =>
              operator.version == 1 &&
              operator.family == MutationFamily.failureMode,
        ),
        isTrue,
      );
      expect(
        classicOperators.any(
          (operator) => operator.family == MutationFamily.failureMode,
        ),
        isFalse,
      );
    },
  );

  test('collector item and replacement budgets reject overflow without adding the item', () async {
    const source = 'bool f() => true;\nbool g() => false;\n';
    final fixture = await _Fixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve(source);
    final nodes = const BooleanLiteralFlip()
        .discover(unit, source)
        .candidates
        .map((candidate) => candidate.anchor)
        .toList();
    final limitedItems = _BudgetProbe(
      source,
      maxItems: 1,
      maxReplacementUnits: 20,
    );
    limitedItems.replace(nodes.first, 'false');
    expect(
      () => limitedItems.skip(
        nodes.last,
        'unsupported',
        'Unsupported expression.',
      ),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'candidate_limit_exceeded',
        ),
      ),
    );
    expect(limitedItems.candidates, hasLength(1));
    expect(limitedItems.skips, isEmpty);

    final limitedText = _BudgetProbe(
      source,
      maxItems: 10,
      maxReplacementUnits: 9,
    );
    limitedText.replace(nodes.first, 'false');
    limitedText.replace(nodes.last, 'true');
    expect(limitedText.candidates, hasLength(2)); // Exact aggregate boundary.
    expect(
      () => limitedText.replace(nodes.first, 'false'),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'candidate_limit_exceeded',
        ),
      ),
    );
    expect(limitedText.candidates, hasLength(2));
  });

  group('BOOLEAN_LITERAL_FLIP', () {
    test(
      'flips local, return and closure literals with one exact edit',
      () async {
        const source = '''
bool f(bool flag) {
  bool value = false;
  final closure = () => true;
  if (flag) return true;
  return closure() && value;
}
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final scan = const BooleanLiteralFlip().discover(unit, source);
        expect(scan.candidates, hasLength(3));
        expect(scan.skips, isEmpty);
        expect(
          scan.candidates.map((candidate) => _original(source, candidate)),
          ['false', 'true', 'true'],
        );
        expect(scan.candidates.map((candidate) => candidate.replacement), [
          'true',
          'false',
          'false',
        ]);
        await _expectValidMutants(fixture, source, scan.candidates);
      },
    );

    test(
      'excludes metadata, fields, defaults, const and pattern expressions',
      () async {
        const source = '''
class Marker { const Marker(bool value); }
@Marker(true)
class Holder {
  final field = true;
  bool run({bool value = true}) {
    const local = false;
    final items = const [true];
    final text = 'true false'; // true false are not AST literals.
    if (value case true) { return false; }
    return value;
  }
}
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final scan = const BooleanLiteralFlip().discover(unit, source);
        expect(scan.candidates, hasLength(1));
        expect(_original(source, scan.candidates.single), 'false');
        expect(
          scan.candidates.single.offset,
          source.indexOf('return false') + 'return '.length,
        );
        expect(
          scan.skips.map((skip) => skip.code),
          containsAll([
            'metadata_context',
            'non_executable_context',
            'default_parameter',
            'constant_context',
            'pattern_context',
          ]),
        );
        await _expectValidMutants(fixture, source, scan.candidates);
      },
    );

    test('excludes if-case selector literals and when guards but keeps branch bodies', () async {
      const source = '''
bool f(Object value, bool gate) {
  if (true case false) return false;
  if (value case bool item when item && true) return gate;
  return gate;
}
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const BooleanLiteralFlip().discover(unit, source);
      expect(scan.candidates, hasLength(1));
      expect(
        scan.candidates.single.offset,
        source.indexOf('return false') + 'return '.length,
      );
      expect(
        scan.skips.map((skip) => skip.code),
        containsAll(['pattern_condition', 'pattern_context']),
      );
    });

    test('keeps UTF-16 ranges and UTF-8 patches exact with BOM, Cyrillic, emoji and CRLF', () async {
      const source = '\ufeff// привет 😀\r\nbool f() => true;\r\n';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final candidate = const BooleanLiteralFlip()
          .discover(unit, unit.content)
          .candidates
          .single;
      expect(candidate.offset, source.indexOf('true') - 1);
      expect(candidate.length, 4);
      final bytes = utf8.encode(source);
      final snapshot = SourceFile('lib/source.dart', fixture.path, bytes);
      final edit = snapshot.editFromAnalyzer(
        candidate.offset,
        candidate.length,
        candidate.replacement,
      );
      expect(
        edit.offsetBytes,
        utf8.encode(source.substring(0, candidate.offset + 1)).length,
      );
      expect(edit.offsetBytes, greaterThan(candidate.offset));
      expect(edit.expected, 'true');
      expect(edit.replacement, 'false');
      final changed = source.replaceRange(
        candidate.offset + 1,
        candidate.offset + 1 + candidate.length,
        candidate.replacement,
      );
      expect(edit.apply(snapshot), utf8.encode(changed));
      expect(await File(fixture.path).readAsBytes(), bytes);
      _expectNoErrors(await fixture.resolve(changed));
      _expectNoErrors(await fixture.resolve(source));
    });
  });

  group('CONDITIONAL_NEGATION', () {
    test('negates the original expression once and retains comments and parentheses', () async {
      const source = '''
bool check() => true;
int f(bool a, bool b) {
  if (check(/* keep */)) return 1;
  if ((a && b)) return 2;
  return 0;
}
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const ConditionalNegation().discover(unit, source);
      expect(scan.candidates, hasLength(2));
      expect(scan.candidates.map((candidate) => _original(source, candidate)), [
        'check(/* keep */)',
        '(a && b)',
      ]);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        '!(check(/* keep */))',
        '!((a && b))',
      ]);
      expect(
        'check'.allMatches(scan.candidates.first.replacement),
        hasLength(1),
      );
      await _expectValidMutants(fixture, source, scan.candidates);
    });

    test(
      'skips if-case and unresolved boolean proof for dynamic conditions',
      () async {
        const source = '''
int f(dynamic value, bool flag) {
  if (value case int number) return number;
  if (value) return 2;
  if (flag) return 3;
  return 0;
}
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final scan = const ConditionalNegation().discover(unit, source);
        expect(scan.candidates, hasLength(1));
        expect(_original(source, scan.candidates.single), 'flag');
        expect(scan.skips.map((skip) => skip.code), [
          'pattern_condition',
          'unsupported_condition_type',
        ]);
      },
    );

    test(
      'promotion-breaking inversion is syntactic but semantically invalid',
      () async {
        const source = '''
int length(String? value) {
  if (value != null) { return value.length; }
  return 0;
}
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final baseline = await fixture.resolve(source);
        _expectNoErrors(baseline);
        final candidate = const ConditionalNegation()
            .discover(baseline, source)
            .candidates
            .single;
        expect(_original(source, candidate), 'value != null');
        expect(candidate.replacement, '!(value != null)');
        final changed = await fixture.resolve(_apply(source, candidate));
        expect(
          changed.diagnostics.where(
            (diagnostic) => diagnostic.severity == Severity.error,
          ),
          isNotEmpty,
        );
        expect(
          changed.diagnostics.where(
            (diagnostic) =>
                diagnostic.diagnosticCode.type ==
                DiagnosticType.SYNTACTIC_ERROR,
          ),
          isEmpty,
        );
        _expectNoErrors(await fixture.resolve(source));
        expect(await File(fixture.path).readAsBytes(), utf8.encode(source));
      },
    );
  });

  group('RELATIONAL_BOUNDARY', () {
    test(
      'mutates all four confirmed numeric operators only at their tokens',
      () async {
        const source = '''
bool f(num a, num b, int i, double d) =>
    a > b && a >= b && i < d && d <= i;
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final scan = const RelationalBoundary().discover(unit, source);
        expect(scan.skips, isEmpty);
        expect(
          scan.candidates.map((candidate) => _original(source, candidate)),
          ['>', '>=', '<', '<='],
        );
        expect(scan.candidates.map((candidate) => candidate.replacement), [
          '>=',
          '>',
          '<=',
          '<',
        ]);
        await _expectValidMutants(fixture, source, scan.candidates);
      },
    );

    test('rejects a user class named num and dynamic operands', () async {
      const source = '''
class num { bool operator >(num other) => true; }
bool custom(num a, num b) => a > b;
bool dynamicLeft(dynamic a, int b) => a > b;
bool dynamicRight(int a, dynamic b) => a > b;
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const RelationalBoundary().discover(unit, source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips, hasLength(3));
      expect(
        scan.skips.every(
          (skip) => skip.code == 'unsupported_numeric_operation',
        ),
        isTrue,
      );
    });

    test('recognizes SDK numeric types through a prefixed import', () async {
      const source = '''
import 'dart:core' as core;
core.bool f(core.num a, core.double b) => a >= b;
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const RelationalBoundary().discover(unit, source);
      expect(scan.candidates, hasLength(1));
      expect(_original(source, scan.candidates.single), '>=');
      await _expectValidMutants(fixture, source, scan.candidates);
    });

    test(
      'uses promoted nullable numeric operands and excludes const comparisons',
      () async {
        const source = '''
bool f(int? value) {
  const threshold = 1 > 0;
  return value != null && value > 0;
}
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final scan = const RelationalBoundary().discover(unit, source);
        expect(scan.candidates, hasLength(1));
        expect(scan.skips.single.code, 'constant_context');
        await _expectValidMutants(fixture, source, scan.candidates);
      },
    );
  });

  group('LOGICAL_CONNECTOR_SWAP', () {
    test('preserves inner and outer AST grouping with actual short-circuit side effects', () async {
      for (final originalOperator in ['&&', '||']) {
        final source =
            '''
final trace = <String>[];
bool read(String tag, bool value) { trace.add(tag); return value; }
bool f() => read('a', true) $originalOperator read('b', false) $originalOperator read('c', false);
void main() { print(f()); print(trace.join(',')); }
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final baseline = await fixture.resolve(source);
        _expectNoErrors(baseline);
        final candidates = const LogicalConnectorSwap()
            .discover(baseline, source)
            .candidates;
        final candidate = candidates.singleWhere((candidate) {
          final node = candidate.anchor as BinaryExpression;
          return originalOperator == '&&'
              ? node.leftOperand is! BinaryExpression
              : node.leftOperand is BinaryExpression;
        });
        final changed = _apply(source, candidate);
        _expectNoErrors(await fixture.resolve(changed));
        expect(await _executeFiniteFixture(fixture, changed), ['false', 'a,c']);
        // The historical token-only edit instead short-circuits at a=true,
        // returning true and skipping c. Execute it to prove the regression.
        final binary = candidate.anchor as BinaryExpression;
        final tokenOnly = source.replaceRange(
          binary.operator.offset,
          binary.operator.end,
          originalOperator == '&&' ? '||' : '&&',
        );
        expect(await _executeFiniteFixture(fixture, tokenOnly), ['true', 'a']);
        expect(await File(fixture.path).readAsBytes(), utf8.encode(source));
      }
    });

    test('swaps AST connectors while preserving groups and exact intervening trivia', () async {
      const source =
          'bool f(bool a, bool b, bool c) => a /* keep */ && b || c;\n';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const LogicalConnectorSwap().discover(unit, source);
      final candidates = [...scan.candidates]
        ..sort((a, b) => a.length.compareTo(b.length));
      expect(candidates.map((candidate) => _original(source, candidate)), [
        'a /* keep */ && b',
        'a /* keep */ && b || c',
      ]);
      expect(candidates.map((candidate) => candidate.replacement), [
        '((a) /* keep */ || (b))',
        '((a /* keep */ && b) && (c))',
      ]);
      expect(_apply(source, candidates.first), contains('/* keep */'));
      await _expectValidMutants(fixture, source, candidates);
    });

    test('ignores strings and excludes const and pattern guards', () async {
      const source = '''
bool f(Object value, bool gate) {
  const local = true && false;
  final text = '&& ||'; // && || are not AST operators.
  if (value case bool item when item && gate) return false;
  return gate;
}
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const LogicalConnectorSwap().discover(unit, source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips.map((skip) => skip.code), [
        'constant_context',
        'pattern_context',
      ]);
    });

    test(
      'promotion-breaking connector change is rejected by semantic analysis',
      () async {
        const source =
            'bool f(String? value) => value != null && value.isNotEmpty;\n';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final baseline = await fixture.resolve(source);
        _expectNoErrors(baseline);
        final candidate = const LogicalConnectorSwap()
            .discover(baseline, source)
            .candidates
            .single;
        expect(
          _original(source, candidate),
          'value != null && value.isNotEmpty',
        );
        expect(
          candidate.replacement,
          '((value != null) || (value.isNotEmpty))',
        );
        final changed = await fixture.resolve(_apply(source, candidate));
        expect(
          changed.diagnostics.where(
            (diagnostic) => diagnostic.severity == Severity.error,
          ),
          isNotEmpty,
        );
        expect(
          changed.diagnostics.where(
            (diagnostic) =>
                diagnostic.diagnosticCode.type ==
                DiagnosticType.SYNTACTIC_ERROR,
          ),
          isEmpty,
        );
        _expectNoErrors(await fixture.resolve(source));
      },
    );
  });
  group('EXCEPTION_AT_ENTRY', () {
    test('replaces only body ranges and preserves sync, async and member signatures', () async {
      const source = '''
int plain(int x) => x + 1;
Future<int> asynchronous(int x) async { return x + 1; }
class Service {
  Service();
  int run(int x) { return x; }
  int get value => 1;
  set value(int x) { print(x); }
}
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const ExceptionAtEntry().discover(unit, source);
      expect(scan.candidates, hasLength(5));
      expect(scan.skips, isEmpty);
      expect(
        scan.candidates.every(
          (candidate) =>
              candidate.anchor is FunctionBody &&
              candidate.family == MutationFamily.failureMode,
        ),
        isTrue,
      );
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        "{ throw StateError('FAILFORGE_EXCEPTION_AT_ENTRY'); }",
        "async { throw StateError('FAILFORGE_EXCEPTION_AT_ENTRY'); }",
        "{ throw StateError('FAILFORGE_EXCEPTION_AT_ENTRY'); }",
        "{ throw StateError('FAILFORGE_EXCEPTION_AT_ENTRY'); }",
        "{ throw StateError('FAILFORGE_EXCEPTION_AT_ENTRY'); }",
      ]);
      await _expectValidMutants(fixture, source, scan.candidates);
    });

    test(
      'async entry failures complete the returned Future with the SDK error',
      () async {
        const source = '''
Future<int> read() async => 7;
Future<void> main() async {
  try {
    final future = read();
    print('returned-future');
    await future;
  } on StateError catch (error) {
    print(error.message);
  }
}
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final candidate = const ExceptionAtEntry()
            .discover(unit, source)
            .candidates
            .first;
        expect(
          await _executeFiniteFixture(fixture, _apply(source, candidate)),
          ['returned-future', 'FAILFORGE_EXCEPTION_AT_ENTRY'],
        );
        await _expectValidMutants(fixture, source, [candidate]);
      },
    );

    test(
      'skips external, abstract, generator and local function bodies',
      () async {
        const source = '''
external int nativeCall();
Iterable<int> values() sync* { yield 1; }
Stream<int> events() async* { yield 1; }
abstract class Service {
  Service();
  int abstractCall();
}
int outer() {
  int local() => 1;
  return local();
}
''';
        final fixture = await _Fixture.open(source);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final scan = const ExceptionAtEntry().discover(unit, source);
        expect(scan.candidates, hasLength(1));
        expect(
          _original(source, scan.candidates.single),
          contains('int local()'),
        );
        expect(
          scan.skips.where((skip) => skip.code == 'unsupported_function_body'),
          hasLength(4),
        );
        expect(
          scan.skips.where((skip) => skip.code == 'local_function'),
          hasLength(1),
        );
        await _expectValidMutants(fixture, source, scan.candidates);
      },
    );

    test('uses an existing core prefix when StateError is user-defined or a parameter', () async {
      const source = '''
import 'dart:core';
import 'dart:core' as sdk;
class StateError { StateError(String message); }
int run(int StateError) => StateError;
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const ExceptionAtEntry().discover(unit, source);
      expect(
        scan.candidates.single.replacement,
        contains('throw sdk.StateError('),
      );
      await _expectValidMutants(fixture, source, scan.candidates);
    });

    test('rejects parameter, generic and inherited member shadows but ignores removed body locals', () async {
      const source = '''
int parameter(int StateError) => StateError;
T generic<T, StateError>(T value) => value;
class Base { Object get StateError => Object(); }
class Child extends Base { int run() => 1; }
int bodyLocal() { final StateError = 1; return StateError; }
''';
      final fixture = await _Fixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const ExceptionAtEntry().discover(unit, source);
      expect(scan.candidates, hasLength(1));
      expect(
        _original(source, scan.candidates.single),
        contains('final StateError'),
      );
      expect(scan.skips, hasLength(4)); // Includes the shadowing getter itself.
      expect(
        scan.skips.every((skip) => skip.code == 'sdk_exception_not_available'),
        isTrue,
      );
      await _expectValidMutants(fixture, source, scan.candidates);
    });
  });

  group('METHOD_CHANNEL_ERROR', () {
    test('binding caches stay isolated between scopes and successive overlay scans', () async {
      const source = '''
import 'package:flutter/services.dart';
Future<int?> shadowed(MethodChannel channel, Object PlatformException) async => await channel.invokeMethod<int>('read');
Future<int?> clear(MethodChannel channel) async => await channel.invokeMethod<int>('read');
''';
      final fixture = await _Fixture.open(source, flutter: true);
      addTearDown(fixture.close);
      const operator = MethodChannelError();
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final first = operator.discover(unit, source);
      expect(first.candidates, hasLength(3));
      expect(first.skips, hasLength(1));
      final changed = source.replaceFirst(
        'Future<int?> clear(MethodChannel channel)',
        'Future<int?> clear(MethodChannel channel, Object PlatformException, Object MissingPluginException)',
      );
      final changedUnit = await fixture.resolve(changed);
      _expectNoErrors(changedUnit);
      final second = operator.discover(changedUnit, changed);
      expect(second.candidates, hasLength(1));
      expect(second.skips, hasLength(3));
      expect(
        second.candidates.single.replacement,
        contains('throw MissingPluginException('),
      );
      final restoredUnit = await fixture.resolve(source);
      _expectNoErrors(restoredUnit);
      final restored = operator.discover(restoredUnit, source);
      expect(
        restored.candidates.map((candidate) => candidate.replacement),
        first.candidates.map((candidate) => candidate.replacement),
      );
      expect(restored.skips, hasLength(1));
      expect(await File(fixture.path).readAsBytes(), utf8.encode(source));
    });

    test(
      'records opt-in resolver scaling measurements on resolved units',
      () async {
        final samples = <Object?>[];
        final fixture = await _Fixture.open('', flutter: true);
        addTearDown(fixture.close);
        for (final count in [100, 200, 400]) {
          final source =
              "import 'package:flutter/services.dart' as services;\nFuture<void> read(services.MethodChannel channel) async {\n" +
              List.filled(
                count,
                "await channel.invokeMethod<int>('read');\n",
              ).join() +
              '}\n';
          final unit = await fixture.resolve(source);
          _expectNoErrors(unit);
          const operator = MethodChannelError();
          operator.discover(unit, source); // One unmeasured warmup per unit.
          final runs = <int>[];
          for (var repeat = 0; repeat < 3; repeat++) {
            final timer = Stopwatch()..start();
            final scan = operator.discover(unit, source);
            timer.stop();
            runs.add(timer.elapsedMicroseconds);
            expect(scan.candidates, hasLength(count * 2));
            expect(scan.skips, isEmpty);
          }
          samples.add({
            'calls': count,
            'candidates': count * 2,
            'runsMicros': runs,
          });
        }
        final path = Platform.environment['FAILFORGE_OPERATOR_SCALING_PROBE']!;
        final artifact = File(path);
        await artifact.parent.create(recursive: true);
        await artifact.writeAsString(
          const JsonEncoder.withIndent('  ').convert({
            'operator': 'METHOD_CHANNEL_ERROR',
            'sdk': Platform.version,
            'warmupPerResolvedUnit': 1,
            'repetitionsPerResolvedUnit': 3,
            'includesAnalysis': false,
            'samples': samples,
          }),
        );
      },
      skip: Platform.environment['FAILFORGE_OPERATOR_SCALING_PROBE'] == null
          ? 'Set an artifact path to run the measured scaling probe.'
          : false,
    );

    test('uses real SDK prefixed symbols and preserves await and nullable generic result types', () async {
      const source = '''
import 'package:flutter/services.dart' as services;
Future<T?> read<T>(services.MethodChannel channel, String method, Object? args) async {
  return await (channel.invokeMethod<T>(method, args));
}
Future<int?> literal(services.MethodChannel channel) async => await channel.invokeMethod<int>('get', null);
''';
      final fixture = await _Fixture.open(source, flutter: true);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const MethodChannelError().discover(unit, source);
      expect(scan.candidates, hasLength(4));
      expect(scan.skips, isEmpty);
      expect(scan.candidates.map((candidate) => _original(source, candidate)), [
        'channel.invokeMethod<T>(method, args)',
        'channel.invokeMethod<T>(method, args)',
        "channel.invokeMethod<int>('get', null)",
        "channel.invokeMethod<int>('get', null)",
      ]);
      expect(
        scan.candidates[0].replacement,
        "(() async => throw services.PlatformException(code: 'FAILFORGE_METHOD_CHANNEL_ERROR'))()",
      );
      expect(
        scan.candidates[1].replacement,
        "(() async => throw services.MissingPluginException('FAILFORGE_METHOD_CHANNEL_ERROR'))()",
      );
      await _expectValidMutants(fixture, source, scan.candidates);
    });

    test('honors show/hide and supports only the available exception variant', () async {
      for (final directive in [
        "show MethodChannel, MissingPluginException",
        "hide PlatformException",
      ]) {
        final source =
            "import 'package:flutter/services.dart' $directive;\nFuture<int?> read(MethodChannel channel) async => await channel.invokeMethod<int>('read');\n";
        final fixture = await _Fixture.open(source, flutter: true);
        try {
          final unit = await fixture.resolve(source);
          _expectNoErrors(unit);
          final scan = const MethodChannelError().discover(unit, source);
          expect(scan.candidates, hasLength(1));
          expect(
            scan.candidates.single.replacement,
            contains('throw MissingPluginException('),
          );
          expect(scan.skips.single.code, 'sdk_exception_not_available');
          await _expectValidMutants(fixture, source, scan.candidates);
        } finally {
          await fixture.close();
        }
      }
    });

    test(
      'does not substitute user exception names or shadowed imported prefixes',
      () async {
        const source = '''
import 'package:flutter/services.dart';
class PlatformException { PlatformException({String? code}); }
Future<int?> read(MethodChannel channel, Object MissingPluginException) async => await channel.invokeMethod<int>('read');
''';
        final fixture = await _Fixture.open(source, flutter: true);
        addTearDown(fixture.close);
        final unit = await fixture.resolve(source);
        _expectNoErrors(unit);
        final scan = const MethodChannelError().discover(unit, source);
        expect(scan.candidates, isEmpty);
        expect(scan.skips.map((skip) => skip.code), [
          'sdk_exception_not_available',
          'sdk_exception_not_available',
        ]);

        const prefixed = '''
import 'package:flutter/services.dart' as services;
class Base { Object get services => Object(); }
class Child extends Base {
  Future<int?> read(services.MethodChannel channel) async => await channel.invokeMethod<int>('read');
}
''';
        final unit2 = await fixture.resolve(prefixed);
        _expectNoErrors(unit2);
        final scan2 = const MethodChannelError().discover(unit2, prefixed);
        expect(scan2.candidates, isEmpty);
        expect(scan2.skips, hasLength(2));
      },
    );

    test('rejects non-SDK, OptionalMethodChannel, unawaited and side-effecting expressions', () async {
      const source = '''
import 'package:flutter/services.dart' as sdk;
class MethodChannel { Future<int?> invokeMethod(String name) async => 1; }
class Holder { sdk.MethodChannel get channel => const sdk.MethodChannel('x'); }
String method() => 'read';
Object? argument() => null;
sdk.MethodChannel makeChannel() => const sdk.MethodChannel('x');
Future<void> read(sdk.MethodChannel channel, sdk.OptionalMethodChannel optional, MethodChannel fake, Holder holder) async {
  await fake.invokeMethod('read');
  await optional.invokeMethod<int>('read');
  channel.invokeMethod<int>('read');
  await makeChannel().invokeMethod<int>('read');
  await holder.channel.invokeMethod<int>('read');
  await channel.invokeMethod<int>(method());
  await channel.invokeMethod<int>('read', argument());
  late final lateChannel = makeChannel();
  await lateChannel.invokeMethod<int>('read');
  late final lateMethod = method();
  await channel.invokeMethod<int>(lateMethod);
}
''';
      final fixture = await _Fixture.open(source, flutter: true);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const MethodChannelError().discover(unit, source);
      expect(scan.candidates, isEmpty);
      expect(scan.skips.map((skip) => skip.code), [
        'unsupported_method_channel',
        'unsupported_method_channel',
        'not_directly_awaited',
        'side_effecting_evaluation',
        'side_effecting_evaluation',
        'side_effecting_evaluation',
        'side_effecting_evaluation',
        'side_effecting_evaluation',
        'side_effecting_evaluation',
      ]);
    });

    test('uses prefix fallback for body bindings and rejects inherited and type parameter collisions', () async {
      const source = '''
import 'package:flutter/services.dart';
import 'package:flutter/services.dart' as sdk;
Future<int?> local(MethodChannel channel) async {
  final PlatformException = 1;
  final MissingPluginException = 2;
  return await channel.invokeMethod<int>('read');
}
Future<int?> generic<sdk>(MethodChannel channel, Object PlatformException, Object MissingPluginException) async => await channel.invokeMethod<int>('read');
''';
      final fixture = await _Fixture.open(source, flutter: true);
      addTearDown(fixture.close);
      final unit = await fixture.resolve(source);
      _expectNoErrors(unit);
      final scan = const MethodChannelError().discover(unit, source);
      expect(scan.candidates, hasLength(2));
      expect(
        scan.candidates.every(
          (candidate) => candidate.replacement.contains('throw sdk.'),
        ),
        isTrue,
      );
      expect(scan.skips, hasLength(2));
      await _expectValidMutants(fixture, source, scan.candidates);
    });
  });
}

class _BudgetProbe extends OperatorCollector {
  _BudgetProbe(
    String source, {
    required super.maxItems,
    required super.maxReplacementUnits,
  }) : super(const BooleanLiteralFlip(), source);
}

/// Test-only execution of a finite, SDK-only fixture. The adapter operators
/// themselves never execute commands; the test programs are finite.
Future<List<String>> _executeFiniteFixture(
  _Fixture fixture,
  String source,
) async {
  final file = File(p.join(fixture.directory.path, 'run_mutant.dart'));
  await file.writeAsBytes(utf8.encode(source));
  final process = await Process.start(Platform.resolvedExecutable, [
    '--packages=' +
        p.join(fixture.directory.path, '.dart_tool', 'package_config.json'),
    file.path,
  ], workingDirectory: fixture.directory.path);
  var exited = false;
  final exitCode = process.exitCode.then((code) {
    exited = true;
    return code;
  });
  final output = process.stdout.transform(utf8.decoder).join();
  final errors = process.stderr.transform(utf8.decoder).join();
  try {
    final result = await Future.wait<Object>([exitCode, output, errors])
        .timeout(const Duration(seconds: 10));
    expect(result[0], 0, reason: result[2] as String);
    return semanticStdout(result[1] as String).trim().split('\n');
  } finally {
    if (!exited) {
      process.kill(ProcessSignal.sigkill);
      await exitCode.timeout(const Duration(seconds: 5));
    }
  }
}

String _original(String source, CandidateDraft candidate) =>
    source.substring(candidate.offset, candidate.offset + candidate.length);

String _apply(String source, CandidateDraft candidate) => source.replaceRange(
  candidate.offset,
  candidate.offset + candidate.length,
  candidate.replacement,
);

void _expectNoErrors(ResolvedUnitResult unit) {
  expect(
    unit.diagnostics.where(
      (diagnostic) => diagnostic.severity == Severity.error,
    ),
    isEmpty,
    reason: unit.diagnostics.map((diagnostic) => diagnostic.message).join('\n'),
  );
}

Future<void> _expectValidMutants(
  _Fixture fixture,
  String source,
  Iterable<CandidateDraft> candidates,
) async {
  for (final candidate in candidates) {
    expect(candidate.offset, greaterThanOrEqualTo(0));
    expect(candidate.length, greaterThan(0));
    expect(candidate.replacement, isNot(_original(source, candidate)));
    final changed = _apply(source, candidate);
    expect(
      changed.substring(0, candidate.offset),
      source.substring(0, candidate.offset),
    );
    expect(
      changed.substring(candidate.offset + candidate.replacement.length),
      source.substring(candidate.offset + candidate.length),
    );
    _expectNoErrors(await fixture.resolve(changed));
    _expectNoErrors(await fixture.resolve(source));
  }
  expect(await File(fixture.path).readAsBytes(), utf8.encode(source));
}

/// Each unit test creates a separate package without invoking pub get. Mutation
/// validation uses the real analyzer overlay; original files remain untouched.
class _Fixture {
  _Fixture(this.directory, this.path, this.overlay, this.collection);

  final Directory directory;
  final String path;
  final OverlayResourceProvider overlay;
  final AnalysisContextCollection collection;
  int stamp = 0;

  static Future<_Fixture> open(String source, {bool flutter = false}) async {
    final directory = await Directory.systemTemp.createTemp(
      'ff operator fixture ',
    );
    final path = p.join(directory.path, 'lib', 'source.dart');
    await File(path).parent.create(recursive: true);
    await File(path).writeAsBytes(utf8.encode(source));
    await File(p.join(directory.path, 'pubspec.yaml')).writeAsString(
      'name: operator_fixture\nenvironment:\n  sdk: ">=3.13.0 <4.0.0"\n',
    );
    final config = File(
      p.join(directory.path, '.dart_tool', 'package_config.json'),
    );
    await config.parent.create(recursive: true);
    final dependencies = <Object?>[];
    if (flutter) {
      final flutterConfig = File(
        p.join(
          p.dirname(p.dirname(Directory.current.path)),
          'examples',
          'flutter_fixture',
          '.dart_tool',
          'package_config.json',
        ),
      );
      final contents = jsonDecode(
        await flutterConfig.readAsString(),
      ) as Map<String, Object?>;
      for (final package in contents['packages']! as List<Object?>) {
        final entry = Map<String, Object?>.from(package! as Map);
        if (entry['name'] == 'failforge_flutter_fixture') continue;
        entry['rootUri'] = flutterConfig.uri
            .resolve(entry['rootUri']! as String)
            .toString();
        dependencies.add(entry);
      }
    }
    await config.writeAsString(
      jsonEncode({
        'configVersion': 2,
        'packages': [
          ...dependencies,
          {
            'name': 'operator_fixture',
            'rootUri': '../',
            'packageUri': 'lib/',
            'languageVersion': '3.13',
          },
        ],
      }),
    );
    final overlay = OverlayResourceProvider(PhysicalResourceProvider.INSTANCE);
    overlay.setOverlay(path, content: source, modificationStamp: 0);
    final collection = AnalysisContextCollection(
      includedPaths: [directory.path],
      sdkPath: p.dirname(p.dirname(Platform.resolvedExecutable)),
      resourceProvider: overlay,
    );
    return _Fixture(directory, path, overlay, collection);
  }

  Future<ResolvedUnitResult> resolve(String source) async {
    final context = collection.contextFor(path);
    overlay.setOverlay(path, content: source, modificationStamp: ++stamp);
    context.changeFile(path);
    await context.applyPendingFileChanges();
    final result = await context.currentSession.getResolvedUnit(path);
    if (result is! ResolvedUnitResult) {
      throw StateError('The real analyzer did not resolve the fixture unit.');
    }
    return result;
  }

  Future<void> close() async {
    await collection.dispose();
    await directory.delete(recursive: true);
  }
}

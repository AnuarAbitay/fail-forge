import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:failforge_dart_adapter/failforge_dart_adapter.dart'
    hide classicOperators;
import 'package:failforge_dart_adapter/src/operators/registry.dart';
import 'package:failforge_dart_adapter/src/scope/scope_rules.dart' as scope;
import 'package:test/test.dart';

import '../support/project_fixture.dart';

// Independent examples from the requested catalogue, not generated from the
// registry. Every advertised transformation must discover and authenticate.
const examples = <String, String>{
  'RELATIONAL_BOUNDARY': 'bool f(int a, int b) => a > b;',
  'COMPARISON_NEGATION': 'bool f(int a, int b) => a > b;',
  'COMPARISON_OPERATOR_REPLACEMENT': 'bool f(int a, int b) => a > b;',
  'LOGICAL_CONNECTOR_SWAP': 'bool f(bool a, bool b) => a && b;',
  'LOGICAL_OPERAND_REMOVAL': 'bool f(bool a, bool b) => a && b;',
  'BOOLEAN_LITERAL_FLIP': 'bool f() => true;',
  'BOOLEAN_NEGATION': 'bool f(bool a) => a;',
  'ARITHMETIC_OPERATOR_REPLACEMENT': 'double f(double a, double b) => a + b;',
  'ARITHMETIC_OPERAND_REMOVAL': 'double f(double a, double b) => a + b;',
  'NUMERIC_NEGATION': 'int f(int a) => a;',
  'INCREMENT_DECREMENT_REPLACEMENT': 'int f(int a) => a++;',
  'COMPOUND_ASSIGNMENT_REPLACEMENT':
      'double f(double a, double b) { a += b; return a; }',
  'NUMERIC_LITERAL_REPLACEMENT': 'int f() => 10;',
  'STRING_LITERAL_REPLACEMENT': "String f() => 'text';",
  'RETURN_VALUE_REPLACEMENT': 'List<int> f() => [1, 2];',
  'CONDITION_FORCING': 'int f(bool a) { if (a) return 1; return 2; }',
  'BRANCH_BODY_REMOVAL':
      'void action() {} void f(bool a) { if (a) {action();} else {action();} }',
  'CONDITIONAL_EXPRESSION_MUTATION': 'int f(bool a) => a ? 1 : 2;',
  'STATEMENT_DELETION': 'void f() { int a = 0; a = 1; }',
  'FUNCTION_CALL_REMOVAL': 'void action() {} void f() { action(); }',
  'VARIABLE_REPLACEMENT': 'int f(int a, int b) => a;',
  'ARGUMENT_REPLACEMENT': 'int id(int a) => a; int f(int a, int b) => id(a);',
  'ARGUMENT_PERMUTATION':
      'int sum(int a, int b) => a - b; int f(int a, int b) => sum(a, b);',
  'BITWISE_OPERATOR_REPLACEMENT': 'int f(int a, int b) => a & b;',
  'BITWISE_NEGATION': 'int f(int a) => a;',
  'SHIFT_OPERATOR_REPLACEMENT': 'int f(int a, int b) => a << b;',
  'BREAK_CONTINUE_MUTATION': 'void f() { for (int i = 0; i < 3; i++) { if (i == 1) break; continue; } }',
  'SWITCH_CASE_MUTATION':
      'int f(int a) { switch (a) { case 1: return 1; default: return 0; } }',
  'THROW_REMOVAL': "void f() { throw StateError('bad'); }",
  'EXCEPTION_REPLACEMENT': "void f() { throw StateError('bad'); }",
  'COLLECTION_LITERAL_MUTATION': 'List<int> f() => [1, 2, 3];',
};

void main() {
  test('complete catalogue has 31 defaults, twelve categories and one legacy compatibility operator', () {
    expect(
      defaultClassicOperators.map((op) => op.id).toSet(),
      examples.keys.toSet(),
    );
    expect(
      operatorsByClassicalCategory.keys.toSet(),
      ClassicalCategory.values.toSet(),
    );
    expect(classicOperators, hasLength(32));
    expect(
      classicOperators.where((op) => op.isLegacy).single.id,
      'CONDITIONAL_NEGATION',
    );
    final seen = <String>{};
    for (final entry in operatorsByClassicalCategory.entries) {
      expect(entry.value, isNotEmpty);
      for (final op in entry.value) {
        expect(op.category, entry.key);
        expect(op.family, MutationFamily.classic);
        expect(seen.add(op.id), true);
      }
      expect(() => entry.value.clear(), throwsUnsupportedError);
    }
    expect(() => operatorsByClassicalCategory.clear(), throwsUnsupportedError);
    expect(() => defaultClassicOperators.clear(), throwsUnsupportedError);
    for (final category in ClassicalCategory.values) {
      expect(ClassicalCategory.fromWire(category.wireName), category);
    }
    for (final invalid in <Object?>[
      null,
      1,
      'Classical',
      'controlFlow',
      'returns',
    ]) {
      expect(
        () => ClassicalCategory.fromWire(invalid),
        throwsA(isA<AdapterException>()),
      );
    }
  });

  test('handshake exposes family/category/operator tree without changing mutant identity format', () async {
    final result = await AdapterEngine().handle('handshake', {});
    final tree = result['operatorTree'] as Map;
    expect(tree.keys.toSet(), {'classic', 'failure_mode'});
    final categories = tree['classic'] as Map;
    expect(
      categories.keys.toSet(),
      ClassicalCategory.values.map((c) => c.wireName).toSet(),
    );
    final all = categories.values
        .expand((value) => (value as List).cast<String>())
        .toSet();
    expect(all, examples.keys.toSet());
    expect(tree['failure_mode'], [
      'EXCEPTION_AT_ENTRY',
      'METHOD_CHANNEL_ERROR',
    ]);
    final metadata = (result['operators'] as List).cast<Map>();
    for (final op in classicOperators) {
      final descriptor = metadata.singleWhere((m) => m['id'] == op.id);
      expect(descriptor['category'], op.category!.wireName);
      expect(descriptor['legacy'] == true, op.isLegacy);
    }
    for (final op in failureModeOperators) {
      expect(
        metadata.singleWhere((m) => m['id'] == op.id).containsKey('category'),
        false,
      );
    }
  });

  test('scope and JSON schema use the same complete supported catalogue', () {
    final rules = scope.ScopeRules.fromJson({});
    for (final id in examples.keys) {
      expect(rules.match(file: 'lib/main.dart', operatorId: id).allowed, true);
    }
    for (final id in [
      'CONDITIONAL_NEGATION',
      'EXCEPTION_AT_ENTRY',
      'METHOD_CHANNEL_ERROR',
    ]) {
      expect(rules.match(file: 'lib/main.dart', operatorId: id).allowed, false);
    }
    expect(scope.supportedOperatorIds, {
      ...examples.keys,
      'CONDITIONAL_NEGATION',
      'EXCEPTION_AT_ENTRY',
      'METHOD_CHANNEL_ERROR',
    });
    final schema = jsonDecode(
      File('lib/src/protocol/schema.json').readAsStringSync(),
    );
    final enums = <Set<String>>[];
    void collect(Object? value) {
      if (value is Map) {
        final enumeration = value['enum'];
        if (enumeration is List &&
            enumeration.contains('BOOLEAN_LITERAL_FLIP')) {
          enums.add(enumeration.cast<String>().toSet());
        }
        value.values.forEach(collect);
      } else if (value is List) {
        value.forEach(collect);
      }
    }

    collect(schema);
    expect(enums, hasLength(3));
    for (final enumeration in enums) {
      expect(enumeration, scope.supportedOperatorIds);
    }
  });

  for (final entry in examples.entries) {
    test(
      '${entry.key}: discovery, byte patch, authentication and overlay restoration',
      () async {
        final fixture = ProjectFixture.create({
          'lib/main.dart': '// 😀 Кириллица\r\n${entry.value}\r\n',
        });
        addTearDown(fixture.close);
        final payload = {
          ...fixture.payload,
          'rules': {
            'operators': [entry.key],
          },
        };
        final file = File('${fixture.root}/lib/main.dart');
        final original = file.readAsBytesSync();
        final engine = AdapterEngine();
        final discovery = await engine.handle('discover', payload);
        final mutants = (discovery['mutants'] as List)
            .cast<Map<String, Object?>>();
        expect(
          mutants,
          isNotEmpty,
          reason: jsonEncode(discovery['diagnostics']),
        );
        expect(
          mutants.every(
            (m) => m['operatorId'] == entry.key && m['family'] == 'classic',
          ),
          true,
        );
        final patchedHashes = <String>{};
        final snapshot = SafeProject.open(payload).readTarget('lib/main.dart');
        for (final raw in mutants) {
          final mutant = Mutant.fromJson(raw);
          expect(
            patchedHashes.add(
              sha256.convert(mutant.edit.apply(snapshot)).toString(),
            ),
            true,
          );
        }
        // Unit suites check every variant; here we exercise the real trust boundary
        // once per advertised operator through the full discovery/validation API.
        final result = await engine.handle('validate', {
          ...payload,
          'mutant': mutants.first,
        });
        expect(result['valid'], isA<bool>());
        expect(
          result['valid'],
          result['syntaxValid'] == true && result['semanticValid'] == true,
        );
        expect(result['compileChecked'], false);
        final baselineCompilation = await _loadFiniteProgram(fixture, original);
        expect(baselineCompilation.$1, 0, reason: baselineCompilation.$2);
        final changed = Mutant.fromJson(mutants.first).edit.apply(snapshot);
        final mutantCompilation = await _loadFiniteProgram(fixture, changed);
        if (result['valid'] == true) {
          expect(mutantCompilation.$1, 0, reason: mutantCompilation.$2);
        } else {
          expect(
            mutantCompilation.$1,
            isNot(0),
            reason: 'Static INVALID must be a load/compile failure, never a killed assertion.',
          );
        }
        expect(file.readAsBytesSync(), original);
        expect((await engine.handle('discover', payload))['mutants'], mutants);
      },
    );
  }

  test('overlapping comparison variants merge before truncation and preserve the original boundary ID', () async {
    final fixture = ProjectFixture.create({
      'lib/main.dart': 'bool f(int a, int b) => a > b;',
    });
    addTearDown(fixture.close);
    final payload = {
      ...fixture.payload,
      'rules': {
        'operators': [
          'RELATIONAL_BOUNDARY',
          'COMPARISON_NEGATION',
          'COMPARISON_OPERATOR_REPLACEMENT',
        ],
      },
    };
    final engine = AdapterEngine();
    final boundary = await engine.handle('discover', {
      ...payload,
      'rules': {
        'operators': ['RELATIONAL_BOUNDARY'],
      },
    });
    final all = await engine.handle('discover', payload);
    expect(all['rawCandidates'], 7);
    expect(all['totalCandidates'], 5);
    expect(all['deduplicatedCandidates'], 2);
    final mutants = (all['mutants'] as List).cast<Map>();
    final original = (boundary['mutants'] as List).single as Map;
    expect(
      mutants.singleWhere((m) => (m['edit'] as Map)['replacement'] == '>='),
      original,
    );
    final limited = await engine.handle('discover', {
      ...payload,
      'limits': {'maxMutants': 1},
    });
    expect(limited['totalCandidates'], 5);
    expect(limited['mutants'], [mutants.first]);
    expect(limited['truncated'], true);
  });

  test('default discovery enables new categories and leaves legacy/failure modes opt-in', () async {
    final fixture = ProjectFixture.create({
      'lib/main.dart':
          'int f(int a, int b) { if (a > b) return a + 10; return 1; }',
    });
    addTearDown(fixture.close);
    final result = await AdapterEngine().handle('discover', fixture.payload);
    final ids = (result['mutants'] as List)
        .map((m) => (m as Map)['operatorId'])
        .toSet();
    expect(
      ids,
      containsAll([
        'ARITHMETIC_OPERATOR_REPLACEMENT',
        'NUMERIC_LITERAL_REPLACEMENT',
        'CONDITION_FORCING',
        'RELATIONAL_BOUNDARY',
      ]),
    );
    expect(
      ids.intersection({
        'CONDITIONAL_NEGATION',
        'EXCEPTION_AT_ENTRY',
        'METHOD_CHANNEL_ERROR',
      }),
      isEmpty,
    );
    expect(
      (result['mutants'] as List).every(
        (m) => (m as Map)['family'] == 'classic',
      ),
      true,
    );
  });
}

// Compile/load the complete baseline and changed library in the actual Dart VM.
// main is finite and never invokes the mutated loops or throws; assertion-driven
// KILLED/SURVIVED evidence belongs to the dedicated runtime/Flutter fixtures.
Future<(int, String)> _loadFiniteProgram(
  ProjectFixture fixture,
  List<int> bytes,
) async {
  final probe = File('${fixture.root}/compile_probe.dart');
  await probe.writeAsBytes([
    ...bytes,
    ...utf8.encode('\nvoid main() { print("loaded"); }\n'),
  ]);
  final process = await Process.start(Platform.resolvedExecutable, [
    '--packages=${fixture.root}/.dart_tool/package_config.json',
    probe.path,
  ], workingDirectory: fixture.root);
  var exited = false;
  final exit = process.exitCode.then((code) {
    exited = true;
    return code;
  });
  final output = process.stdout.transform(utf8.decoder).join();
  final errors = process.stderr.transform(utf8.decoder).join();
  try {
    final results = await Future.wait<Object>([exit, output, errors])
        .timeout(const Duration(seconds: 10));
    final code = results[0] as int;
    if (code == 0) expect((results[1] as String).trim(), 'loaded');
    return (code, results[2] as String);
  } finally {
    if (!exited) {
      process.kill(ProcessSignal.sigkill);
      await exit.timeout(const Duration(seconds: 5));
    }
    await probe.delete();
  }
}

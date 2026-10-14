import 'dart:convert';
import 'dart:io';

import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';
import 'package:failforge_dart_adapter/src/analysis/analysis_workspace.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  late ProjectFixture fixture;
  late AdapterEngine engine;
  setUp(() {
    fixture = ProjectFixture.create({
      'lib/main.dart': 'class C { bool flag() => true; }\n',
    });
    engine = AdapterEngine();
  });
  tearDown(() => fixture.close());

  Future<List<Mutant>> discover([Map<String, Object?>? extra]) async {
    final result = await engine.handle('discover', {
      ...fixture.payload,
      'rules': {
        'operators': [
          'BOOLEAN_LITERAL_FLIP',
          'CONDITIONAL_NEGATION',
          'RELATIONAL_BOUNDARY',
          'LOGICAL_CONNECTOR_SWAP',
        ],
      },
      ...?extra,
    });
    return (result['mutants'] as List)
        .map((value) => Mutant.fromJson(value as Map<String, Object?>))
        .toList();
  }

  Future<Map<String, Object?>> validate(
    Mutant mutant, [
    Map<String, Object?>? extra,
  ]) => engine.handle('validate', {
    ...fixture.payload,
    'rules': {
      'operators': [
        'BOOLEAN_LITERAL_FLIP',
        'CONDITIONAL_NEGATION',
        'RELATIONAL_BOUNDARY',
        'LOGICAL_CONNECTOR_SWAP',
      ],
    },
    ...?extra,
    'mutant': mutant.toJson(),
  });

  test('validation rejects forged component labels even when the byte patch and ID are authentic', () async {
    final rules = <String, Object?>{
      'components': [
        {
          'name': 'selected',
          'classes': ['C'],
          'operators': ['BOOLEAN_LITERAL_FLIP'],
        },
      ],
    };
    final original = (await discover({'rules': rules})).single;
    expect((await validate(original, {'rules': rules}))['valid'], isTrue);
    final forged = Mutant.create(
      file: original.file,
      sourceHash: original.sourceHash,
      operatorId: original.operatorId,
      family: original.family,
      edit: original.edit,
      className: original.className,
      methodName: original.methodName,
      components: ['invented'],
    );
    expect(
      forged.id,
      original.id,
      reason: 'components intentionally are outside the content ID',
    );
    await expectLater(
      validate(forged, {'rules': rules}),
      throwsA(isA<AdapterException>()),
    );
  });

  test('validation rejects forged class and method symbol metadata', () async {
    final original = (await discover()).single;
    for (final change in [
      (className: 'Other', methodName: original.methodName),
      (className: original.className, methodName: 'other'),
    ]) {
      final forged = Mutant.create(
        file: original.file,
        sourceHash: original.sourceHash,
        operatorId: original.operatorId,
        family: original.family,
        edit: original.edit,
        className: change.className,
        methodName: change.methodName,
      );
      expect(forged.id, original.id);
      await expectLater(validate(forged), throwsA(isA<AdapterException>()));
    }
  });

  test('validation respects the explicit file selection rather than replacing it with the mutant file', () async {
    fixture.write('lib/other.dart', 'bool other() => true;\n');
    final mutant = (await discover({
      'files': ['lib/other.dart'],
    })).single;
    expect(
      (await validate(mutant, {
        'files': ['lib/other.dart'],
      }))['valid'],
      isTrue,
    );
    await expectLater(
      validate(mutant, {
        'files': ['lib/main.dart'],
      }),
      throwsA(isA<AdapterException>()),
    );
  });

  for (final value in <Object?>[
    null,
    1,
    [1],
  ]) {
    test(
      'validate rejects malformed files selection ${jsonEncode(value)}',
      () async {
        final mutant = (await discover()).single;
        await expectLater(
          validate(mutant, {'files': value}),
          throwsA(isA<AdapterException>()),
        );
      },
    );
  }

  for (final command in ['discover', 'validate']) {
    test(
      '$command rejects an explicit null maxMutants rather than silently using its default',
      () async {
        final payload = {
          ...fixture.payload,
          'limits': {'maxMutants': null},
          if (command == 'validate')
            'mutant': (await discover()).single.toJson(),
        };
        await expectLater(
          engine.handle(command, payload),
          throwsA(isA<AdapterException>()),
        );
      },
    );
  }

  test('a generated-header phrase inside a Dart string is not a generated file header', () async {
    fixture.write(
      'lib/main.dart',
      "const note = 'GENERATED CODE - DO NOT MODIFY BY HAND';\nbool flag() => true;\n",
    );
    expect(await discover(), hasLength(1));
  });

  test(
    'diagnostic limit is enforced after the final operator in a single source',
    () async {
      fixture.write(
        'lib/main.dart',
        'const values = <bool>[${List.filled(10001, 'true').join(',')}];\n',
      );
      await expectLater(
        engine.handle('discover', {
          ...fixture.payload,
          'rules': {
            'operators': ['BOOLEAN_LITERAL_FLIP'],
          },
        }),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'diagnostic_limit_exceeded',
          ),
        ),
      );
    },
  );

  test('a new request observes dependency semantics despite unchanged target bytes', () async {
    fixture.write(
      'lib/main.dart',
      "import 'dependency.dart';\nbool flag() => true;\nbool greater() => value > 0;\n",
    );
    fixture.write('lib/dependency.dart', 'int value = 1;\n');
    final first = await discover({
      'files': ['lib/main.dart'],
    });
    final oldBoolean = first.singleWhere(
      (mutant) => mutant.operatorId == 'BOOLEAN_LITERAL_FLIP',
    );
    expect(
      first.any((mutant) => mutant.operatorId == 'RELATIONAL_BOUNDARY'),
      isTrue,
    );
    fixture.write(
      'lib/dependency.dart',
      'class Value { bool operator >(int other) => true; }\nValue value = Value();\n',
    );
    final second = await discover({
      'files': ['lib/main.dart'],
    });
    expect(
      second.any((mutant) => mutant.operatorId == 'RELATIONAL_BOUNDARY'),
      isFalse,
    );
    expect(second.single.id, oldBoolean.id);
  });

  test('a new request observes changed package language version rather than a hash-only AST cache', () async {
    fixture.write(
      'lib/main.dart',
      'bool flag(Object value) { if (value case true) return true; return false; }\n',
    );
    expect(await discover(), isNotEmpty);
    final config = File('${fixture.root}/.dart_tool/package_config.json');
    final json = jsonDecode(config.readAsStringSync()) as Map<String, Object?>;
    ((json['packages'] as List).single as Map)['languageVersion'] = '2.19';
    config.writeAsStringSync(jsonEncode(json));
    await expectLater(
      discover(),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'baseline_analysis_errors',
        ),
      ),
    );
  });

  test('existing semantic errors in another part stay baseline diagnostics and do not become syntax errors', () async {
    fixture.write(
      'lib/main.dart',
      "part 'part.dart';\nclass C { bool flag() => true; }\n",
    );
    fixture.write(
      'lib/part.dart',
      "part of 'main.dart';\nint partValue = 1;\n",
    );
    final original = (await discover({
      'files': ['lib/main.dart'],
    })).single;
    fixture.write(
      'lib/part.dart',
      "part of 'main.dart';\nint partValue = true;\n",
    );
    final result = await validate(original, {
      'files': ['lib/main.dart'],
    });
    expect(result['valid'], isFalse);
    expect(result['reason'], 'baseline_analysis_errors');
    expect(result['diagnostics'], isEmpty);
    expect(result['baselineDiagnostics'] as List, isNotEmpty);
    expect(
      (result['baselineDiagnostics'] as List).any(
        (value) => (value as Map)['file'] == 'lib/part.dart',
      ),
      isTrue,
    );
    expect(
      result['syntaxValid'],
      isTrue,
      reason: 'the existing error is semantic; both units parse successfully',
    );
    expect(result['semanticValid'], isFalse);
    expect(result['compileChecked'], isFalse);
  });

  test('an invalid overlay cannot contaminate the next valid mutation in the same analysis workspace', () async {
    fixture.write(
      'lib/main.dart',
      'int length(String? value) { if (value != null) return value.length; return 0; }\nbool flag() => true;\n',
    );
    final mutants = await discover();
    final invalid = mutants.singleWhere(
      (mutant) => mutant.operatorId == 'CONDITIONAL_NEGATION',
    );
    final valid = mutants.singleWhere(
      (mutant) => mutant.operatorId == 'BOOLEAN_LITERAL_FLIP',
    );
    final project = SafeProject.open(fixture.payload);
    final source = project.readTarget('lib/main.dart');
    final workspace = AnalysisWorkspace.open(project, [source]);
    try {
      for (var round = 0; round < 2; round++) {
        expect((await workspace.validate(source, invalid))['valid'], isFalse);
        final result = await workspace.validate(source, valid);
        expect(result['valid'], isTrue);
        expect(result['baselineDiagnostics'], isEmpty);
        final restored = await workspace.library(source);
        expect(
          restored.unitWithPath(source.absolutePath)!.content,
          source.analysisText,
        );
      }
    } finally {
      await workspace.dispose();
    }
  });

  test('analysis-options suppression cannot turn a new compiler type error into semantic success', () async {
    fixture.write(
      'analysis_options.yaml',
      'analyzer:\n  errors:\n    unchecked_use_of_nullable_value: ignore\n',
    );
    fixture.write(
      'lib/main.dart',
      'int length(String? value) { if (value != null) return value.length; return 0; }\n',
    );
    final mutant = (await discover()).singleWhere(
      (mutant) => mutant.operatorId == 'CONDITIONAL_NEGATION',
    );
    final result = await validate(mutant);
    expect(result['valid'], isFalse);
    expect(result['syntaxValid'], isTrue);
    expect(result['semanticValid'], isFalse);
  });

  test('an explicitly supplied Flutter SDK must contain an installation rather than one marker file', () async {
    final fake = '${fixture.root}/fake-flutter';
    fixture.write(
      'fake-flutter/packages/flutter/lib/src/services/platform_channel.dart',
      '// marker only\n',
    );
    await expectLater(
      engine.handle('discover', {...fixture.payload, 'flutterSdkPath': fake}),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'sdk_missing',
        ),
      ),
    );
  });
}

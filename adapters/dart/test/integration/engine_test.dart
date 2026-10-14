import 'dart:convert';
import 'dart:io';

import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';
import 'package:failforge_dart_adapter/src/analysis/analysis_workspace.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

const subject = '''// Кириллица 😀
class C {
  bool flag() => true;
  bool boundary(int n) => n >= 10;
  bool both(bool a, bool b) => a && b;
}
int length(String? value) {
  if (value != null) return value.length;
  return 0;
}
bool topLevel() => false;
''';

const legacyOperatorIds = [
  'BOOLEAN_LITERAL_FLIP',
  'CONDITIONAL_NEGATION',
  'RELATIONAL_BOUNDARY',
  'LOGICAL_CONNECTOR_SWAP',
];

void main() {
  late ProjectFixture fixture;
  late AdapterEngine engine;
  setUp(() {
    fixture = ProjectFixture.create({'lib/main.dart': subject});
    engine = AdapterEngine();
  });
  tearDown(() => fixture.close());

  Map<String, Object?> legacyPayload() => {
    ...fixture.payload,
    'rules': {'operators': legacyOperatorIds},
  };

  Future<List<Map<String, Object?>>> discover([
    Map<String, Object?>? extra,
  ]) async {
    final result = await engine.handle('discover', {
      ...legacyPayload(),
      ...?extra,
    });
    return (result['mutants'] as List).cast<Map<String, Object?>>();
  }

  test('handshake declares catalogue, compatibility operator and opt-in failure modes', () async {
    final result = await engine.handle('handshake', {});
    expect(result['operators'], hasLength(34));
    expect(
      (result['operators'] as List).where(
        (value) => (value as Map)['family'] == 'classic',
      ),
      hasLength(32),
    );
    expect(result['testExecution'], false);
    await expectLater(
      engine.handle('unknown', {}),
      throwsA(isA<AdapterException>()),
    );
  });

  test('discovers all P0 families with exact byte applicability', () async {
    final mutants = await discover();
    expect(mutants.map((m) => m['operatorId']).toSet(), {
      'BOOLEAN_LITERAL_FLIP',
      'CONDITIONAL_NEGATION',
      'RELATIONAL_BOUNDARY',
      'LOGICAL_CONNECTOR_SWAP',
    });
    final source = SafeProject.open(fixture.payload)
        .readTarget('lib/main.dart');
    for (final json in mutants) {
      final mutant = Mutant.fromJson(json);
      final changed = mutant.edit.apply(source);
      expect(changed, isNot(equals(source.rawBytes)));
      expect(utf8.decode(changed), contains(mutant.edit.replacement));
    }
  });

  test('nullable promotion loss is invalid; other operators validate; originals unchanged', () async {
    final file = File('${fixture.root}/lib/main.dart');
    final before = file.readAsBytesSync();
    for (final mutant in await discover()) {
      final result = await engine.handle('validate', {
        ...legacyPayload(),
        'mutant': mutant,
      });
      expect(result['valid'], mutant['operatorId'] != 'CONDITIONAL_NEGATION');
      expect(result['compileChecked'], false);
      if (mutant['operatorId'] == 'CONDITIONAL_NEGATION') {
        expect(result['syntaxValid'], true);
        expect(result['semanticValid'], false);
        expect(result['diagnostics'] as List, isNotEmpty);
      }
      expect(file.readAsBytesSync(), before);
    }
  });

  test(
    'multiple overlays restore library state before each candidate',
    () async {
      final mutants = (await discover())
          .map(Mutant.fromJson)
          .where((m) => m.operatorId == 'BOOLEAN_LITERAL_FLIP')
          .toList();
      final project = SafeProject.open(fixture.payload);
      final source = project.readTarget('lib/main.dart');
      final workspace = AnalysisWorkspace.open(project, [source]);
      try {
        for (final mutant in [...mutants, ...mutants.reversed]) {
          expect((await workspace.validate(source, mutant))['valid'], true);
          final restored = await workspace.library(source);
          expect(
            restored.unitWithPath(source.absolutePath)!.content,
            source.analysisText,
          );
        }
      } finally {
        await workspace.dispose();
      }
    },
  );

  test('whole part library validation detects promotion loss', () async {
    fixture.write(
      'lib/main.dart',
      "part 'part.dart';\nint read(String? v) => readImpl(v);\n",
    );
    fixture.write(
      'lib/part.dart',
      "part of 'main.dart';\nint readImpl(String? v) { if (v != null) return v.length; return 0; }\n",
    );
    final mutants = await discover();
    final mutant = mutants.singleWhere(
      (m) => m['operatorId'] == 'CONDITIONAL_NEGATION',
    );
    expect(mutant['file'], 'lib/part.dart');
    final result = await engine.handle('validate', {
      ...legacyPayload(),
      'mutant': mutant,
    });
    expect(result['valid'], false);
    expect(result['baselineDiagnostics'], isEmpty);
  });

  test(
    'discovery is repeatable and independent of absolute snapshot path',
    () async {
      final first = await discover();
      expect(await discover(), first);
      final other = ProjectFixture.create({'lib/main.dart': subject});
      try {
        final result = await engine.handle('discover', {
          ...other.payload,
          'rules': {'operators': legacyOperatorIds},
        });
        expect(result['mutants'], first);
      } finally {
        other.close();
      }
    },
  );

  test(
    'filter, dedup and sort precede maxMutants with explicit truncation',
    () async {
      final all = await discover();
      final result = await engine.handle('discover', {
        ...legacyPayload(),
        'limits': {'maxMutants': 1},
      });
      expect(result['mutants'], [all.first]);
      expect(result['truncated'], true);
      expect(result['totalCandidates'], all.length);
      final zero = await engine.handle('discover', {
        ...legacyPayload(),
        'limits': {'maxMutants': 0},
      });
      expect(zero['mutants'], isEmpty);
      expect(zero['truncated'], true);
    },
  );

  test('component intersections union operator sets and names; top-level excluded by class', () async {
    final result = await discover({
      'rules': {
        'components': [
          {
            'name': 'one',
            'classes': ['C'],
            'operators': ['BOOLEAN_LITERAL_FLIP'],
          },
          {
            'name': 'two',
            'methods': ['flag'],
            'operators': ['BOOLEAN_LITERAL_FLIP'],
          },
          {
            'name': 'three',
            'classes': ['C'],
            'operators': ['RELATIONAL_BOUNDARY'],
          },
        ],
      },
    });
    final flag = result.singleWhere(
      (m) => (m['symbol'] as Map)['methodName'] == 'flag',
    );
    expect(flag['components'], ['one', 'three', 'two']);
    expect(
      result.any((m) => (m['symbol'] as Map)['methodName'] == 'topLevel'),
      false,
    );
    expect(result.map((m) => m['operatorId']).toSet(), {
      'BOOLEAN_LITERAL_FLIP',
      'RELATIONAL_BOUNDARY',
    });
  });

  test('explicit empty operator set produces no candidates', () async {
    expect(
      await discover({
        'rules': {'operators': <String>[]},
      }),
      isEmpty,
    );
  });

  test(
    'BOM CRLF Unicode offsets work through analyzer and validation',
    () async {
      fixture.write(
        'lib/main.dart',
        '\uFEFF// Привет 😀\r\nbool f() => true;\r\n',
      );
      final mutants = await discover();
      expect(mutants, hasLength(1));
      final mutant = Mutant.fromJson(mutants.single);
      final bytes = File('${fixture.root}/lib/main.dart').readAsBytesSync();
      expect(
        bytes.sublist(
          mutant.edit.offsetBytes,
          mutant.edit.offsetBytes + mutant.edit.lengthBytes,
        ),
        utf8.encode('true'),
      );
      expect(
        (await engine.handle('validate', {
          ...legacyPayload(),
          'mutant': mutant.toJson(),
        }))['valid'],
        true,
      );
      expect(File('${fixture.root}/lib/main.dart').readAsBytesSync(), bytes);
    },
  );

  test('changed snapshot and forged AST patch are rejected', () async {
    final json = (await discover()).first;
    fixture.write('lib/main.dart', '$subject\n// changed\n');
    await expectLater(
      engine.handle('validate', {...legacyPayload(), 'mutant': json}),
      throwsA(isA<AdapterException>()),
    );
    fixture.write('lib/main.dart', subject);
    final original = Mutant.fromJson(json);
    final forged = Mutant.create(
      file: original.file,
      sourceHash: original.sourceHash,
      operatorId: original.operatorId,
      family: original.family,
      className: original.className,
      methodName: original.methodName,
      edit: ByteEdit(
        offsetBytes: original.edit.offsetBytes,
        lengthBytes: original.edit.lengthBytes,
        expected: original.edit.expected,
        replacement: '42',
      ),
    );
    await expectLater(
      engine.handle('validate', {
        ...legacyPayload(),
        'mutant': forged.toJson(),
      }),
      throwsA(isA<AdapterException>()),
    );
  });

  test(
    'baseline semantic errors return diagnostic error, never empty success',
    () async {
      fixture.write('lib/main.dart', 'int f() => true;\n');
      await expectLater(discover(), throwsA(isA<AdapterException>()));
    },
  );
}

import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';
import 'package:failforge_dart_adapter/src/analysis/analysis_workspace.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/null_safety/null_safety.dart';
import 'package:failforge_dart_adapter/src/operators/registry.dart'
    show languageSpecificOperators;
import 'package:test/test.dart';

import '../support/project_fixture.dart';

// This expected catalogue is independent of the registry and its grouping code.
const expected = <String, (String, String)>{
  'NSM-001': ('null_safety', 'NullCoalescingFallbackRemoval'),
  'NSM-002': ('null_safety', 'NullAwareAccessReplacement'),
  'NSM-003': ('null_safety', 'NullableValueReplacement'),
  'NSM-004': ('null_safety', 'NullAwareAssignmentMutation'),
  'NSM-005': ('null_safety', 'NullAwareSpreadRemoval'),
  'DPM-001': ('parameters', 'DefaultParameterReplacement'),
  'DPM-002': ('parameters', 'BooleanDefaultInversion'),
  'DPM-003': ('parameters', 'NullableDefaultReplacement'),
  'SWM-001': ('switch', 'SwitchBranchResultReplacement'),
  'SWM-002': ('switch', 'SwitchExpressionReplacement'),
  'SWM-003': ('switch', 'SwitchGuardMutation'),
  'PMM-001': ('patterns', 'PatternConstraintMutation'),
  'PMM-002': ('patterns', 'PatternGuardInversion'),
  'PMM-003': ('patterns', 'LogicalPatternMutation'),
  'CLM-001': ('collections', 'CollectionIfInversion'),
  'CLM-002': ('collections', 'CollectionSpreadRemoval'),
  'CLM-003': ('collections', 'CollectionIfElementRemoval'),
  'CLM-004': ('collections', 'CollectionForRemoval'),
  'ENM-001': ('enums', 'EnumValueReplacement'),
};

Map<String, Object?> payload(ProjectFixture fixture, String operator) => {
  ...fixture.payload,
  'rules': {
    'operators': [operator],
    'methods': ['subject'],
  },
};

Future<(ProjectFixture, Map<String, Object?>, Map<String, Object?>)> discover(
  String source,
  String operator,
) async {
  final fixture = ProjectFixture.create({'lib/main.dart': source});
  addTearDown(fixture.close);
  final request = payload(fixture, operator);
  return (fixture, request, await AdapterEngine().handle('discover', request));
}

void main() {
  final fixtureDocument = jsonDecode(
    File('../../protocol/fixtures/dart-semantic-catalog.json')
        .readAsStringSync(),
  ) as Map;
  final cases = (fixtureDocument['cases'] as List).cast<Map>();

  test(
    'all nineteen Dart operators have independent classes and categories',
    () {
      expect(fixtureDocument['family'], 'language_specific');
      expect(fixtureDocument['language'], 'dart');
      expect(cases, hasLength(19));
      expect(
        cases.map((row) => row['operatorId']).toSet(),
        expected.keys.toSet(),
      );
      expect(languageSpecificOperators, hasLength(19));
      expect(
        languageSpecificOperators.map((operator) => operator.id).toSet(),
        expected.keys.toSet(),
      );
      expect(
        languageSpecificOperators
            .map((operator) => operator.runtimeType)
            .toSet(),
        hasLength(19),
      );
      for (final operator in languageSpecificOperators) {
        expect(operator.runtimeType.toString(), expected[operator.id]!.$2);
        expect(operator.category.wireName, expected[operator.id]!.$1);
        expect(operator.family, MutationFamily.languageSpecific);
        expect(operator.language, 'dart');
        expect(operator.version, 1);
      }
      expect(() => languageSpecificOperators.clear(), throwsUnsupportedError);
    },
  );

  test(
    'handshake places semantic categories under language_specific / dart',
    () async {
      final handshake = await AdapterEngine().handle('handshake', {});
      final catalogue = (handshake['operators'] as List).cast<Map>();
      expect(catalogue, hasLength(53));
      final semantic = catalogue
          .where((row) => row['family'] == 'language_specific')
          .toList();
      expect(semantic, hasLength(19));
      expect(semantic.map((row) => row['id']).toSet(), expected.keys.toSet());
      for (final row in semantic) {
        expect(row['language'], 'dart');
        expect(row['category'], expected[row['id']]!.$1);
        expect(row['version'], 1);
      }
      final tree =
          (handshake['operatorTree'] as Map)['language_specific'] as Map;
      expect(tree.keys.toSet(), {'dart'});
      final dart = tree['dart'] as Map;
      expect(dart.keys.toSet(), expected.values.map((row) => row.$1).toSet());
      for (final category in dart.keys) {
        expect((dart[category] as List).toSet(), {
          for (final entry in expected.entries)
            if (entry.value.$1 == category) entry.key,
        });
      }
      expect(
        catalogue.where((row) => row['family'] == 'classic'),
        hasLength(32),
      );
      expect(
        catalogue.where((row) => row['family'] == 'failure_mode'),
        hasLength(2),
      );
    },
  );

  for (final example in cases) {
    final id = example['operatorId'] as String;
    test(
      '$id: scoped semantic publication, canonical bytes, validation and restoration',
      () async {
        final text = example['source'] as String;
        expect(example['category'], expected[id]!.$1);
        final (fixture, request, result) = await discover(text, id);
        final file = File('${fixture.root}/lib/main.dart');
        final original = file.readAsBytesSync();
        expect(original, utf8.encode(text));
        final mutants = (result['mutants'] as List)
            .cast<Map<String, Object?>>();
        expect(mutants, isNotEmpty, reason: jsonEncode(result['diagnostics']));
        expect(result['truncated'], false);
        expect(result['returnedCandidates'], mutants.length);
        final source = SafeProject.open(request).readTarget('lib/main.dart');
        final ids = <String>{};
        final outputs = <String>{};
        for (final raw in mutants) {
          expect(raw.keys.toSet(), {
            'id',
            'operatorId',
            'operatorVersion',
            'family',
            'file',
            'sourceHash',
            'components',
            'edit',
            'symbol',
            'validation',
          });
          expect(raw['operatorId'], id);
          expect(raw['operatorVersion'], 1);
          expect(raw['family'], 'language_specific');
          expect(raw['validation'], 'unvalidated');
          expect(raw['sourceHash'], sha256.convert(original).toString());
          expect((raw['symbol'] as Map)['methodName'], 'subject');
          final mutant = Mutant.fromJson(raw);
          expect(ids.add(mutant.id), true);
          final canonical = jsonEncode([
            1,
            mutant.file,
            mutant.sourceHash,
            mutant.operatorId,
            mutant.operatorVersion,
            'language_specific',
            mutant.edit.offsetBytes,
            mutant.edit.lengthBytes,
            mutant.edit.expected,
            mutant.edit.replacement,
          ]);
          expect(mutant.id, sha256.convert(utf8.encode(canonical)).toString());
          expect(
            utf8.encode(mutant.edit.expected).length,
            mutant.edit.lengthBytes,
          );
          final changed = mutant.edit.apply(source);
          expect(outputs.add(sha256.convert(changed).toString()), true);
          expect(
            changed.sublist(0, mutant.edit.offsetBytes),
            original.sublist(0, mutant.edit.offsetBytes),
          );
          expect(
            changed.sublist(
              mutant.edit.offsetBytes +
                  utf8.encode(mutant.edit.replacement).length,
            ),
            original.sublist(mutant.edit.offsetBytes + mutant.edit.lengthBytes),
          );
          final validation = await AdapterEngine().handle('validate', {
            ...request,
            'mutant': raw,
          });
          expect(validation['mutantId'], mutant.id);
          expect(validation['valid'], true, reason: jsonEncode(validation));
          expect(validation['syntaxValid'], true);
          expect(validation['semanticValid'], true);
          expect(validation['compileChecked'], false);
          expect(raw['validation'], 'unvalidated');
          expect(file.readAsBytesSync(), original);
        }
        expect(
          (await AdapterEngine().handle('discover', request))['mutants'],
          mutants,
        );
        expect(file.readAsBytesSync(), original);
      },
    );
  }

  test(
    'collection-if promotion loss is filtered before a mutant is published',
    () async {
      const source =
          'List<int> subject(int? value) => [if (value != null) value];\n';
      final (fixture, _, result) = await discover(source, 'CLM-001');
      expect(result['mutants'], isEmpty);
      expect(
        (result['diagnostics'] as List).whereType<Map>().any(
          (row) =>
              row['operatorId'] == 'CLM-001' &&
              row['code'] == 'semantic_candidate_rejected',
        ),
        true,
      );
      expect(File('${fixture.root}/lib/main.dart').readAsStringSync(), source);
    },
  );

  test(
    'coalescing removal cannot publish a nullable value in a non-null return',
    () async {
      const source = 'String subject(String? value) => value ?? "fallback";\n';
      final (fixture, _, result) = await discover(source, 'NSM-001');
      expect(result['mutants'], isEmpty);
      expect(
        (result['diagnostics'] as List).whereType<Map>().any(
          (row) =>
              row['operatorId'] == 'NSM-001' &&
              row['code'] == 'semantic_candidate_rejected',
        ),
        true,
      );
      expect(File('${fixture.root}/lib/main.dart').readAsStringSync(), source);
    },
  );

  test(
    'unchanged baseline warnings are not promoted to global semantic errors',
    () async {
      const source = '''
String? subject(String? value) {
  if (false) { print('unreachable'); }
  final unused = 7;
  return value ?? 'fallback';
}
''';
      final (_, request, result) = await discover(source, 'NSM-001');
      expect(
        (result['diagnostics'] as List).whereType<Map>().any(
          (row) => (row['code'] as String).endsWith('dead_code'),
        ),
        true,
      );
      final mutants = (result['mutants'] as List).cast<Map<String, Object?>>();
      expect(mutants, hasLength(1));
      expect(
        (await AdapterEngine().handle('validate', {
          ...request,
          'mutant': mutants.single,
        }))['valid'],
        true,
      );
    },
  );

  test('a guard that makes a later switch arm unreachable is filtered', () async {
    const source =
        'int subject(int value) => switch(value) { int n when false => 1, _ => 2 };\n';
    final (_, _, result) = await discover(source, 'SWM-003');
    expect(result['mutants'], isEmpty);
    expect(
      (result['diagnostics'] as List).whereType<Map>().any(
        (row) => row['operatorId'] == 'SWM-003',
      ),
      true,
    );
  });

  test(
    'baseline analysis failure is explicit and leaves original source intact',
    () async {
      const source = 'int subject() => null;\n';
      final fixture = ProjectFixture.create({'lib/main.dart': source});
      addTearDown(fixture.close);
      await expectLater(
        AdapterEngine().handle('discover', payload(fixture, 'NSM-003')),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'baseline_analysis_errors',
          ),
        ),
      );
      expect(File('${fixture.root}/lib/main.dart').readAsStringSync(), source);
    },
  );

  test('failed semantic overlay validation restores content before a later valid candidate', () async {
    const text = '''
String subject(String? input) => input ?? 'required';
String? optional(String? input) => input ?? 'optional';
''';
    final fixture = ProjectFixture.create({'lib/main.dart': text});
    addTearDown(fixture.close);
    final project = SafeProject.open(fixture.payload);
    final source = project.readTarget('lib/main.dart');
    final workspace = AnalysisWorkspace.open(project, [source]);
    try {
      final library = await workspace.library(source);
      final unit = library.unitWithPath(source.absolutePath)!;
      final drafts = const NullCoalescingFallbackRemoval()
          .discover(unit, source.analysisText)
          .candidates;
      expect(drafts, hasLength(2));
      for (final index in [0, 1, 0, 1]) {
        final draft = drafts[index];
        final mutant = Mutant.create(
          file: source.relativePath,
          sourceHash: source.sourceHash,
          operatorId: draft.operatorId,
          family: draft.family,
          edit: source.editFromAnalyzer(
            draft.offset,
            draft.length,
            draft.replacement,
          ),
        );
        final validation = await workspace.validate(source, mutant);
        expect(validation['valid'], index == 1);
        final restored = await workspace.library(source);
        expect(
          restored.unitWithPath(source.absolutePath)!.content,
          source.analysisText,
        );
        expect(File('${fixture.root}/lib/main.dart').readAsStringSync(), text);
      }
    } finally {
      await workspace.dispose();
    }
  });
}

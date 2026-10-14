import 'dart:convert';
import 'dart:io';

import 'package:failforge_dart_adapter/failforge_dart_adapter.dart'
    hide classicOperators;
import 'package:failforge_dart_adapter/src/operators/registry.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  test('wire values are explicit and independent of enum case names', () {
    expect(MutationFamily.values.map((family) => family.wireName), [
      'classic',
      'failure_mode',
    ]);
    expect(MutationFamily.failureMode.name, 'failureMode');
    for (final family in MutationFamily.values) {
      expect(MutationFamily.fromWire(family.wireName), family);
    }
  });

  test('unknown, aliased and non-string wire families reject', () {
    for (final value in <Object?>[
      null,
      true,
      1,
      <Object?>[],
      <String, Object?>{},
      '',
      'Classic',
      'failureMode',
      'failure-mode',
      'other',
    ]) {
      expect(
        () => MutationFamily.fromWire(value),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'invalid_family',
          ),
        ),
      );
    }
  });

  test('catalog groups every unique operator under exactly two families', () {
    expect(operatorsByFamily.keys.toSet(), MutationFamily.values.toSet());
    expect(operatorsByFamily[MutationFamily.classic]!, hasLength(32));
    expect(defaultClassicOperators, hasLength(31));
    expect(
      classicOperators.where((operator) => operator.isLegacy).single.id,
      'CONDITIONAL_NEGATION',
    );
    expect(
      operatorsByFamily[MutationFamily.failureMode]!
          .map((operator) => operator.id)
          .toSet(),
      {'EXCEPTION_AT_ENTRY', 'METHOD_CHANNEL_ERROR'},
    );
    final ids = <String>{};
    for (final group in operatorsByFamily.entries) {
      for (final operator in group.value) {
        expect(operator.family, group.key);
        expect(
          ids.add(operator.id),
          true,
          reason: 'An operator belongs to one family.',
        );
      }
    }
    expect(ids, hasLength(34));
    expect(
      () => operatorsByFamily[MutationFamily.classic]!.clear(),
      throwsUnsupportedError,
    );
    expect(() => operatorsByFamily.clear(), throwsUnsupportedError);
  });

  test('handshake preserves flat operator wire contract while using family catalog', () async {
    final handshake = await AdapterEngine().handle('handshake', {});
    final groups = <MutationFamily, Set<String>>{};
    for (final raw in handshake['operators'] as List) {
      final operator = raw as Map<String, Object?>;
      final family = MutationFamily.fromWire(operator['family']);
      groups.putIfAbsent(family, () => {}).add(operator['id'] as String);
      expect(operator['family'], isA<String>());
    }
    for (final group in operatorsByFamily.entries) {
      expect(
        groups[group.key],
        group.value.map((operator) => operator.id).toSet(),
      );
    }
  });

  for (final file in ['patch.json', 'patch-unicode-expression.json']) {
    test(
      'typed family preserves shared canonical ID and byte patch: $file',
      () {
        final fixture = jsonDecode(
          File('../../protocol/fixtures/$file').readAsStringSync(),
        ) as Map<String, Object?>;
        final raw = fixture['mutant'] as Map<String, Object?>;
        final mutant = Mutant.fromJson(raw);
        expect(mutant.family, MutationFamily.classic);
        expect(mutant.id, raw['id']);
        expect(mutant.toJson(), raw);
        final source = SourceFile(
          mutant.file,
          '/snapshot/${mutant.file}',
          utf8.encode(fixture['sourceUtf8'] as String),
        );
        expect(
          mutant.edit.apply(source),
          utf8.encode(fixture['patchedUtf8'] as String),
        );
        final altered = {...raw, 'family': 'failure_mode'};
        expect(
          () => Mutant.fromJson(altered),
          throwsA(isA<AdapterException>()),
        );
      },
    );
  }

  test('family is bound to identity; result metadata does not change it', () {
    final source = SourceFile(
      'lib/a.dart',
      '/snapshot/lib/a.dart',
      utf8.encode('true'),
    );
    Mutant make(MutationFamily family, {String validation = 'unvalidated'}) =>
        Mutant.create(
          file: source.relativePath,
          sourceHash: source.sourceHash,
          operatorId: 'BOOLEAN_LITERAL_FLIP',
          family: family,
          edit: source.editFromUtf16(0, 4, 'false'),
          validation: validation,
        );
    final classic = make(MutationFamily.classic);
    final failureMode = make(MutationFamily.failureMode);
    expect(classic.id, isNot(failureMode.id));
    for (final family in MutationFamily.values) {
      final original = make(family);
      for (final validation in ['valid', 'invalid']) {
        final changed = make(family, validation: validation);
        expect(changed.family, family);
        expect(changed.id, original.id);
        expect(changed.toJson()['family'], family.wireName);
      }
    }
  });

  for (final originalFamily in MutationFamily.values) {
    test(
      'validation rejects relabelled ${originalFamily.wireName} operator even with a recomputed ID',
      () async {
        final fixture = ProjectFixture.create({
          'lib/main.dart': 'bool flag() => true;\n',
        });
        try {
          final engine = AdapterEngine();
          final operatorId = originalFamily == MutationFamily.classic
              ? 'BOOLEAN_LITERAL_FLIP'
              : 'EXCEPTION_AT_ENTRY';
          final payload = {
            ...fixture.payload,
            'rules': {
              'methods': ['flag'],
              'operators': [operatorId],
            },
          };
          final discovery = await engine.handle('discover', payload);
          final original = Mutant.fromJson(
            (discovery['mutants'] as List).single as Map<String, Object?>,
          );
          expect(original.family, originalFamily);
          final relabelled = Mutant.create(
            file: original.file,
            sourceHash: original.sourceHash,
            operatorId: original.operatorId,
            operatorVersion: original.operatorVersion,
            family: originalFamily == MutationFamily.classic
                ? MutationFamily.failureMode
                : MutationFamily.classic,
            edit: original.edit,
            components: original.components,
            className: original.className,
            methodName: original.methodName,
          );
          expect(relabelled.id, isNot(original.id));
          await expectLater(
            engine.handle('validate', {
              ...payload,
              'mutant': relabelled.toJson(),
            }),
            throwsA(
              isA<AdapterException>().having(
                (error) => error.code,
                'code',
                'unsupported_candidate',
              ),
            ),
          );
        } finally {
          fixture.close();
        }
      },
    );
  }
}

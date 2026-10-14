import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  late ProjectFixture fixture;
  late AdapterEngine engine;
  setUp(() {
    fixture = ProjectFixture.create({
      'lib/main.dart':
          'class C { bool flag() => true; }\nFuture<int> load() async => 42;\n',
    });
    engine = AdapterEngine();
  });
  tearDown(() => fixture.close());
  Future<Map<String, Object?>> discover(Map<String, Object?> rules) =>
      engine.handle('discover', {...fixture.payload, 'rules': rules});

  test(
    'failure modes default off; explicit operator still needs positive scope',
    () async {
      final defaults = await discover({});
      expect(
        (defaults['mutants'] as List).any(
          (value) => (value as Map)['family'] == 'failure_mode',
        ),
        false,
      );
      final unscoped = await discover({
        'operators': ['EXCEPTION_AT_ENTRY'],
      });
      expect(unscoped['mutants'], isEmpty);
      expect(
        (unscoped['diagnostics'] as List).any(
          (value) => (value as Map)['code'] == 'explicit_scope_required',
        ),
        true,
      );
    },
  );
  test(
    'scoped sync and async entry injection validate without source writes',
    () async {
      for (final method in ['flag', 'load']) {
        final rules = <String, Object?>{
          'operators': ['EXCEPTION_AT_ENTRY'],
          'methods': [method],
        };
        final result = await discover(rules);
        final mutant = Mutant.fromJson(
          (result['mutants'] as List).single as Map<String, Object?>,
        );
        expect(mutant.family, MutationFamily.failureMode);
        if (method == 'load')
          expect(mutant.edit.replacement, startsWith('async '));
        final validation = await engine.handle('validate', {
          ...fixture.payload,
          'rules': rules,
          'mutant': mutant.toJson(),
        });
        expect(validation['valid'], true);
        expect(validation['compileChecked'], false);
      }
    },
  );
  test(
    'global selector and component operator compose with component metadata',
    () async {
      final rules = <String, Object?>{
        'methods': ['flag'],
        'components': [
          {
            'name': 'fault',
            'operators': ['EXCEPTION_AT_ENTRY'],
          },
        ],
      };
      final result = await discover(rules);
      final mutant = Mutant.fromJson(
        (result['mutants'] as List).single as Map<String, Object?>,
      );
      expect(mutant.components, ['fault']);
      expect(
        (await engine.handle('validate', {
          ...fixture.payload,
          'rules': rules,
          'mutant': mutant.toJson(),
        }))['valid'],
        true,
      );
    },
  );
  test('global operator and component selector inherit; empty operator set disables', () async {
    final rules = <String, Object?>{
      'operators': ['EXCEPTION_AT_ENTRY'],
      'components': [
        {
          'name': 'fault',
          'methods': ['flag'],
        },
      ],
    };
    expect((await discover(rules))['mutants'], hasLength(1));
    final disabled = <String, Object?>{
      'operators': ['EXCEPTION_AT_ENTRY'],
      'components': [
        {
          'name': 'fault',
          'methods': ['flag'],
          'operators': <String>[],
        },
      ],
    };
    expect((await discover(disabled))['mutants'], isEmpty);
  });
  test(
    'failure edit cannot validate without an explicit selected scope',
    () async {
      final result = await discover({
        'operators': ['EXCEPTION_AT_ENTRY'],
        'methods': ['flag'],
      });
      final mutant = (result['mutants'] as List).single;
      await expectLater(
        engine.handle('validate', {
          ...fixture.payload,
          'rules': {
            'operators': ['EXCEPTION_AT_ENTRY'],
          },
          'mutant': mutant,
        }),
        throwsA(isA<AdapterException>()),
      );
    },
  );
  test('MethodChannel failure mode requires explicitly supplied trusted Flutter SDK', () async {
    await expectLater(
      discover({
        'operators': ['METHOD_CHANNEL_ERROR'],
        'methods': ['flag'],
      }),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'flutter_sdk_required',
        ),
      ),
    );
  });
}

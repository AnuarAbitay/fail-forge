import 'dart:convert';

import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  test('deduplication rejects excessive full-file work before hashing candidates', () async {
    final source =
        '//${'x' * (1024 * 1024)}\n${List.generate(300, (i) => 'bool f$i()=>true;').join('\n')}';
    final fixture = ProjectFixture.create({'lib/main.dart': source});
    addTearDown(fixture.close);
    final engine = AdapterEngine();
    final limits = (await engine.handle('handshake', {}))['limits'] as Map;
    expect(limits['maxDedupWorkBytes'], 256 * 1024 * 1024);
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
          'candidate_work_limit_exceeded',
        ),
      ),
    );
  });
  for (final operators in <List<String>>[
    [],
    ['BOOLEAN_LITERAL_FLIP'],
  ]) {
    test(
      'disabled catalogue budgets cannot fail requested operators ${operators.join(',')}',
      () async {
        final fixture = ProjectFixture.create({
          'lib/main.dart':
              'bool selected()=>true;void data(){final values=<int>[${List.filled(9000, '10').join(',')}];print(values);}',
        });
        addTearDown(fixture.close);
        final result = await AdapterEngine().handle('discover', {
          ...fixture.payload,
          'rules': {'operators': operators},
        });
        expect(result['rawCandidates'], operators.isEmpty ? 0 : 1);
        expect(result['totalCandidates'], operators.isEmpty ? 0 : 1);
        expect(
          (result['mutants'] as List).every(
            (item) => (item as Map)['operatorId'] == 'BOOLEAN_LITERAL_FLIP',
          ),
          isTrue,
        );
      },
    );
  }

  test('cross-range dedup preserves authentic representative ID and component scope', () async {
    final fixture = ProjectFixture.create({
      'lib/main.dart': 'void action(){} void f(bool c){if(c){action();}}',
    });
    addTearDown(fixture.close);
    final payload = {
      ...fixture.payload,
      'rules': {
        'components': [
          {
            'name': 'branch',
            'methods': ['f'],
            'operators': ['BRANCH_BODY_REMOVAL'],
          },
          {
            'name': 'statement',
            'methods': ['f'],
            'operators': ['STATEMENT_DELETION'],
          },
        ],
      },
    };
    final engine = AdapterEngine();
    final discovery = await engine.handle('discover', payload);
    expect(discovery['rawCandidates'], 2);
    expect(discovery['totalCandidates'], 1);
    final mutant = Mutant.fromJson(
      (discovery['mutants'] as List).single as Map<String, Object?>,
    );
    expect(mutant.components, ['branch', 'statement']);
    expect(
      (await engine.handle('validate', {
        ...payload,
        'mutant': mutant.toJson(),
      }))['valid'],
      isTrue,
    );
    final reordered = await engine.handle('discover', {
      ...payload,
      'rules': {
        'components': [
          {
            'name': 'statement',
            'methods': ['f'],
            'operators': ['STATEMENT_DELETION'],
          },
          {
            'name': 'branch',
            'methods': ['f'],
            'operators': ['BRANCH_BODY_REMOVAL'],
          },
        ],
      },
    });
    expect(reordered['mutants'], discovery['mutants']);
  });

  test('measure actual discovery work for bounded candidate counts and source sizes', () async {
    final rows = <Map<String, Object?>>[];
    for (final count in [100, 200]) {
      for (final padding in [0, 1024 * 1024]) {
        final source =
            '//${'x' * padding}\n${List.generate(count, (i) => 'bool f$i()=>true;').join('\n')}';
        final fixture = ProjectFixture.create({'lib/main.dart': source});
        try {
          final micros = <int>[];
          final hashedBytes = <int>[];
          for (var repeat = 0; repeat < 2; repeat++) {
            final result = await AdapterEngine().handle('discover', {
              ...fixture.payload,
              'rules': {
                'operators': ['BOOLEAN_LITERAL_FLIP'],
              },
              'limits': {'maxMutants': 0},
            });
            expect(result['rawCandidates'], count);
            expect(result['totalCandidates'], count);
            expect(result['mutants'], isEmpty);
            expect(
              (result['metrics'] as Map)['dedupHashedBytes'],
              lessThanOrEqualTo(256 * 1024 * 1024),
            );
            expect(
              (result['metrics'] as Map)['dedupHashedBytes'],
              greaterThan(0),
            );
            final bytes = (result['metrics'] as Map)['dedupHashedBytes'] as int;
            // Every true -> false edit adds exactly one UTF-8 byte.
            expect(bytes, count * (utf8.encode(source).length + 1));
            hashedBytes.add(bytes);
            micros.add((result['metrics'] as Map)['discoveryMicros'] as int);
          }
          rows.add({
            'candidates': count,
            'sourceBytes': utf8.encode(source).length,
            'fullFileHashBytesBeforeFix': utf8.encode(source).length * count,
            'dedupHashedBytes': hashedBytes,
            'discoveryMicros': micros,
          });
        } finally {
          fixture.close();
        }
      }
    }
    // Measurements are evidence, not machine-dependent pass/fail thresholds.
    print('CLASSICAL_DEDUP_MEASUREMENT ${jsonEncode(rows)}');
  }, timeout: const Timeout(Duration(seconds: 60)));
}

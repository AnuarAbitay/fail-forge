import 'dart:io';

import 'package:failforge_dart_adapter/src/operators/language_specific/dart/common.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/null_safety/null_safety.dart';
import 'package:test/test.dart';

import '../support/call_operator_fixture.dart';

const _equivalent = 'equivalent_null_coalescing_comparison';

Future<(CallOperatorFixture, OperatorScan)> _scan(
  String source, {
  Map<String, String> files = const {},
}) async {
  final fixture = CallOperatorFixture.open(source);
  addTearDown(fixture.close);
  files.forEach(fixture.package.write);
  final unit = await fixture.resolve();
  CallOperatorFixture.noErrors(unit);
  return (
    fixture,
    const NullCoalescingFallbackRemoval().discover(unit, source),
  );
}

void main() {
  group('NSM-001 proven boolean comparison equivalence', () {
    test('filters the clock Zone lookup without changing source', () async {
      const source = '''
import 'dart:async';
final key = Object();
bool subject() => ((Zone.current[key] /* read */ ?? /* fallback */ false)) == true;
''';
      final (fixture, result) = await _scan(source);
      expect(result.candidates, isEmpty);
      expect(result.skips.map((skip) => skip.code), [_equivalent]);
      expect(await File(fixture.path).readAsString(), source);
    });

    for (final type in ['dynamic', 'bool?']) {
      for (final comparison in ['==', '!=']) {
        for (final fallback in [false, true]) {
          test('$type opposite $fallback $comparison ${!fallback}', () async {
            final (_, result) = await _scan(
              'bool subject($type value) => '
              '((value ?? ($fallback))) $comparison (${!fallback});\n',
            );
            expect(result.candidates, isEmpty);
            expect(result.skips.map((skip) => skip.code), [_equivalent]);
          });
        }
      }
    }

    test('accepts resolved const aliases and static fields', () async {
      final (_, result) = await _scan('''
const disabled = false;
class Flags { static const enabled = true; }
bool subject(dynamic value) => (value ?? disabled) == Flags.enabled;
''');
      expect(result.candidates, isEmpty);
      expect(result.skips.map((skip) => skip.code), [_equivalent]);
    });

    test('accepts an ordinary imported const with a prefix', () async {
      final (_, result) = await _scan(
        '''
import 'flags.dart' as flags;
bool subject(dynamic value) => (value ?? flags.disabled) != flags.Flags.enabled;
''',
        files: {
          'lib/flags.dart': '''
const disabled = false;
class Flags { static const enabled = true; }
''',
        },
      );
      expect(result.candidates, isEmpty);
      expect(result.skips.map((skip) => skip.code), [_equivalent]);
    });

    test(
      'supports local const aliases and retains adjacent real mutations',
      () async {
        const source = '''
bool subject(dynamic value) {
  const disabled = false;
  const enabled = true;
  return (value ?? disabled) == enabled;
}
dynamic other(dynamic value) => value /* keep */ ?? 'replacement';
''';
        final (fixture, result) = await _scan(source);
        expect(result.skips.map((skip) => skip.code), [_equivalent]);
        expect(result.candidates, hasLength(1));
        expect(
          fixture.before(result.candidates.single),
          " /* keep */ ?? 'replacement'",
        );
        await fixture.validateAll(result.candidates);
      },
    );
  });

  group('NSM-001 retains unproven or observable comparisons', () {
    final cases = <String, String>{
      'same false value':
          'bool subject(dynamic value) => (value ?? false) == false;',
      'same true value with inequality':
          'bool subject(dynamic value) => (value ?? true) != true;',
      'reversed comparison operands':
          'bool subject(dynamic value) => true == (value ?? false);',
      'nonboolean comparison':
          'bool subject(dynamic value) => (value ?? false) == 0;',
      'fallback call': 'bool fallback() => false; bool subject(dynamic value) => (value ?? fallback()) == true;',
      'fallback getter': 'bool get disabled => false; bool subject(dynamic value) => (value ?? disabled) == true;',
      'nonconst fallback': 'final disabled = false; bool subject(dynamic value) => (value ?? disabled) == true;',
      'comparison call': 'bool enabled() => true; bool subject(dynamic value) => (value ?? false) == enabled();',
      'comparison getter': 'bool get enabled => true; bool subject(dynamic value) => (value ?? false) == enabled;',
      'nonconst comparison': 'final enabled = true; bool subject(dynamic value) => (value ?? false) == enabled;',
      'custom static receiver': '''
class Flag {
  @override bool operator ==(Object other) => true;
  @override int get hashCode => 0;
}
bool subject(Flag? value) => (value ?? false) == true;
''',
      'extension type receiver': 'extension type Flag(bool? value) {} bool subject(Flag? value) => (value ?? false) == true;',
      'generic receiver':
          'bool subject<T>(T? value) => (value ?? false) == true;',
    };
    for (final entry in cases.entries) {
      test(entry.key, () async {
        final (fixture, result) = await _scan(entry.value);
        expect(result.candidates, hasLength(1));
        expect(result.skips.where((skip) => skip.code == _equivalent), isEmpty);
        await fixture.validateAll(result.candidates);
      });
    }

    for (final deferredPosition in ['fallback', 'comparison']) {
      test('does not discard access to deferred $deferredPosition', () async {
        final (_, result) = await _scan(
          '''
import 'flags.dart' deferred as flags;
bool subject(dynamic value) => (value ?? ${deferredPosition == 'fallback' ? 'flags.disabled' : 'false'}) == ${deferredPosition == 'comparison' ? 'flags.enabled' : 'true'};
''',
          files: {
            'lib/flags.dart': 'const disabled = false; const enabled = true;',
          },
        );
        expect(result.candidates, hasLength(1));
        expect(result.skips.where((skip) => skip.code == _equivalent), isEmpty);
      });
    }

    test('retains existing nonnullable redundancy diagnostic', () async {
      final (_, result) = await _scan(
        'bool subject(bool value) => (value ?? false) == true;',
      );
      expect(result.candidates, isEmpty);
      expect(result.skips.map((skip) => skip.code), [
        'nonnullable_coalescing_receiver',
      ]);
    });
  });

  test('dynamic equality dispatch, exceptions and receiver effects agree', () {
    for (final fallback in [false, true]) {
      for (final inequality in [false, true]) {
        for (final kind in [
          'null',
          'false',
          'true',
          'number',
          'equals',
          'throws',
        ]) {
          final originalLog = <Object>[];
          final mutantLog = <Object>[];
          final error = StateError('same equality error');
          dynamic read(List<Object> log) {
            log.add('read');
            return switch (kind) {
              'null' => null,
              'false' => false,
              'true' => true,
              'number' => 42,
              'equals' => _ObservedEquality(log),
              _ => _ObservedEquality(log, error),
            };
          }

          Object observe(bool Function() evaluate) {
            try {
              return evaluate();
            } catch (caught) {
              return caught;
            }
          }

          final target = !fallback;
          final original = observe(
            () => inequality
                ? (read(originalLog) ?? fallback) != target
                : (read(originalLog) ?? fallback) == target,
          );
          final mutant = observe(
            () => inequality
                ? read(mutantLog) != target
                : read(mutantLog) == target,
          );
          expect(mutant, same(original), reason: '$kind/$fallback/$inequality');
          expect(mutantLog, originalLog);
          expect(mutantLog.where((event) => event == 'read'), hasLength(1));
        }
      }
    }
  });
}

class _ObservedEquality {
  _ObservedEquality(this.log, [this.error]);
  final List<Object> log;
  final Object? error;

  @override
  bool operator ==(Object other) {
    log.add('== $other');
    if (error case final error?) throw error;
    return other == true;
  }

  @override
  int get hashCode => 0;
}

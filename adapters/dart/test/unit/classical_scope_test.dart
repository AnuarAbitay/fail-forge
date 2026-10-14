import 'package:failforge_dart_adapter/src/scope/scope_rules.dart';
import 'package:test/test.dart';

void main() {
  test('potential operator set preserves every allowed candidate across global/component/file/symbol intersections', () {
    final specifications = <Map<String, Object?>>[
      {},
      {'operators': <String>[]},
      {
        'operators': ['BOOLEAN_LITERAL_FLIP', 'NUMERIC_LITERAL_REPLACEMENT'],
      },
      {'classes': <String>[]},
      {'methods': <String>[]},
      {'includePaths': <String>[]},
      {
        'excludePaths': ['lib/a.dart'],
      },
      {
        'components': [
          {
            'name': 'a',
            'includePaths': ['lib/a.dart'],
            'operators': ['BOOLEAN_LITERAL_FLIP'],
          },
          {
            'name': 'b',
            'includePaths': ['lib/b.dart'],
            'classes': ['C'],
            'operators': ['NUMERIC_LITERAL_REPLACEMENT', 'EXCEPTION_AT_ENTRY'],
          },
        ],
      },
      {
        'operators': ['BOOLEAN_LITERAL_FLIP'],
        'components': [
          {
            'name': 'blocked',
            'operators': ['NUMERIC_LITERAL_REPLACEMENT'],
          },
          {
            'name': 'inherited',
            'methods': ['f'],
          },
        ],
      },
      {
        'components': [
          {
            'name': 'empty-paths',
            'includePaths': <String>[],
            'operators': ['EXCEPTION_AT_ENTRY'],
          },
          {
            'name': 'empty-symbols',
            'classes': <String>[],
            'operators': ['NUMERIC_LITERAL_REPLACEMENT'],
          },
          {'name': 'disabled', 'operators': <String>[]},
        ],
      },
    ];
    for (final specification in specifications) {
      final rules = ScopeRules.fromJson(specification);
      final unfiltered = rules.operatorIdsForFile();
      expect(() => unfiltered.add('EXAMPLE'), throwsUnsupportedError);
      for (final file in ['lib/a.dart', 'lib/b.dart', 'test/a.dart']) {
        final potential = rules.operatorIdsForFile(file);
        expect(unfiltered, containsAll(potential));
        expect(() => potential.clear(), throwsUnsupportedError);
        final actuallyAllowed = <String>{};
        for (final id in supportedOperatorIds) {
          for (final className in <String?>[null, 'C', 'Other']) {
            for (final methodName in <String?>[null, 'f', 'other']) {
              if (rules
                  .match(
                    file: file,
                    operatorId: id,
                    className: className,
                    methodName: methodName,
                  )
                  .allowed) {
                actuallyAllowed.add(id);
              }
            }
          }
        }
        expect(potential, actuallyAllowed, reason: '$specification at $file');
      }
    }
  });

  test('a caller cannot mutate explicit rule state through potential operator metadata', () {
    final rules = ScopeRules.fromJson({
      'operators': ['BOOLEAN_LITERAL_FLIP'],
    });
    expect(() => rules.operatorIdsForFile().clear(), throwsUnsupportedError);
    expect(
      rules
          .match(file: 'lib/a.dart', operatorId: 'BOOLEAN_LITERAL_FLIP')
          .allowed,
      true,
    );
    expect(
      rules
          .match(file: 'lib/a.dart', operatorId: 'NUMERIC_LITERAL_REPLACEMENT')
          .allowed,
      false,
    );
  });
}

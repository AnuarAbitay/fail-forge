import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';
import 'package:failforge_dart_adapter/src/scope/scope_rules.dart';
import 'package:test/test.dart';

void main() {
  test('P1 cross-filter combines a global method selector with a component operator', () {
    final rules = ScopeRules.fromJson({
      'methods': ['flag'],
      'components': [
        {
          'name': 'fault',
          'operators': ['EXCEPTION_AT_ENTRY'],
        },
      ],
    });
    expect(
      rules
          .match(
            file: 'lib/a.dart',
            methodName: 'flag',
            operatorId: 'EXCEPTION_AT_ENTRY',
          )
          .allowed,
      isTrue,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        methodName: 'flag',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isTrue,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        methodName: 'other',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isFalse,
    );
  });

  test('P1 cross-filter inherits explicit global operators into component selectors', () {
    final rules = ScopeRules.fromJson({
      'operators': ['EXCEPTION_AT_ENTRY'],
      'components': [
        {
          'name': 'selected',
          'classes': ['C'],
          'methods': ['flag'],
        },
      ],
    });
    expect(
      rules
          .match(
            file: 'lib/a.dart',
            className: 'C',
            methodName: 'flag',
            operatorId: 'EXCEPTION_AT_ENTRY',
          )
          .allowed,
      isTrue,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        className: 'C',
        methodName: 'flag',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isTrue,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        methodName: 'flag',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isFalse,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        className: 'C',
        methodName: 'other',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isFalse,
    );
  });

  test('P1 cross-filter preserves explicit empty global and component operator sets', () {
    for (final value in <Map<String, Object?>>[
      {
        'operators': [],
        'components': [
          {
            'name': 'fault',
            'methods': ['flag'],
            'operators': ['EXCEPTION_AT_ENTRY'],
          },
        ],
      },
      {
        'operators': ['EXCEPTION_AT_ENTRY'],
        'methods': ['flag'],
        'components': [
          {'name': 'disabled', 'operators': []},
        ],
      },
    ]) {
      final rules = ScopeRules.fromJson(value);
      expect(
        rules
            .match(
              file: 'lib/a.dart',
              methodName: 'flag',
              operatorId: 'EXCEPTION_AT_ENTRY',
            )
            .allowed,
        isFalse,
      );
      expect(
        rules.failureModeExplicitlySelected(
          file: 'lib/a.dart',
          methodName: 'flag',
          operatorId: 'EXCEPTION_AT_ENTRY',
        ),
        isFalse,
      );
    }
  });

  test('P1 cross-filter ignores selectors supplied only by an operator-disabled component', () {
    final rules = ScopeRules.fromJson({
      'components': [
        {
          'name': 'broad',
          'operators': ['EXCEPTION_AT_ENTRY'],
        },
        {
          'name': 'disabled',
          'methods': ['flag'],
          'operators': [],
        },
      ],
    });
    final matched = rules.match(
      file: 'lib/a.dart',
      methodName: 'flag',
      operatorId: 'EXCEPTION_AT_ENTRY',
    );
    expect(matched.allowed, isTrue);
    expect(matched.components, ['broad', 'disabled']);
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        methodName: 'flag',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isFalse,
    );
  });

  test('P1 cross-filter ignores nonmatching component selectors and keeps default P1 disabled', () {
    final rules = ScopeRules.fromJson({
      'operators': ['EXCEPTION_AT_ENTRY'],
      'components': [
        {'name': 'broad'},
        {
          'name': 'elsewhere',
          'includePaths': ['lib/other.dart'],
          'methods': ['flag'],
        },
      ],
    });
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        methodName: 'flag',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isFalse,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/other.dart',
        methodName: 'flag',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isTrue,
    );
    final defaults = ScopeRules.fromJson({
      'components': [
        {
          'name': 'selected',
          'methods': ['flag'],
        },
      ],
    });
    expect(
      defaults
          .match(
            file: 'lib/a.dart',
            methodName: 'flag',
            operatorId: 'EXCEPTION_AT_ENTRY',
          )
          .allowed,
      isFalse,
    );
  });
  test('build test and integration_test exclusions are anchored to the package root', () {
    final rules = ScopeRules.fromJson({});
    for (final path in [
      'lib/build/model.dart',
      'lib/test/support.dart',
      'lib/integration_test/bridge.dart',
    ]) {
      expect(rules.includesFile(path), isTrue, reason: path);
    }
    for (final path in [
      'build/model.dart',
      'test/support.dart',
      'integration_test/bridge.dart',
    ]) {
      expect(rules.includesFile(path), isFalse, reason: path);
    }
  });
  test('component names use the shared 512 UTF-8 byte bound', () {
    final boundary = List.filled(256, 'я').join();
    expect(
      ScopeRules.fromJson({
        'components': [
          {'name': boundary},
        ],
      }),
      isA<ScopeRules>(),
    );
    expect(
      () => ScopeRules.fromJson({
        'components': [
          {'name': '${boundary}я'},
        ],
      }),
      throwsA(isA<AdapterException>()),
    );
  });
  ScopeMatch match(
    ScopeRules rules, {
    String file = 'lib/a.dart',
    String? className,
    String? methodName,
    String operator = 'BOOLEAN_LITERAL_FLIP',
  }) => rules.match(
    file: file,
    operatorId: operator,
    className: className,
    methodName: methodName,
  );

  test('default allows classic operators and absent filters do not constrain symbols', () {
    final rules = ScopeRules.fromJson({});
    for (final operator in classicOperators) {
      expect(match(rules, operator: operator).allowed, isTrue);
    }
    expect(match(rules, operator: 'EXCEPTION_AT_ENTRY').allowed, isFalse);
    expect(match(ScopeRules.fromJson({'operators': []})).allowed, isFalse);
    expect(match(ScopeRules.fromJson({'components': []})).allowed, isTrue);
  });

  test('categories are AND, list values OR, exclusions take priority', () {
    final rules = ScopeRules.fromJson({
      'includePaths': ['lib/a.dart', 'lib/sub/**'],
      'excludePaths': ['lib/sub/blocked.dart'],
      'classes': ['A', 'B'],
      'methods': ['run', 'stop'],
    });
    expect(match(rules, className: 'A', methodName: 'run').allowed, isTrue);
    expect(
      match(
        rules,
        file: 'lib/sub/b.dart',
        className: 'B',
        methodName: 'stop',
      ).allowed,
      isTrue,
    );
    expect(
      match(
        rules,
        file: 'lib/sub/blocked.dart',
        className: 'A',
        methodName: 'run',
      ).allowed,
      isFalse,
    );
    expect(
      match(
        rules,
        file: 'lib/c.dart',
        className: 'A',
        methodName: 'run',
      ).allowed,
      isFalse,
    );
    expect(match(rules, className: 'C', methodName: 'run').allowed, isFalse);
    expect(match(rules, className: 'A', methodName: 'other').allowed, isFalse);
    expect(
      match(rules, methodName: 'run').allowed,
      isFalse,
      reason: 'top-level has no class',
    );
  });

  test('explicit empty categories match nothing', () {
    for (final category in [
      'includePaths',
      'classes',
      'methods',
      'operators',
    ]) {
      expect(
        match(
          ScopeRules.fromJson({category: []}),
          className: 'A',
          methodName: 'run',
        ).allowed,
        isFalse,
      );
    }
  });

  test('overlapping component operators union while all matched names are retained', () {
    final rules = ScopeRules.fromJson({
      'components': [
        {
          'name': 'z',
          'classes': ['A'],
          'operators': ['RELATIONAL_BOUNDARY'],
        },
        {
          'name': 'a',
          'methods': ['run'],
          'operators': ['BOOLEAN_LITERAL_FLIP'],
        },
        {
          'name': 'disabled',
          'classes': ['A'],
          'operators': [],
        },
      ],
    });
    final selected = match(rules, className: 'A', methodName: 'run');
    expect(selected.allowed, isTrue);
    expect(selected.components, ['a', 'disabled', 'z']);
    expect(
      match(
        rules,
        className: 'A',
        methodName: 'run',
        operator: 'RELATIONAL_BOUNDARY',
      ).allowed,
      isTrue,
    );
    expect(
      match(
        rules,
        className: 'A',
        methodName: 'run',
        operator: 'LOGICAL_CONNECTOR_SWAP',
      ).allowed,
      isFalse,
    );
    expect(
      match(rules, className: 'Other', methodName: 'other').allowed,
      isFalse,
    );
    final global = ScopeRules.fromJson({
      'operators': [],
      'components': [
        {
          'name': 'all',
          'operators': ['BOOLEAN_LITERAL_FLIP'],
        },
      ],
    });
    expect(match(global).allowed, isFalse);
  });

  test('component-selected failure mode requires explicit operator and positive scope', () {
    final rules = ScopeRules.fromJson({
      'components': [
        {
          'name': 'failure',
          'methods': ['run'],
          'operators': ['EXCEPTION_AT_ENTRY'],
        },
      ],
    });
    expect(
      match(rules, methodName: 'run', operator: 'EXCEPTION_AT_ENTRY').allowed,
      isTrue,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        methodName: 'run',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isTrue,
    );
    expect(
      rules.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        methodName: 'other',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isFalse,
    );
    final broad = ScopeRules.fromJson({
      'operators': ['EXCEPTION_AT_ENTRY'],
    });
    expect(
      broad.failureModeExplicitlySelected(
        file: 'lib/a.dart',
        operatorId: 'EXCEPTION_AT_ENTRY',
      ),
      isFalse,
    );
  });

  test('generated patterns and headers extend the defaults', () {
    final rules = ScopeRules.fromJson({
      'generatedPatterns': ['lib/gen/**'],
      'generatedHeaders': ['CUSTOM GENERATED'],
    });
    expect(rules.isGenerated('lib/a.g.dart', ''), isTrue);
    expect(rules.isGenerated('lib/a.freezed.dart', ''), isTrue);
    expect(rules.isGenerated('lib/gen/a.dart', ''), isTrue);
    expect(
      rules.isGenerated('lib/a.dart', '// CUSTOM GENERATED\nbool x = true;'),
      isTrue,
    );
    expect(
      rules.isGenerated(
        'lib/a.dart',
        '// GENERATED CODE - DO NOT MODIFY BY HAND',
      ),
      isTrue,
    );
    expect(rules.isGenerated('lib/a.dart', 'bool x = true;'), isFalse);
    expect(
      rules.isGenerated(
        'lib/a.dart',
        '\ufeff/* license /* nested */ CUSTOM GENERATED */\r\nbool x = true;',
      ),
      isTrue,
    );
    expect(
      rules.isGenerated(
        'lib/a.dart',
        "// license\nconst note = 'CUSTOM GENERATED';\nbool x = true;",
      ),
      isFalse,
    );
    expect(
      rules.isGenerated('lib/a.dart', "bool x = true; // CUSTOM GENERATED"),
      isFalse,
    );
    expect(
      rules.isGenerated(
        'lib/a.dart',
        "const note = r'''\n// CUSTOM GENERATED\n''';",
      ),
      isFalse,
    );
    for (final file in [
      'test/a.dart',
      'integration_test/a.dart',
      'lib/.dart_tool/a.dart',
      'build/a.dart',
      'lib/.git/a.dart',
    ]) {
      expect(rules.includesFile(file), isFalse);
    }
  });

  test(
    'unknown fields, operators, duplicate components and unsafe globs reject',
    () {
      for (final value in <Map<String, Object?>>[
        {'typo': []},
        {
          'operators': ['NOT_SUPPORTED'],
        },
        {'classes': null},
        {
          'includePaths': ['/outside/**'],
        },
        {
          'includePaths': ['lib/../**'],
        },
        {
          'components': [
            {'name': 'a'},
            {'name': 'a'},
          ],
        },
        {
          'components': [
            {'name': 'a', 'command': 'execute'},
          ],
        },
      ]) {
        expect(
          () => ScopeRules.fromJson(value),
          throwsA(isA<AdapterException>()),
        );
      }
    },
  );
}

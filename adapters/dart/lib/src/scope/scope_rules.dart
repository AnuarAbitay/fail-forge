import 'dart:convert';

import 'package:glob/glob.dart';
import 'package:path/path.dart' as p;

import '../protocol/adapter_error.dart';
import '../operators/registry.dart' as registry;

final Set<String> classicOperators = Set.unmodifiable(
  registry.defaultClassicOperators.map((operator) => operator.id),
);
final Set<String> supportedOperatorIds = Set.unmodifiable(
  registry.operatorsByFamily.values
      .expand((group) => group)
      .map((operator) => operator.id),
);

class ScopeMatch {
  const ScopeMatch(this.allowed, this.components);
  final bool allowed;
  final List<String> components;
}

class ScopeRules {
  ScopeRules._(
    this._global,
    this._components,
    this.generatedPatterns,
    this.generatedHeaders,
  );

  factory ScopeRules.fromJson(Map<String, Object?> value) {
    onlyKeys(value, {
      'includePaths',
      'excludePaths',
      'generatedPatterns',
      'generatedHeaders',
      'classes',
      'methods',
      'operators',
      'components',
    }, 'rules');
    final global = _Filter.fromJson(value);
    final components = <_Component>[];
    final rawComponents = value['components'];
    if (value.containsKey('components')) {
      if (rawComponents is! List<Object?> || rawComponents.length > 10000) {
        throw AdapterException(
          'invalid_payload',
          'components must be a bounded array',
        );
      }
      final names = <String>{};
      for (final item in rawComponents) {
        final component = jsonObject(item, 'component');
        onlyKeys(component, {
          'name',
          'includePaths',
          'excludePaths',
          'classes',
          'methods',
          'operators',
        }, 'component');
        final name = jsonString(component['name'], 'component name');
        if (utf8.encode(name).length > 512 || !names.add(name)) {
          throw AdapterException(
            'invalid_payload',
            'component names must be unique and bounded',
          );
        }
        components.add(_Component(name, _Filter.fromJson(component)));
      }
    }
    final generated = [
      '**/*.g.dart',
      '**/*.freezed.dart',
      if (value.containsKey('generatedPatterns'))
        ...jsonStrings(value['generatedPatterns'], 'generatedPatterns'),
    ].map(_glob).toList(growable: false);
    final headers = [
      'GENERATED CODE - DO NOT MODIFY BY HAND',
      if (value.containsKey('generatedHeaders'))
        ...jsonStrings(value['generatedHeaders'], 'generatedHeaders'),
    ];
    return ScopeRules._(
      global,
      components,
      generated,
      List.unmodifiable(headers),
    );
  }

  final _Filter _global;
  final List<_Component> _components;
  final List<Glob> generatedPatterns;
  final List<String> generatedHeaders;

  bool isExcluded(String file) {
    final segments = file.split('/');
    if ({'test', 'integration_test', 'build'}.contains(segments.first))
      return true;
    if (segments.any({'.dart_tool', '.git', '.pub-cache'}.contains)) {
      return true;
    }
    return _global.excluded(file);
  }

  bool includesFile(String file) => !isExcluded(file) && _global.includes(file);

  /// Potentially enabled operators before expensive AST scans. Symbol matching
  /// still happens per candidate; disabled/file-excluded operators consume no
  /// candidate or diagnostic budget.
  Set<String> operatorIdsForFile([String? file]) {
    if (!_global.hasPossibleSelection || (file != null && !includesFile(file)))
      return const {};
    final global = _global.explicitOperators ?? classicOperators;
    if (_components.isEmpty) return Set.unmodifiable(global);
    final enabled = <String>{};
    for (final component in _components) {
      final filter = component.filter;
      if (!filter.hasPossibleSelection ||
          (file != null && (filter.excluded(file) || !filter.includes(file))))
        continue;
      enabled.addAll(filter.explicitOperators ?? global);
    }
    if (_global.explicitOperators != null) enabled.retainAll(global);
    return Set.unmodifiable(enabled);
  }

  bool isGenerated(String file, String text) {
    if (generatedPatterns.any((pattern) => pattern.matches(file))) return true;
    final comments = _leadingComments(text);
    return generatedHeaders.any(comments.contains);
  }

  ScopeMatch match({
    required String file,
    required String operatorId,
    String? className,
    String? methodName,
  }) {
    if (!includesFile(file) ||
        !_global.symbolsMatch(className, methodName) ||
        (_global.explicitOperators != null &&
            !_global.explicitOperators!.contains(operatorId))) {
      return const ScopeMatch(false, []);
    }
    final matched = _components
        .where(
          (component) => component.filter.matches(file, className, methodName),
        )
        .toList();
    if (_components.isEmpty && !_global.operatorAllows(operatorId)) {
      return const ScopeMatch(false, []);
    }
    if (_components.isNotEmpty &&
        !matched.any((component) => _componentAllows(component, operatorId))) {
      return const ScopeMatch(false, []);
    }
    final names = matched.map((component) => component.name).toSet().toList()
      ..sort();
    return ScopeMatch(true, List.unmodifiable(names));
  }

  bool _componentAllows(_Component component, String operatorId) =>
      component.filter.explicitOperators?.contains(operatorId) ??
      _global.operatorAllows(operatorId);

  /// P1 additionally requires an explicit scope selection and explicit operator.
  /// The engine applies this gate before asking a failure-mode operator to run.
  bool failureModeExplicitlySelected({
    required String file,
    required String operatorId,
    String? className,
    String? methodName,
  }) {
    if (!match(
      file: file,
      operatorId: operatorId,
      className: className,
      methodName: methodName,
    ).allowed)
      return false;
    final enabledComponents = _components.where(
      (component) =>
          component.filter.matches(file, className, methodName) &&
          _componentAllows(component, operatorId),
    );
    final explicitOperator =
        _global.explicitOperators?.contains(operatorId) == true ||
        enabledComponents.any(
          (component) =>
              component.filter.explicitOperators?.contains(operatorId) == true,
        );
    final positiveScope =
        _global.hasSelection ||
        enabledComponents.any((component) => component.filter.hasSelection);
    return explicitOperator && positiveScope;
  }
}

String _leadingComments(String text) {
  final end = text.length < 4096 ? text.length : 4096;
  final comments = StringBuffer();
  var index = 0;
  while (index < end) {
    final unit = text.codeUnitAt(index);
    if (unit == 0xfeff || text[index].trim().isEmpty) {
      index++;
    } else if (text.startsWith('//', index) ||
        (index == 0 && text.startsWith('#!'))) {
      final start = index;
      while (index < end && text[index] != '\r' && text[index] != '\n') {
        index++;
      }
      if (text.startsWith('//', start))
        comments.writeln(text.substring(start, index));
    } else if (text.startsWith('/*', index)) {
      final start = index;
      var depth = 1;
      index += 2;
      while (index < end && depth != 0) {
        if (text.startsWith('/*', index)) {
          depth++;
          index += 2;
        } else if (text.startsWith('*/', index)) {
          depth--;
          index += 2;
        } else {
          index++;
        }
      }
      comments.writeln(text.substring(start, index < end ? index : end));
    } else {
      break; // Markers inside strings or later code are not file headers.
    }
  }
  return comments.toString();
}

Glob _glob(String pattern) {
  if (pattern.startsWith('/') ||
      pattern.contains('\\') ||
      pattern.contains('\u0000') ||
      pattern.split('/').contains('..')) {
    throw AdapterException(
      'invalid_payload',
      'path patterns must stay package-relative',
    );
  }
  try {
    return Glob(pattern, context: p.Context(style: p.Style.posix));
  } on FormatException {
    throw AdapterException('invalid_payload', 'path pattern is malformed');
  }
}

class _Component {
  const _Component(this.name, this.filter);
  final String name;
  final _Filter filter;
}

class _Filter {
  _Filter(
    this.includePaths,
    this.excludePaths,
    this.classes,
    this.methods,
    this.explicitOperators,
  );

  factory _Filter.fromJson(Map<String, Object?> value) {
    List<String>? list(String name) =>
        value.containsKey(name) ? jsonStrings(value[name], name) : null;
    final operators = list('operators');
    if (operators != null &&
        operators.any((operator) => !supportedOperatorIds.contains(operator))) {
      throw AdapterException(
        'unknown_operator',
        'rules contain an unsupported operator ID',
      );
    }
    return _Filter(
      list('includePaths')?.map(_glob).toList(growable: false),
      (list('excludePaths') ?? []).map(_glob).toList(growable: false),
      list('classes'),
      list('methods'),
      operators?.toSet(),
    );
  }

  final List<Glob>? includePaths;
  final List<Glob> excludePaths;
  final List<String>? classes;
  final List<String>? methods;
  final Set<String>? explicitOperators;

  bool get hasSelection =>
      includePaths != null || classes != null || methods != null;
  bool get hasPossibleSelection =>
      includePaths?.isEmpty != true &&
      classes?.isEmpty != true &&
      methods?.isEmpty != true;
  bool excluded(String file) =>
      excludePaths.any((pattern) => pattern.matches(file));
  bool includes(String file) =>
      includePaths == null ||
      includePaths!.any((pattern) => pattern.matches(file));
  bool symbolsMatch(String? className, String? methodName) =>
      (classes == null ||
          (className != null && classes!.contains(className))) &&
      (methods == null ||
          (methodName != null && methods!.contains(methodName)));
  bool matches(String file, String? className, String? methodName) =>
      !excluded(file) && includes(file) && symbolsMatch(className, methodName);
  bool operatorAllows(String operator) =>
      (explicitOperators ?? classicOperators).contains(operator);
}

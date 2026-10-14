import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/nullability_suffix.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../failure_modes/symbols.dart';

/// Renders only types whose names resolve to the original semantic class at
/// this insertion site. In particular, prefixes and shadowing are preserved.
String? visibleType(
  DartType type,
  SdkSymbols symbols,
  AstNode anchor,
  ValueTypeBindings bindings,
) {
  if (type is DynamicType) return 'dynamic';
  if (type is TypeParameterType) {
    final name = type.element.name;
    if (name == null ||
        !_visibleTypeParameter(type, anchor) ||
        bindings.shadowed(anchor, name))
      return null;
    return type.getDisplayString();
  }
  if (type is! InterfaceType) return null;
  final name = type.element.name;
  if (name == null) return null;
  final visible = symbols.available(
    anchor,
    name,
    type.element.library.uri,
    replacingBody: false,
  );
  if (visible == null) return null;
  final arguments = <String>[];
  for (final argument in type.typeArguments) {
    final rendered = visibleType(argument, symbols, anchor, bindings);
    if (rendered == null) return null;
    arguments.add(rendered);
  }
  final generic = arguments.isEmpty ? '' : '<${arguments.join(', ')}>';
  final nullable = type.nullabilitySuffix == NullabilitySuffix.question
      ? '?'
      : '';
  return '$visible$generic$nullable';
}

bool _visibleTypeParameter(TypeParameterType type, AstNode anchor) {
  for (AstNode? current = anchor; current != null; current = current.parent) {
    final parameters = switch (current) {
      FunctionExpression() => current.typeParameters,
      MethodDeclaration() => current.typeParameters,
      ClassDeclaration() => current.namePart.typeParameters,
      MixinDeclaration() => current.typeParameters,
      ExtensionDeclaration() => current.typeParameters,
      ExtensionTypeDeclaration() => current.namePart.typeParameters,
      EnumDeclaration() => current.namePart.typeParameters,
      _ => null,
    };
    if (parameters == null) continue;
    for (final parameter in parameters.typeParameters) {
      if (parameter.name.lexeme == type.element.name) {
        return parameter.declaredFragment?.element.baseElement ==
            type.element.baseElement;
      }
    }
  }
  return false;
}

/// Per-scan lexical cache. Repeated generic returns do not rescan an entire
/// function body, and a later overlay cannot inherit stale bindings.
class ValueTypeBindings {
  final _names = <AstNode, Set<String>>{};
  bool _contains(AstNode? node, String name) {
    if (node == null) return false;
    return _names
        .putIfAbsent(node, () {
          final visitor = _ValueBindings();
          node.accept(visitor);
          return visitor.names;
        })
        .contains(name);
  }

  bool shadowed(AstNode anchor, String name) {
    for (AstNode? current = anchor; current != null; current = current.parent) {
      if (current is FunctionExpression) {
        if (_contains(current.parameters, name) ||
            _contains(current.body, name))
          return true;
      }
      if (current is MethodDeclaration) {
        if (_contains(current.parameters, name) ||
            _contains(current.body, name))
          return true;
      }
    }
    return false;
  }
}

class _ValueBindings extends GeneralizingAstVisitor<void> {
  final names = <String>{};
  @override
  void visitNode(AstNode node) {
    final binding = switch (node) {
      VariableDeclaration() => node.name.lexeme,
      FormalParameter() => node.name?.lexeme,
      DeclaredVariablePattern() => node.name.lexeme,
      CatchClauseParameter() => node.name.lexeme,
      FunctionDeclaration() => node.name.lexeme,
      _ => null,
    };
    if (binding != null) names.add(binding);
    super.visitNode(node);
  }
}

/// Literal syntax may differ while representing the same return value (hex,
/// quotes, trivia or parentheses). Do not emit those ineffective mutations.
bool isUnchangedReturnValue(Expression expression, String replacement) {
  final value = expression.unParenthesized;
  if (replacement == 'null') return value is NullLiteral;
  if (replacement == 'true' || replacement == 'false')
    return value is BooleanLiteral && value.value == (replacement == 'true');
  if (replacement == "''")
    return value is StringLiteral && value.stringValue == '';
  if (value is ListLiteral && value.elements.isEmpty ||
      value is SetOrMapLiteral && value.elements.isEmpty) {
    return replacement.endsWith('[]') || replacement.endsWith('{}');
  }
  num? original;
  if (value is IntegerLiteral)
    original = value.staticType?.isDartCoreDouble == true
        ? value.parseDoubleValue(negated: false)
        : value.value;
  if (value is DoubleLiteral) original = value.value;
  if (value is PrefixExpression && value.operator.lexeme == '-') {
    final operand = value.operand.unParenthesized;
    if (operand is IntegerLiteral && operand.value != null) {
      original = value.staticType?.isDartCoreDouble == true
          ? operand.parseDoubleValue(negated: true)
          : -operand.value!;
    }
    if (operand is DoubleLiteral) original = -operand.value;
  }
  final target = num.tryParse(replacement);
  if (original == null || target == null || original != target) return false;
  // Negative zero can affect subsequent numerical operations.
  return original != 0 || original.isNegative == target.isNegative;
}

String? typedEmptyCollection(
  InterfaceType type,
  SdkSymbols symbols,
  AstNode anchor,
  ValueTypeBindings bindings,
) {
  if (type.element.library.uri != Uri.parse('dart:core')) return null;
  final brackets = switch (type.element.name) {
    'List' => '[]',
    'Set' || 'Map' => '{}',
    _ => null,
  };
  if (brackets == null) return null;
  final arguments = <String>[];
  for (final type in type.typeArguments) {
    final rendered = visibleType(type, symbols, anchor, bindings);
    if (rendered == null) return null;
    arguments.add(rendered);
  }
  if (arguments.isEmpty) return null;
  return '<${arguments.join(', ')}>$brackets';
}

List<String> returnReplacements(
  DartType type,
  SdkSymbols symbols,
  AstNode anchor,
  ValueTypeBindings bindings,
) {
  if (type is DynamicType || type is VoidType || type is NeverType) return [];
  final values = <String>[];
  if (type.nullabilitySuffix == NullabilitySuffix.question ||
      type.isDartCoreNull)
    values.add('null');
  if (type is InterfaceType &&
      type.element.library.uri == Uri.parse('dart:core')) {
    switch (type.element.name) {
      case 'int':
      case 'num':
        values.addAll(['0', '1', '-1']);
      case 'double':
        values.addAll(['0.0', '1.0', '-1.0']);
      case 'bool':
        values.addAll(['true', 'false']);
      case 'String':
        values.add("''");
      case 'List':
      case 'Set':
      case 'Map':
        final empty = typedEmptyCollection(type, symbols, anchor, bindings);
        if (empty != null) values.add(empty);
    }
  }
  return values;
}

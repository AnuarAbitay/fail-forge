import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';

import '../../common.dart';

export '../../common.dart';

/// These categories belong to Dart, underneath the language_specific family.
enum LanguageSpecificCategory implements MutationCategory {
  nullSafety('null_safety'),
  parameters('parameters'),
  switches('switch'),
  patterns('patterns'),
  collections('collections'),
  enums('enums');

  const LanguageSpecificCategory(this.wireName);
  @override
  final String wireName;
}

abstract class LanguageSpecificOperator extends MutationOperator {
  const LanguageSpecificOperator();

  @override
  MutationFamily get family => MutationFamily.languageSpecific;
  @override
  String get language => 'dart';
  @override
  LanguageSpecificCategory get category;
}

/// A bounded public-API check, not a replacement for analyzer constant analysis.
/// Unknown expressions remain subject to whole-library overlay validation.
bool? constantBoolGuard(Expression expression, [int depth = 0]) {
  if (depth >= 64) return null;
  final node = expression.unParenthesized;
  if (node is BooleanLiteral) return node.value;
  if (node is PrefixExpression && node.operator.lexeme == '!') {
    final value = constantBoolGuard(node.operand, depth + 1);
    return value == null ? null : !value;
  }
  final Element? element = switch (node) {
    SimpleIdentifier() => node.element,
    PrefixedIdentifier() => node.element,
    PropertyAccess() => node.propertyName.element,
    _ => null,
  };
  final variable = element is PropertyAccessorElement
      ? element.variable
      : element;
  if (variable is VariableElement && variable.isConst) {
    return variable.computeConstantValue()?.toBoolValue();
  }
  if (node is BinaryExpression) {
    final left = constantBoolGuard(node.leftOperand, depth + 1);
    final right = constantBoolGuard(node.rightOperand, depth + 1);
    return switch (node.operator.lexeme) {
      '&&' when left == false || right == false => false,
      '||' when left == true || right == true => true,
      '&&' when left != null && right != null => left && right,
      '||' when left != null && right != null => left || right,
      '==' when left != null && right != null => left == right,
      '!=' when left != null && right != null => left != right,
      _ => null,
    };
  }
  return null;
}

/// Requires an observable runtime input rather than trying to evaluate every
/// form of constant comparison or arithmetic. This is a conservative selection
/// gate; a runtime input alone does not prove that a guard can take both values.
bool hasRuntimeGuardDependency(Expression expression) {
  final visitor = _RuntimeGuardDependency();
  expression.accept(visitor);
  return visitor.found;
}

class _RuntimeGuardDependency extends RecursiveAstVisitor<void> {
  bool found = false;

  @override
  void visitSimpleIdentifier(SimpleIdentifier node) {
    final element = node.element;
    final variable = element is PropertyAccessorElement
        ? element.variable
        : element;
    if (variable is VariableElement && !variable.isConst) found = true;
  }

  @override
  void visitMethodInvocation(MethodInvocation node) => found = true;

  @override
  void visitFunctionExpressionInvocation(FunctionExpressionInvocation node) =>
      found = true;

  // An uninvoked closure's body does not supply a runtime input to its guard.
  @override
  void visitFunctionExpression(FunctionExpression node) {}
}

import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../common.dart';
import '../variables/lexical_variables.dart';

/// Defaults are constructed using the selected library's SDK types, rather
/// than spelling-based assumptions about application classes.
List<String> typedDefaults(ResolvedUnitResult unit, DartType target) {
  final types = unit.libraryElement.typeSystem;
  final provider = unit.libraryElement.typeProvider;
  if (target is VoidType) return const ['(() {})()'];
  if (target is DynamicType || target is InvalidType || target is NeverType) {
    return const [];
  }
  final values = <(String, DartType)>[
    ('0', provider.intType),
    ('1', provider.intType),
    ('-1', provider.intType),
    ('0.0', provider.doubleType),
    ('false', provider.boolType),
    ('true', provider.boolType),
    ("''", provider.stringType),
    ('null', provider.nullType),
  ];
  return [
    for (final (text, type) in values)
      if (replacementFits(types, type, target)) text,
  ];
}

/// Removing or exchanging an expression must not incidentally discard getter,
/// argument or receiver effects. Local reads and literal construction have no
/// user-defined dispatch. Fields/getters, calls, overloaded operators, late
/// reads, collection elements and dynamic accesses are deliberately excluded.
bool effectFree(Expression expression) {
  if (expression is ParenthesizedExpression)
    return effectFree(expression.expression);
  if (expression is IntegerLiteral ||
      expression is DoubleLiteral ||
      expression is BooleanLiteral ||
      expression is NullLiteral ||
      expression is SimpleStringLiteral ||
      expression is ThisExpression ||
      expression is SuperExpression)
    return true;
  if (expression is SimpleIdentifier) {
    final element = expression.element;
    if (element is ClassElement ||
        element is PrefixElement ||
        element is TopLevelFunctionElement)
      return true;
    return (element is LocalVariableElement ||
            element is FormalParameterElement) &&
        element is VariableElement &&
        !element.isLate;
  }
  if (expression is PrefixExpression &&
      const {'-', '+'}.contains(expression.operator.lexeme) &&
      (expression.operand is IntegerLiteral ||
          expression.operand is DoubleLiteral)) {
    return isSdkNumeric(expression.staticType);
  }
  return false;
}

bool resolvedArguments(ArgumentList list) =>
    list.arguments.every((argument) => argument.correspondingParameter != null);

Expression unparenthesized(Expression expression) {
  while (expression is ParenthesizedExpression) {
    expression = expression.expression;
  }
  return expression;
}

/// Suppress known literal no-ops independently of quotes, raw delimiters,
/// parenthesization and numeric formatting. Double sign-zero is retained.
(String, Object?)? literalIdentity(Expression expression) {
  expression = unparenthesized(expression);
  (String, Object?)? value;
  if (expression is SimpleStringLiteral) value = ('string', expression.value);
  if (expression is BooleanLiteral) value = ('bool', expression.value);
  if (expression is NullLiteral) value = ('null', null);
  num? numeric;
  if (expression is IntegerLiteral) numeric = expression.value;
  if (expression is DoubleLiteral) numeric = expression.value;
  if (expression is PrefixExpression &&
      const {'-', '+'}.contains(expression.operator.lexeme)) {
    final operand = unparenthesized(expression.operand);
    if (operand is IntegerLiteral) numeric = operand.value;
    if (operand is DoubleLiteral) numeric = operand.value;
    if (numeric != null && expression.operator.lexeme == '-')
      numeric = -numeric;
  }
  if (numeric != null) {
    final isDouble =
        numeric is double ||
        isSdkInterface(expression.staticType, const {'double'});
    value = (
      isDouble ? 'double' : 'int',
      isDouble ? numeric.toDouble().toString() : numeric.toString(),
    );
  }
  return value;
}

bool sameLiteralDefault(Expression expression, String replacement) {
  final value = literalIdentity(expression);
  final (String, Object?)? replacementValue = switch (replacement) {
    "''" => ('string', ''),
    'true' => ('bool', true),
    'false' => ('bool', false),
    'null' => ('null', null),
    '0' => ('int', '0'),
    '1' => ('int', '1'),
    '-1' => ('int', '-1'),
    '0.0' => ('double', '0.0'),
    _ => null,
  };
  return value != null && value == replacementValue;
}

bool samePureValue(Expression left, Expression right) {
  left = unparenthesized(left);
  right = unparenthesized(right);
  if (left is SimpleIdentifier &&
      right is SimpleIdentifier &&
      left.element != null &&
      identical(left.element, right.element))
    return true;
  final value = literalIdentity(left);
  return value != null && value == literalIdentity(right);
}

/// Argument.correspondingParameter is analyzer's instantiated binding. It
/// handles named/optional arguments, generic substitution and constructors.
/// There is no manual positional indexing or use of the original value's type.
DartType? parameterType(Argument argument) =>
    argument.correspondingParameter?.type;

bool callEffectsSafe(InvocationExpression node) {
  if (!resolvedArguments(node.argumentList) ||
      !node.argumentList.arguments.every(
        (argument) => effectFree(argument.argumentExpression),
      )) {
    return false;
  }
  if (node is MethodInvocation) {
    return !node.isCascaded &&
        !node.isNullAware &&
        node.methodName.element is ExecutableElement &&
        (node.target == null || effectFree(node.target!));
  }
  if (node is FunctionExpressionInvocation) {
    return node.staticInvokeType is FunctionType && effectFree(node.function);
  }
  return false;
}

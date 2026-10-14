import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:analyzer/dart/element/type.dart';

import '../common.dart';

const arithmeticTokens = ['+', '-', '*', '/', '%'];
const comparisonTokens = ['>', '>=', '<', '<=', '==', '!='];

OperatorScan scanAlgebra(
  ClassicOperator operator,
  ResolvedUnitResult unit,
  String source,
  void Function(Expression, AlgebraCollector) visit,
) {
  final collector = AlgebraCollector(operator, source);
  unit.unit.accept(_ExpressionVisitor(collector, visit));
  return collector.result;
}

class AlgebraCollector extends OperatorCollector {
  AlgebraCollector(super.operator, super.source);
  final _edits = <(int, int), Set<String>>{};

  String text(AstNode node) => source.substring(node.offset, node.end);

  void emit(AstNode node, String replacement, {int? offset, int? length}) {
    final start = offset ?? node.offset;
    final count = length ?? node.length;
    if (start < 0 || count <= 0 || start > source.length - count) {
      replace(node, replacement, offset: start, length: count);
      return;
    }
    if (source.substring(start, start + count) == replacement ||
        !_edits
            .putIfAbsent((start, count), () => <String>{})
            .add(replacement)) {
      return;
    }
    replace(node, replacement, offset: start, length: count);
  }

  /// One whole-expression edit preserves grouping and every original comment
  /// around the operator even when the replacement has different precedence.
  String binary(BinaryExpression node, String token) {
    final left = node.leftOperand;
    final right = node.rightOperand;
    return '(${source.substring(node.offset, left.offset)}'
        '(${text(left)})'
        '${source.substring(left.end, node.operator.offset)}$token'
        '${source.substring(node.operator.end, right.offset)}'
        '(${text(right)})'
        '${source.substring(right.end, node.end)})';
  }
}

class _ExpressionVisitor extends GeneralizingAstVisitor<void> {
  _ExpressionVisitor(this.collector, this.visit);
  final AlgebraCollector collector;
  final void Function(Expression, AlgebraCollector) visit;

  @override
  void visitNode(AstNode node) {
    if (node is Expression) visit(node, collector);
    super.visitNode(node);
  }
}

bool sdkOperator(MethodElement? method, String name, Set<String> owners) {
  final owner = method?.enclosingElement;
  return method != null &&
      method.isOperator &&
      method.name == name &&
      method.library.uri == Uri.parse('dart:core') &&
      owner is InterfaceElement &&
      owners.contains(owner.name);
}

bool sdkArithmetic(BinaryExpression expression) =>
    arithmeticTokens.contains(expression.operator.lexeme) &&
    isSdkNumeric(expression.leftOperand.staticType) &&
    isSdkNumeric(expression.rightOperand.staticType) &&
    sdkOperator(expression.element, expression.operator.lexeme, const {
      'num',
      'int',
      'double',
    });

bool sdkIntBinary(BinaryExpression expression) =>
    isSdkInterface(expression.leftOperand.staticType, const {'int'}) &&
    isSdkInterface(expression.rightOperand.staticType, const {'int'}) &&
    sdkOperator(expression.element, expression.operator.lexeme, const {'int'});

bool sdkComparison(BinaryExpression expression) {
  final token = expression.operator.lexeme;
  if (!comparisonTokens.contains(token)) return false;
  if (token != '==' && token != '!=') {
    return isSdkNumeric(expression.leftOperand.staticType) &&
        isSdkNumeric(expression.rightOperand.staticType) &&
        sdkOperator(expression.element, token, const {'num', 'int', 'double'});
  }
  // Equality may be inherited from Object. Custom overloaded equality and
  // unresolved/dynamic operand types are excluded deliberately.
  return _sdkEqualityType(expression.leftOperand.staticType) &&
      _sdkEqualityType(expression.rightOperand.staticType) &&
      sdkOperator(expression.element, '==', const {
        'Object',
        'num',
        'int',
        'double',
        'bool',
        'String',
        'Null',
      });
}

bool _sdkEqualityType(DartType? type) =>
    type is InterfaceType || type is TypeParameterType;

/// Identifiers used as a qualified name or storage target are not independent
/// value expressions. Mutating them would create malformed selectors or lvalues.
bool isValueExpression(Expression node) {
  if (node is ParenthesizedExpression) return false;
  if (node is SimpleIdentifier &&
      (node.isQualified ||
          node.inDeclarationContext() ||
          !node.inGetterContext() ||
          node.inSetterContext()))
    return false;
  AstNode current = node;
  while (current.parent is ParenthesizedExpression) {
    current = current.parent!;
  }
  final parent = current.parent;
  if (parent is AssignmentExpression && identical(parent.leftHandSide, current))
    return false;
  if (parent is PrefixExpression &&
      const {'++', '--'}.contains(parent.operator.lexeme) &&
      identical(parent.operand, current))
    return false;
  if (parent is PostfixExpression &&
      const {'++', '--'}.contains(parent.operator.lexeme) &&
      identical(parent.operand, current))
    return false;
  return true;
}

bool isOperandOfPrefix(Expression node, String token) {
  AstNode current = node;
  while (current.parent is ParenthesizedExpression) {
    current = current.parent!;
  }
  final parent = current.parent;
  return parent is PrefixExpression &&
      parent.operator.lexeme == token &&
      identical(parent.operand, current);
}

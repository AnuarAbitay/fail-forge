import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../common.dart';
import '../algebra_helpers.dart' show sdkOperator;

class StatementDeletion extends ClassicOperator {
  const StatementDeletion();
  @override
  String get id => 'STATEMENT_DELETION';
  @override
  ClassicalCategory get category => ClassicalCategory.controlFlow;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source);
  @override
  void visitExpressionStatement(ExpressionStatement node) {
    var expression = node.expression.unParenthesized;
    if (acceptsContext(expression)) {
      if (expression is AwaitExpression)
        expression = expression.expression.unParenthesized;
      final supported = switch (expression) {
        AssignmentExpression() =>
          expression.operator.lexeme == '=' &&
              expression.staticType != null &&
              expression.staticType is! DynamicType,
        MethodInvocation() => expression.methodName.element != null,
        FunctionExpressionInvocation() =>
          expression.function.staticType is FunctionType,
        PrefixExpression() =>
          {'++', '--'}.contains(expression.operator.lexeme) &&
              isSdkNumeric(expression.readType) &&
              sdkOperator(
                expression.element,
                expression.operator.lexeme == '++' ? '+' : '-',
                const {'num', 'int', 'double'},
              ),
        PostfixExpression() =>
          {'++', '--'}.contains(expression.operator.lexeme) &&
              isSdkNumeric(expression.readType) &&
              sdkOperator(
                expression.element,
                expression.operator.lexeme == '++' ? '+' : '-',
                const {'num', 'int', 'double'},
              ),
        _ => false,
      };
      if (supported) {
        replace(
          node,
          node.parent is Block || node.parent is SwitchMember ? '' : ';',
        );
      } else {
        skip(
          node,
          'unsupported_statement',
          'Only resolved calls, simple assignments and SDK numeric increments are deletable.',
        );
      }
    }
    super.visitExpressionStatement(node);
  }
}

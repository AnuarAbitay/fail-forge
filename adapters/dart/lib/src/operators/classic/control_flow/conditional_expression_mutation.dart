import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../common.dart';

class ConditionalExpressionMutation extends ClassicOperator {
  const ConditionalExpressionMutation();
  @override
  String get id => 'CONDITIONAL_EXPRESSION_MUTATION';
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
  void visitConditionalExpression(ConditionalExpression node) {
    if (acceptsContext(node)) {
      if (!isSdkBool(node.condition.staticType) ||
          node.staticType == null ||
          node.staticType is DynamicType) {
        skip(
          node,
          'unsupported_conditional_type',
          'Ternary replacement requires a resolved, nondynamic result and SDK bool condition.',
        );
      } else {
        String text(Expression expression) =>
            source.substring(expression.offset, expression.end);
        replace(node, '(${text(node.thenExpression)})');
        replace(node, '(${text(node.elseExpression)})');
        replace(
          node,
          '((${text(node.condition)}) ? (${text(node.elseExpression)}) : (${text(node.thenExpression)}))',
        );
      }
    }
    super.visitConditionalExpression(node);
  }
}

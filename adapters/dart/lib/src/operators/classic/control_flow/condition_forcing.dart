import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class ConditionForcing extends ClassicOperator {
  const ConditionForcing();
  @override
  String get id => 'CONDITION_FORCING';
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
  void _condition(Expression expression) {
    if (!acceptsContext(expression)) return;
    if (!isSdkBool(expression.staticType)) {
      skip(
        expression,
        'unsupported_condition_type',
        'Conditions must have a resolved SDK bool type.',
      );
      return;
    }
    for (final value in ['true', 'false']) {
      final original = expression.unParenthesized;
      if (original is BooleanLiteral && original.value == (value == 'true')) {
        skip(
          expression,
          'unchanged_condition_value',
          'The literal condition already has the requested truth value.',
        );
        continue;
      }
      replace(expression, value);
    }
  }

  @override
  void visitIfStatement(IfStatement node) {
    if (node.caseClause == null) {
      _condition(node.expression);
    } else {
      skip(
        node,
        'pattern_condition',
        'If-case selectors are not boolean conditions.',
      );
    }
    super.visitIfStatement(node);
  }

  @override
  void visitWhileStatement(WhileStatement node) {
    _condition(node.condition);
    super.visitWhileStatement(node);
  }

  @override
  void visitDoStatement(DoStatement node) {
    _condition(node.condition);
    super.visitDoStatement(node);
  }

  @override
  void visitForStatement(ForStatement node) {
    final parts = node.forLoopParts;
    if (parts is ForParts && parts.condition != null) {
      _condition(parts.condition!);
    } else {
      skip(
        node,
        'no_boolean_loop_condition',
        'For-in and unconditional loops have no replaceable boolean condition.',
      );
    }
    super.visitForStatement(node);
  }
}

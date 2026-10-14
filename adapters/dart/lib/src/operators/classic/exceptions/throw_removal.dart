import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class ThrowRemoval extends ClassicOperator {
  const ThrowRemoval();
  @override
  String get id => 'THROW_REMOVAL';
  @override
  ClassicalCategory get category => ClassicalCategory.exceptions;
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
  void visitThrowExpression(ThrowExpression node) {
    if (acceptsContext(node)) {
      AstNode parent = node;
      while (parent.parent is ParenthesizedExpression) {
        parent = parent.parent!;
      }
      if (parent.parent is ExpressionStatement) {
        replace(parent.parent!, ';');
      } else {
        skip(
          node,
          'expression_throw',
          'Throw deletion supports standalone statements only.',
        );
      }
    }
    super.visitThrowExpression(node);
  }
}

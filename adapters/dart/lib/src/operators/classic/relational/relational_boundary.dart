import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class RelationalBoundary extends ClassicOperator {
  const RelationalBoundary();

  @override
  String get id => 'RELATIONAL_BOUNDARY';

  @override
  ClassicalCategory get category => ClassicalCategory.relational;

  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final visitor = _Visitor(this, source);
    result.unit.accept(visitor);
    return visitor.result;
  }
}

class _Visitor extends OperatorCollector {
  _Visitor(super.operator, super.source);

  static const replacements = {'>': '>=', '>=': '>', '<': '<=', '<=': '<'};

  @override
  void visitBinaryExpression(BinaryExpression node) {
    final replacement = replacements[node.operator.lexeme];
    if (replacement != null && acceptsContext(node)) {
      final element = node.element;
      final owner = element?.enclosingElement;
      final sdkOperation =
          element != null &&
          element.isOperator &&
          element.name == node.operator.lexeme &&
          element.library.uri.toString() == 'dart:core' &&
          const {'num', 'int', 'double'}.contains(owner?.name);
      if (!sdkOperation ||
          !isSdkNumeric(node.leftOperand.staticType) ||
          !isSdkNumeric(node.rightOperand.staticType)) {
        skip(
          node,
          'unsupported_numeric_operation',
          'Boundary mutation requires an SDK num/int/double operator and numeric operands.',
        );
      } else {
        replace(
          node,
          replacement,
          offset: node.operator.offset,
          length: node.operator.length,
        );
      }
    }
    super.visitBinaryExpression(node);
  }
}

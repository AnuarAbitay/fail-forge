import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class LogicalOperandRemoval extends ClassicOperator {
  const LogicalOperandRemoval();
  @override
  String get id => 'LOGICAL_OPERAND_REMOVAL';
  @override
  ClassicalCategory get category => ClassicalCategory.logical;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (node is! BinaryExpression ||
            !const {'&&', '||'}.contains(node.operator.lexeme))
          return;
        if (!out.acceptsContext(node)) return;
        if (!isSdkBool(node.leftOperand.staticType) ||
            !isSdkBool(node.rightOperand.staticType) ||
            !isSdkBool(node.staticType)) {
          out.skip(
            node,
            'unsupported_logical',
            'Logical operand removal requires SDK bool operands.',
          );
          return;
        }
        for (final operand in [node.leftOperand, node.rightOperand]) {
          out.emit(node, '(${out.text(operand)})');
        }
      });
}

import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class ArithmeticOperandRemoval extends ClassicOperator {
  const ArithmeticOperandRemoval();
  @override
  String get id => 'ARITHMETIC_OPERAND_REMOVAL';
  @override
  ClassicalCategory get category => ClassicalCategory.arithmetic;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (node is! BinaryExpression ||
            !arithmeticTokens.contains(node.operator.lexeme))
          return;
        if (!out.acceptsContext(node)) return;
        if (!sdkArithmetic(node)) {
          out.skip(
            node,
            'unsupported_arithmetic',
            'Operand removal requires resolved SDK arithmetic.',
          );
          return;
        }
        for (final operand in [node.leftOperand, node.rightOperand]) {
          out.emit(node, '(${out.text(operand)})');
        }
      });
}

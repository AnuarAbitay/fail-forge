import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class ArithmeticOperatorReplacement extends ClassicOperator {
  const ArithmeticOperatorReplacement();
  @override
  String get id => 'ARITHMETIC_OPERATOR_REPLACEMENT';
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
            'Arithmetic requires resolved SDK numeric operands and operator.',
          );
          return;
        }
        final primary = const {
          '+': '-',
          '-': '+',
          '*': '/',
          '/': '*',
          '%': '*',
        }[node.operator.lexeme]!;
        for (final token in [primary, ...arithmeticTokens]) {
          if (token != node.operator.lexeme)
            out.emit(node, out.binary(node, token));
        }
      });
}

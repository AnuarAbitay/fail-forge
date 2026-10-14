import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class ShiftOperatorReplacement extends ClassicOperator {
  const ShiftOperatorReplacement();
  @override
  String get id => 'SHIFT_OPERATOR_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.bitwise;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (node is! BinaryExpression ||
            !const {'<<', '>>', '>>>'}.contains(node.operator.lexeme))
          return;
        if (!out.acceptsContext(node)) return;
        if (!sdkIntBinary(node)) {
          out.skip(
            node,
            'unsupported_shift',
            'Shift mutation requires resolved SDK int operations.',
          );
          return;
        }
        for (final token in const ['<<', '>>', '>>>']) {
          if (token != node.operator.lexeme)
            out.emit(node, out.binary(node, token));
        }
      });
}

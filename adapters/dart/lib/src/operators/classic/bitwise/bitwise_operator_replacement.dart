import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class BitwiseOperatorReplacement extends ClassicOperator {
  const BitwiseOperatorReplacement();
  @override
  String get id => 'BITWISE_OPERATOR_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.bitwise;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (node is! BinaryExpression ||
            !const {'&', '|', '^'}.contains(node.operator.lexeme))
          return;
        if (!out.acceptsContext(node)) return;
        if (!sdkIntBinary(node)) {
          out.skip(
            node,
            'unsupported_bitwise',
            'Bitwise mutation requires resolved SDK int operations.',
          );
          return;
        }
        final replacements = const {
          '&': ['|'],
          '|': ['&'],
          '^': ['&', '|'],
        }[node.operator.lexeme]!;
        for (final token in replacements)
          out.emit(node, out.binary(node, token));
      });
}

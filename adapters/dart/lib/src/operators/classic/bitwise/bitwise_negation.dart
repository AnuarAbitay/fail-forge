import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class BitwiseNegation extends ClassicOperator {
  const BitwiseNegation();
  @override
  String get id => 'BITWISE_NEGATION';
  @override
  ClassicalCategory get category => ClassicalCategory.bitwise;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (!isSdkInterface(node.staticType, const {'int'}) ||
            !isValueExpression(node) ||
            isOperandOfPrefix(node, '~'))
          return;
        if (!out.acceptsContext(node)) return;
        if (node is PrefixExpression && node.operator.lexeme == '~') {
          if (!sdkOperator(node.element, '~', const {'int'})) return;
          out.emit(node, '(${out.text(node.operand)})');
        } else {
          out.emit(node, '(~(${out.text(node)}))');
        }
      });
}

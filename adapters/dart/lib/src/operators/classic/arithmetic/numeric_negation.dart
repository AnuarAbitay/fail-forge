import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class NumericNegation extends ClassicOperator {
  const NumericNegation();
  @override
  String get id => 'NUMERIC_NEGATION';
  @override
  ClassicalCategory get category => ClassicalCategory.arithmetic;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (!isSdkNumeric(node.staticType) ||
            !isValueExpression(node) ||
            isOperandOfPrefix(node, '-'))
          return;
        if (!out.acceptsContext(node)) return;
        if (node is PrefixExpression && node.operator.lexeme == '-') {
          if (!sdkOperator(node.element, '-', const {'num', 'int', 'double'}))
            return;
          out.emit(node, '(${out.text(node.operand)})');
        } else {
          out.emit(node, '(-(${out.text(node)}))');
        }
      });
}

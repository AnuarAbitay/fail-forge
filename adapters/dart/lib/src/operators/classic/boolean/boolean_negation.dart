import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class BooleanNegation extends ClassicOperator {
  const BooleanNegation();
  @override
  String get id => 'BOOLEAN_NEGATION';
  @override
  ClassicalCategory get category => ClassicalCategory.boolean;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (!isSdkBool(node.staticType) ||
            !isValueExpression(node) ||
            isOperandOfPrefix(node, '!'))
          return;
        if (!out.acceptsContext(node)) return;
        if (node is PrefixExpression && node.operator.lexeme == '!') {
          out.emit(node, '(${out.text(node.operand)})');
        } else {
          out.emit(node, '(!(${out.text(node)}))');
        }
      });
}

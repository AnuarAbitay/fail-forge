import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class CompoundAssignmentReplacement extends ClassicOperator {
  const CompoundAssignmentReplacement();
  @override
  String get id => 'COMPOUND_ASSIGNMENT_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.arithmetic;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        if (node is! AssignmentExpression) return;
        final replacement = const {
          '+=': '-=',
          '-=': '+=',
          '*=': '/=',
          '/=': '*=',
          '%=': '*=',
        }[node.operator.lexeme];
        if (replacement == null || !out.acceptsContext(node)) return;
        if (!isSdkNumeric(node.readType) ||
            !isSdkNumeric(node.rightHandSide.staticType) ||
            !sdkOperator(
              node.element,
              node.operator.lexeme.substring(0, 1),
              const {'num', 'int', 'double'},
            )) {
          out.skip(
            node,
            'unsupported_arithmetic',
            'Compound assignment requires resolved SDK arithmetic.',
          );
          return;
        }
        out.emit(
          node,
          replacement,
          offset: node.operator.offset,
          length: node.operator.length,
        );
      });
}

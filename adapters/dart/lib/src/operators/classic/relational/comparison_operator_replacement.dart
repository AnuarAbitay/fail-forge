import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class ComparisonOperatorReplacement extends ClassicOperator {
  const ComparisonOperatorReplacement();
  @override
  String get id => 'COMPARISON_OPERATOR_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.relational;
  @override
  OperatorScan discover(
    ResolvedUnitResult result,
    String source,
  ) => scanAlgebra(this, result, source, (node, out) {
    if (node is! BinaryExpression ||
        !comparisonTokens.contains(node.operator.lexeme))
      return;
    if (!out.acceptsContext(node)) return;
    if (!sdkComparison(node)) {
      out.skip(
        node,
        'unsupported_comparison',
        'Comparison requires resolved SDK operands and operator.',
      );
      return;
    }
    final numeric =
        isSdkNumeric(node.leftOperand.staticType) &&
        isSdkNumeric(node.rightOperand.staticType);
    for (final token in numeric ? comparisonTokens : const ['==', '!=']) {
      if (token != node.operator.lexeme) {
        // Same-precedence edits remain canonical token patches so overlapping
        // boundary/negation operators deduplicate to identical source bytes.
        final originalEquality = const {
          '==',
          '!=',
        }.contains(node.operator.lexeme);
        final replacementEquality = const {'==', '!='}.contains(token);
        if (originalEquality == replacementEquality) {
          out.emit(
            node,
            token,
            offset: node.operator.offset,
            length: node.operator.length,
          );
        } else {
          out.emit(node, out.binary(node, token));
        }
      }
    }
  });
}

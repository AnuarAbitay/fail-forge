import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class ComparisonNegation extends ClassicOperator {
  const ComparisonNegation();
  @override
  String get id => 'COMPARISON_NEGATION';
  @override
  ClassicalCategory get category => ClassicalCategory.relational;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
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
        final replacement = const {
          '>': '<=',
          '>=': '<',
          '<': '>=',
          '<=': '>',
          '==': '!=',
          '!=': '==',
        }[node.operator.lexeme]!;
        out.emit(
          node,
          replacement,
          offset: node.operator.offset,
          length: node.operator.length,
        );
      });
}

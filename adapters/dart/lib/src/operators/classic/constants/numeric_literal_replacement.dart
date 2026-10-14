import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class NumericLiteralReplacement extends ClassicOperator {
  const NumericLiteralReplacement();
  @override
  String get id => 'NUMERIC_LITERAL_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.constants;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        num? value;
        var signed = false;
        if (node is IntegerLiteral) {
          value = isSdkInterface(node.staticType, const {'double'})
              ? node.parseDoubleValue(negated: false)
              : node.parseIntValue(negated: false);
        }
        if (node is DoubleLiteral) value = node.value;
        if (node is PrefixExpression && node.operator.lexeme == '-') {
          final operand = node.operand;
          if (operand is IntegerLiteral) {
            value = isSdkInterface(node.staticType, const {'double'})
                ? operand.parseDoubleValue(negated: true)
                : operand.parseIntValue(negated: true);
          }
          if (operand is DoubleLiteral) value = -operand.value;
          signed = value != null;
        }
        if (value == null || (!signed && isOperandOfPrefix(node, '-'))) return;
        if (!isSdkNumeric(node.staticType) || !out.acceptsContext(node)) return;
        if (!value.isFinite) {
          out.skip(
            node,
            'nonfinite_literal',
            'Nonfinite numeric literals are not mutated.',
          );
          return;
        }
        final original = value.toString();
        final replacements = value is int
            ? <num>[0, 1, -1, value + 1, value - 1, -value]
            : <num>[0.0, 1.0, -1.0, value + 1.0, value - 1.0, -value];
        final seen = <String>{original};
        for (final replacement in replacements) {
          if (!replacement.isFinite) continue;
          final text = replacement.toString();
          if (seen.add(text)) out.emit(node, '($text)');
        }
      });
}

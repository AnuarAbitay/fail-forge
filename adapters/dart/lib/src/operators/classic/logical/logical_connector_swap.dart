import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class LogicalConnectorSwap extends ClassicOperator {
  const LogicalConnectorSwap();

  @override
  String get id => 'LOGICAL_CONNECTOR_SWAP';

  @override
  ClassicalCategory get category => ClassicalCategory.logical;

  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final visitor = _Visitor(this, source);
    result.unit.accept(visitor);
    return visitor.result;
  }
}

class _Visitor extends OperatorCollector {
  _Visitor(super.operator, super.source);

  @override
  void visitBinaryExpression(BinaryExpression node) {
    final token = node.operator;
    final replacement = switch (token.lexeme) {
      '&&' => '||',
      '||' => '&&',
      _ => null,
    };
    if (replacement != null && acceptsContext(node)) {
      if (!isSdkBool(node.staticType)) {
        skip(
          node,
          'unresolved_logical_type',
          'The logical expression is not resolved SDK bool.',
        );
      } else {
        // Logical connectors are intrinsic syntax, not user-overloaded methods.
        // Both operands and the whole node must keep their original AST groups.
        // A token-only edit of the inner && in a&&b&&c would become a||(b&&c)
        // instead of the intended (a||b)&&c. Preserve all intervening trivia.
        // Flow-promotion validity is checked separately in the library context.
        final left = node.leftOperand;
        final right = node.rightOperand;
        if (node.offset < 0 ||
            left.offset < node.offset ||
            left.end > token.offset ||
            token.end > right.offset ||
            right.end > node.end ||
            node.end > source.length) {
          skip(
            node,
            'invalid_ast_range',
            'The logical operand ranges are invalid.',
          );
        } else {
          final rewritten =
              '(' +
              source.substring(node.offset, left.offset) +
              '(' +
              source.substring(left.offset, left.end) +
              ')' +
              source.substring(left.end, token.offset) +
              replacement +
              source.substring(token.end, right.offset) +
              '(' +
              source.substring(right.offset, right.end) +
              ')' +
              source.substring(right.end, node.end) +
              ')';
          replace(node, rewritten);
        }
      }
    }
    super.visitBinaryExpression(node);
  }
}

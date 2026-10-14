import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../algebra_helpers.dart';

class IncrementDecrementReplacement extends ClassicOperator {
  const IncrementDecrementReplacement();
  @override
  String get id => 'INCREMENT_DECREMENT_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.arithmetic;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      scanAlgebra(this, result, source, (node, out) {
        final (operand, token, method) = switch (node) {
          PrefixExpression() => (node.operand, node.operator, node.element),
          PostfixExpression() => (node.operand, node.operator, node.element),
          _ => (null, null, null),
        };
        if (operand == null ||
            token == null ||
            !const {'++', '--'}.contains(token.lexeme))
          return;
        if (!out.acceptsContext(node)) return;
        final readType = node is CompoundAssignmentExpression
            ? node.readType
            : null;
        if (!isSdkNumeric(readType) ||
            !sdkOperator(method, token.lexeme == '++' ? '+' : '-', const {
              'num',
              'int',
              'double',
            })) {
          out.skip(
            node,
            'unsupported_increment',
            'Increment/decrement requires resolved SDK arithmetic.',
          );
          return;
        }
        out.emit(
          node,
          token.lexeme == '++' ? '--' : '++',
          offset: token.offset,
          length: token.length,
        );
      });
}

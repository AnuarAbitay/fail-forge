import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class ConditionalNegation extends ClassicOperator {
  const ConditionalNegation();

  @override
  String get id => 'CONDITIONAL_NEGATION';

  /// Kept for existing explicit configurations; BOOLEAN_NEGATION supersedes it.
  @override
  bool get isLegacy => true;

  @override
  ClassicalCategory get category => ClassicalCategory.boolean;

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
  void visitIfStatement(IfStatement node) {
    final expression = node.expression;
    if (node.caseClause != null) {
      skip(node, 'pattern_condition', 'P0 excludes if-case conditions.');
    } else if (acceptsContext(expression)) {
      if (!isSdkBool(expression.staticType)) {
        skip(
          node,
          'unsupported_condition_type',
          'The condition is not resolved SDK bool.',
        );
      } else if (expression.offset < 0 ||
          expression.length <= 0 ||
          expression.end > source.length) {
        skip(node, 'invalid_ast_range', 'The condition range is invalid.');
      } else {
        // Preserve the exact original expression and evaluate it exactly once.
        final original = source.substring(expression.offset, expression.end);
        replace(
          node,
          '!(' + original + ')',
          offset: expression.offset,
          length: expression.length,
        );
      }
    }
    super.visitIfStatement(node);
  }
}

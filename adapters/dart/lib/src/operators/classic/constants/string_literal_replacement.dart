import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class StringLiteralReplacement extends ClassicOperator {
  const StringLiteralReplacement();
  @override
  String get id => 'STRING_LITERAL_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.constants;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source);
  @override
  void visitSimpleStringLiteral(SimpleStringLiteral node) {
    if (node.parent is AdjacentStrings || node.parent is StringInterpolation) {
      skip(
        node,
        'compound_string',
        'Adjacent/interpolated strings are not replaced.',
      );
    } else if (isSdkInterface(node.staticType, const {'String'}) &&
        acceptsContext(node)) {
      replace(node, node.value.isEmpty ? "'mutated'" : "''");
    }
    super.visitSimpleStringLiteral(node);
  }

  @override
  void visitStringInterpolation(StringInterpolation node) {
    skip(node, 'interpolated_string', 'String interpolation is not replaced.');
    // A nested expression can contain a separate executable string literal;
    // conservatively keep the complete interpolation outside this operator.
  }
}

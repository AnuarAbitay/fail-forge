import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class BooleanLiteralFlip extends ClassicOperator {
  const BooleanLiteralFlip();

  @override
  String get id => 'BOOLEAN_LITERAL_FLIP';

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
  void visitBooleanLiteral(BooleanLiteral node) {
    if (acceptsContext(node)) {
      if (isSdkBool(node.staticType)) {
        replace(node, node.value ? 'false' : 'true');
      } else {
        skip(
          node,
          'unresolved_boolean_type',
          'The literal type is not SDK bool.',
        );
      }
    }
    super.visitBooleanLiteral(node);
  }
}

import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import 'call_semantics.dart';

class FunctionCallRemoval extends ClassicOperator {
  const FunctionCallRemoval();
  @override
  String get id => 'FUNCTION_CALL_REMOVAL';
  @override
  ClassicalCategory get category => ClassicalCategory.calls;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source, result);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source, this.unit);
  final ResolvedUnitResult unit;
  void _call(InvocationExpression node) {
    if (!acceptsContext(node)) return;
    if (!callEffectsSafe(node)) {
      skip(
        node,
        'effectful_or_unresolved_call',
        'Call receivers and arguments must be resolved and effect-free.',
      );
      return;
    }
    AstNode outer = node;
    while (outer.parent is ParenthesizedExpression) {
      outer = outer.parent!;
    }
    if (outer.parent case final ExpressionStatement statement) {
      replace(statement, statement.parent is Block ? '' : ';');
      return;
    }
    final type = node.staticType;
    if (type == null) {
      skip(
        node,
        'unresolved_return_type',
        'The invocation has no resolved return type.',
      );
      return;
    }
    final defaults = typedDefaults(unit, type);
    if (defaults.isEmpty) {
      skip(
        node,
        'unsupported_return_type',
        'No safe typed default exists for this invocation result.',
      );
    }
    for (final replacement in defaults) {
      replace(node, '($replacement)');
    }
  }

  @override
  void visitMethodInvocation(MethodInvocation node) {
    _call(node);
    super.visitMethodInvocation(node);
  }

  @override
  void visitFunctionExpressionInvocation(FunctionExpressionInvocation node) {
    _call(node);
    super.visitFunctionExpressionInvocation(node);
  }
}

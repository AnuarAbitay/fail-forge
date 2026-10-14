import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../variables/lexical_variables.dart';
import 'call_semantics.dart';

class ArgumentPermutation extends ClassicOperator {
  const ArgumentPermutation();
  @override
  String get id => 'ARGUMENT_PERMUTATION';
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
  @override
  void visitArgumentList(ArgumentList node) {
    if (node.parent is! InvocationExpression &&
        node.parent is! InstanceCreationExpression) {
      super.visitArgumentList(node);
      return;
    }
    if (node.arguments.length > 128) {
      skip(
        node,
        'argument_limit',
        'Argument permutation is bounded to 128 arguments per call.',
      );
      return;
    }
    final arguments = node.arguments;
    for (var i = 0; i < arguments.length; i++) {
      final left = arguments[i].argumentExpression;
      final leftParameter = parameterType(arguments[i]);
      if (!acceptsContext(left) ||
          !effectFree(left) ||
          leftParameter == null ||
          left.staticType == null)
        continue;
      for (var j = i + 1; j < arguments.length; j++) {
        // Named values exchange while their labels stay fixed. Mixed
        // positional/named pairs would change parameter bindings and are not
        // permutations of this signature.
        if ((arguments[i] is NamedArgument) !=
            (arguments[j] is NamedArgument)) {
          continue;
        }
        final right = arguments[j].argumentExpression;
        final rightParameter = parameterType(arguments[j]);
        if (unsupportedExpressionContext(right) != null ||
            !effectFree(right) ||
            rightParameter == null ||
            right.staticType == null)
          continue;
        final types = unit.libraryElement.typeSystem;
        if (!replacementFits(types, right.staticType!, leftParameter) ||
            !replacementFits(types, left.staticType!, rightParameter))
          continue;
        final leftText = source.substring(left.offset, left.end);
        final rightText = source.substring(right.offset, right.end);
        if (leftText == rightText || samePureValue(left, right)) continue;
        replace(
          left,
          '$rightText${source.substring(left.end, right.offset)}$leftText',
          offset: left.offset,
          length: right.end - left.offset,
        );
      }
    }
    super.visitArgumentList(node);
  }
}

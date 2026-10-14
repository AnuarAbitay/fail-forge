import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../variables/lexical_variables.dart';
import 'call_semantics.dart';

class ArgumentReplacement extends ClassicOperator {
  const ArgumentReplacement();
  @override
  String get id => 'ARGUMENT_REPLACEMENT';
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
  final scope = LexicalScope();
  @override
  void visitArgumentList(ArgumentList node) {
    if (node.parent is! InvocationExpression &&
        node.parent is! InstanceCreationExpression) {
      super.visitArgumentList(node);
      return;
    }
    for (final argument in node.arguments) {
      final expression = argument.argumentExpression;
      final type = parameterType(argument);
      if (!acceptsContext(expression)) continue;
      if (type == null || !effectFree(expression)) {
        skip(
          expression,
          'unsupported_argument',
          'Arguments require resolved bindings and effect-free values.',
        );
        continue;
      }
      final replacements = <String>{
        for (final replacement in typedDefaults(unit, type))
          if (!sameLiteralDefault(expression, replacement)) replacement,
      };
      final originalExpression = unparenthesized(expression);
      for (final variable in scope.variablesAt(expression)) {
        if (!(originalExpression is SimpleIdentifier &&
                identical(originalExpression.element, variable.element)) &&
            replacementFits(
              unit.libraryElement.typeSystem,
              variable.type,
              type,
            )) {
          replacements.add(variable.name);
        }
      }
      final original = source.substring(expression.offset, expression.end);
      for (final replacement in replacements) {
        if (replacement != original) replace(expression, replacement);
      }
    }
    super.visitArgumentList(node);
  }
}

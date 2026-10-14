import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../common.dart';
import '../../failure_modes/symbols.dart';

class ExceptionReplacement extends ClassicOperator {
  const ExceptionReplacement();
  @override
  String get id => 'EXCEPTION_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.exceptions;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source, SdkSymbols(result));
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source, this.symbols);
  final SdkSymbols symbols;
  @override
  void visitThrowExpression(ThrowExpression node) {
    final expression = node.expression.unParenthesized;
    final type = expression.staticType;
    if (acceptsContext(node)) {
      if (expression is! InstanceCreationExpression ||
          type is! InterfaceType ||
          type.element.library.uri != Uri.parse('dart:core') ||
          !{
            'StateError',
            'ArgumentError',
            'Exception',
          }.contains(type.element.name)) {
        skip(
          node,
          'unsupported_exception_expression',
          'Only direct resolved SDK StateError, ArgumentError and Exception constructions are supported.',
        );
      } else {
        for (final name in ['StateError', 'ArgumentError', 'Exception']) {
          final available = symbols.available(
            node,
            name,
            Uri.parse('dart:core'),
            replacingBody: false,
          );
          if (available == null) {
            skip(
              node,
              'exception_symbol_unavailable',
              'No unshadowed nondeferred $name constructor is available.',
            );
            continue;
          }
          if (name == type.element.name &&
              expression.constructorName.name == null &&
              expression.argumentList.arguments.length == 1) {
            final message = expression
                .argumentList
                .arguments
                .single
                .argumentExpression
                .unParenthesized;
            if (message is StringLiteral && message.stringValue == '') {
              skip(
                expression,
                'unchanged_exception_value',
                'The same SDK constructor already has an empty message.',
              );
              continue;
            }
          }
          replace(expression, "$available('')");
        }
      }
    }
    super.visitThrowExpression(node);
  }
}

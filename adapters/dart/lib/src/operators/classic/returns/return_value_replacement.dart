import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../common.dart';
import '../../failure_modes/symbols.dart';
import 'replacement_values.dart';

class ReturnValueReplacement extends ClassicOperator {
  const ReturnValueReplacement();
  @override
  String get id => 'RETURN_VALUE_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.returns;
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
  final typeBindings = ValueTypeBindings();
  @override
  void visitReturnStatement(ReturnStatement node) {
    final expression = node.expression;
    if (expression == null) {
      skip(
        node,
        'bare_return',
        'A return without a value has no replacement expression.',
      );
    } else {
      _value(expression);
    }
    super.visitReturnStatement(node);
  }

  @override
  void visitExpressionFunctionBody(ExpressionFunctionBody node) {
    _value(node.expression);
    super.visitExpressionFunctionBody(node);
  }

  void _value(Expression expression) {
    if (!acceptsContext(expression)) return;
    FunctionBody? body;
    for (
      AstNode? current = expression.parent;
      current != null;
      current = current.parent
    ) {
      if (current is FunctionBody) {
        body = current;
        break;
      }
    }
    if (body == null ||
        body.isGenerator ||
        body.parent is ConstructorDeclaration) {
      skip(
        expression,
        'unsupported_return_context',
        'Constructors and generators have no supported return replacement.',
      );
      return;
    }
    final owner = body.parent;
    DartType? type = switch (owner) {
      MethodDeclaration() => owner.declaredFragment?.element.returnType,
      FunctionExpression() =>
        (owner.staticType is FunctionType)
            ? (owner.staticType as FunctionType).returnType
            : null,
      _ => null,
    };
    if (body.isAsynchronous &&
        type is InterfaceType &&
        type.element.library.uri == Uri.parse('dart:async') &&
        {'Future', 'FutureOr'}.contains(type.element.name)) {
      type = type.typeArguments.single;
    }
    final replacements = type == null
        ? <String>[]
        : returnReplacements(type, symbols, expression, typeBindings);
    if (replacements.isEmpty) {
      skip(
        expression,
        'unsupported_return_type',
        'Return replacement requires a resolved supported type.',
      );
      return;
    }
    for (final replacement in replacements) {
      if (isUnchangedReturnValue(expression, replacement)) {
        skip(
          expression,
          'unchanged_return_value',
          'The replacement has the same literal return value.',
        );
        continue;
      }
      replace(expression, replacement);
    }
  }
}

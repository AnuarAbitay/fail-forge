import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:analyzer/dart/element/nullability_suffix.dart';
import 'package:analyzer/dart/element/type.dart';

import '../common.dart';
import 'symbols.dart';

final _channelUri = Uri.parse(
  'package:flutter/src/services/platform_channel.dart',
);
final _exceptionUri = Uri.parse(
  'package:flutter/src/services/message_codec.dart',
);

class MethodChannelError extends MutationOperator {
  const MethodChannelError();

  @override
  String get id => 'METHOD_CHANNEL_ERROR';
  @override
  MutationFamily get family => MutationFamily.failureMode;

  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source, result);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source, ResolvedUnitResult unit)
    : symbols = SdkSymbols(unit);
  final SdkSymbols symbols;

  @override
  void visitMethodInvocation(MethodInvocation node) {
    if (node.methodName.name != 'invokeMethod') {
      super.visitMethodInvocation(node);
      return;
    }
    final element = node.methodName.element;
    final owner = element?.enclosingElement;
    final receiver = node.target;
    final receiverType = receiver?.staticType;
    if (element is! MethodElement ||
        owner is! InterfaceElement ||
        owner.name != 'MethodChannel' ||
        owner.library.uri != _channelUri ||
        receiverType is! InterfaceType ||
        receiverType.nullabilitySuffix != NullabilitySuffix.none ||
        receiverType.element.name != 'MethodChannel' ||
        receiverType.element.library.uri != _channelUri ||
        node.staticType?.isDartAsyncFuture != true ||
        node.isCascaded ||
        node.isNullAware) {
      skip(
        node,
        'unsupported_method_channel',
        'Only a resolved SDK MethodChannel.invokeMethod with a nonnullable exact receiver is supported.',
      );
    } else if (!_directlyAwaited(node)) {
      skip(
        node,
        'not_directly_awaited',
        'MethodChannel failure injection requires a directly awaited invocation.',
      );
    } else if (!acceptsContext(node)) {
      // acceptsContext records the unsupported executable context.
    } else if (!_safeRead(receiver!) ||
        !node.argumentList.arguments.every(
          (argument) => _safeRead(argument.argumentExpression),
        )) {
      skip(
        node,
        'side_effecting_evaluation',
        'Receiver and arguments must be literals or non-late local/parameter reads.',
      );
    } else {
      for (final className in const [
        'PlatformException',
        'MissingPluginException',
      ]) {
        final exception = symbols.available(
          node,
          className,
          _exceptionUri,
          replacingBody: false,
        );
        if (exception == null) {
          skip(
            node,
            'sdk_exception_not_available',
            'No unshadowed imported SDK $className is available.',
          );
          continue;
        }
        final arguments = className == 'PlatformException'
            ? "code: 'FAILFORGE_METHOD_CHANNEL_ERROR'"
            : "'FAILFORGE_METHOD_CHANNEL_ERROR'";
        // An async throw expression infers Future<Never>, preserving asynchronous
        // failure for every invokeMethod<T> result without naming Future.
        replace(node, '(() async => throw $exception($arguments))()');
      }
    }
    super.visitMethodInvocation(node);
  }
}

bool _directlyAwaited(Expression expression) {
  AstNode current = expression;
  while (current.parent is ParenthesizedExpression) {
    current = current.parent!;
  }
  return current.parent is AwaitExpression;
}

bool _safeRead(Expression expression) {
  final value = expression.unParenthesized;
  if (value is SimpleStringLiteral ||
      value is IntegerLiteral ||
      value is DoubleLiteral ||
      value is BooleanLiteral ||
      value is NullLiteral) {
    return true;
  }
  if (value is! SimpleIdentifier) return false;
  final element = value.element;
  return element is FormalParameterElement ||
      (element is LocalVariableElement && !element.isLate);
}

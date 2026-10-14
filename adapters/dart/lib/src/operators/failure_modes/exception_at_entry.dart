import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../common.dart';
import 'symbols.dart';

class ExceptionAtEntry extends MutationOperator {
  const ExceptionAtEntry();

  @override
  String get id => 'EXCEPTION_AT_ENTRY';
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
  void visitFunctionDeclaration(FunctionDeclaration node) {
    if (node.parent is CompilationUnit) {
      _body(node.functionExpression.body, node.externalKeyword != null);
    } else {
      skip(
        node,
        'local_function',
        'Entry failures support named top-level functions and methods.',
      );
    }
    super.visitFunctionDeclaration(node);
  }

  @override
  void visitMethodDeclaration(MethodDeclaration node) {
    _body(node.body, node.externalKeyword != null);
    super.visitMethodDeclaration(node);
  }

  void _body(FunctionBody body, bool external) {
    if (external ||
        body is EmptyFunctionBody ||
        body.isGenerator ||
        (body is! BlockFunctionBody && body is! ExpressionFunctionBody)) {
      skip(
        body,
        'unsupported_function_body',
        'Entry failures exclude external, abstract and generator bodies.',
      );
      return;
    }
    final exception = symbols.available(
      body,
      'StateError',
      Uri.parse('dart:core'),
      replacingBody: true,
    );
    if (exception == null) {
      skip(
        body,
        'sdk_exception_not_available',
        'No unshadowed imported SDK StateError is available.',
      );
      return;
    }
    final async = body.isAsynchronous ? 'async ' : '';
    replace(
      body,
      '${async}{ throw $exception(\'FAILFORGE_EXCEPTION_AT_ENTRY\'); }',
    );
  }
}

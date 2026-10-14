import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class BranchBodyRemoval extends ClassicOperator {
  const BranchBodyRemoval();
  @override
  String get id => 'BRANCH_BODY_REMOVAL';
  @override
  ClassicalCategory get category => ClassicalCategory.controlFlow;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source);
  void _remove(Statement statement) {
    if (statement is EmptyStatement ||
        (statement is Block && statement.statements.isEmpty)) {
      skip(statement, 'empty_branch', 'The branch is already empty.');
      return;
    }
    replace(statement, '{}');
  }

  @override
  void visitIfStatement(IfStatement node) {
    _remove(node.thenStatement);
    final otherwise = node.elseStatement;
    if (otherwise != null) _remove(otherwise);
    super.visitIfStatement(node);
  }
}

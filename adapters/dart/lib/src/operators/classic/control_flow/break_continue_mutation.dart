import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';

class BreakContinueMutation extends ClassicOperator {
  const BreakContinueMutation();
  @override
  String get id => 'BREAK_CONTINUE_MUTATION';
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
  AstNode? _target(AstNode node) {
    for (
      var parent = node.parent;
      parent != null && parent is! FunctionBody;
      parent = parent.parent
    ) {
      if (parent is WhileStatement ||
          parent is DoStatement ||
          parent is ForStatement ||
          parent is SwitchStatement)
        return parent;
    }
    return null;
  }

  @override
  void visitBreakStatement(BreakStatement node) {
    if (node.label != null) {
      skip(
        node,
        'labelled_control_flow',
        'Labelled control-flow targets are unsupported.',
      );
    } else {
      final target = _target(node);
      if (target == null) {
        skip(
          node,
          'unresolved_control_target',
          'No loop or switch target exists.',
        );
      } else {
        replace(node, ';');
        if (target is! SwitchStatement) replace(node, 'continue;');
      }
    }
    super.visitBreakStatement(node);
  }

  @override
  void visitContinueStatement(ContinueStatement node) {
    if (node.label != null) {
      skip(
        node,
        'labelled_control_flow',
        'Labelled control-flow targets are unsupported.',
      );
    } else {
      AstNode? target = _target(node);
      final insideSwitch = target is SwitchStatement;
      // A switch is not a target for an unlabelled continue. Continue searches
      // for an enclosing loop, stopping at the current function boundary.
      while (target is SwitchStatement) {
        target = _target(target);
      }
      if (target == null) {
        skip(node, 'unresolved_control_target', 'No loop target exists.');
      } else {
        replace(node, ';');
        if (!insideSwitch) {
          replace(node, 'break;');
        } else {
          skip(
            node,
            'continue_inside_switch',
            'An unlabelled break would target the switch rather than the original loop.',
          );
        }
      }
    }
    super.visitContinueStatement(node);
  }
}

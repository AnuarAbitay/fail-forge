import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../common.dart';
import '../returns/replacement_values.dart';

class SwitchCaseMutation extends ClassicOperator {
  const SwitchCaseMutation();
  @override
  String get id => 'SWITCH_CASE_MUTATION';
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
  void _member(SwitchMember member, Expression? constant) {
    if (member.labels.isNotEmpty) {
      skip(
        member,
        'labelled_switch_case',
        'Cases with jump labels are unsupported.',
      );
      return;
    }
    replace(member, '');
    if (member.statements.isNotEmpty) {
      replace(
        member,
        '',
        offset: member.statements.first.offset,
        length: member.statements.last.end - member.statements.first.offset,
      );
    } else {
      skip(member, 'empty_switch_body', 'The case body is already empty.');
    }
    if (constant == null) return;
    final value = constant.unParenthesized;
    final variants = switch (value) {
      IntegerLiteral() => ['0', '1', '-1'],
      BooleanLiteral() => ['true', 'false'],
      SimpleStringLiteral() => ["''", "'mutated'"],
      _ => <String>[],
    };
    if (variants.isEmpty) {
      skip(
        constant,
        'unsupported_case_constant',
        'Case replacement supports int, bool and simple string constants only.',
      );
    }
    for (final variant in variants) {
      if (isUnchangedReturnValue(constant, variant)) {
        skip(
          constant,
          'unchanged_case_value',
          'The literal case already has the replacement value.',
        );
        continue;
      }
      replace(constant, variant);
    }
  }

  @override
  void visitSwitchCase(SwitchCase node) {
    _member(node, node.expression);
    super.visitSwitchCase(node);
  }

  @override
  void visitSwitchDefault(SwitchDefault node) {
    _member(node, null);
    super.visitSwitchDefault(node);
  }

  @override
  void visitSwitchPatternCase(SwitchPatternCase node) {
    final guarded = node.guardedPattern;
    final pattern = guarded.pattern;
    if (guarded.whenClause == null && pattern is ConstantPattern) {
      _member(node, pattern.expression);
    } else {
      skip(
        node,
        'unsupported_switch_pattern',
        'Only unguarded constant cases and default are supported.',
      );
    }
    super.visitSwitchPatternCase(node);
  }

  @override
  void visitSwitchExpression(SwitchExpression node) {
    skip(
      node,
      'unsupported_switch_expression',
      'Switch expressions require exhaustiveness-preserving replacements.',
    );
    super.visitSwitchExpression(node);
  }
}

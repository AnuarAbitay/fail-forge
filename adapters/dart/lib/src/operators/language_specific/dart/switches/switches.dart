import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../../classic/calls/call_semantics.dart' show literalIdentity;
import '../common.dart';

const switchOperators = <LanguageSpecificOperator>[
  SwitchBranchResultReplacement(),
  SwitchExpressionReplacement(),
  SwitchGuardMutation(),
];

class SwitchBranchResultReplacement extends LanguageSpecificOperator {
  const SwitchBranchResultReplacement();
  @override
  String get id => 'SWM-001';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.switches;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _BranchCollector(this, source, result);
    result.unit.accept(collector);
    return collector.result;
  }
}

class SwitchExpressionReplacement extends LanguageSpecificOperator {
  const SwitchExpressionReplacement();
  @override
  String get id => 'SWM-002';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.switches;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _ExpressionCollector(this, source, result);
    result.unit.accept(collector);
    return collector.result;
  }
}

class SwitchGuardMutation extends LanguageSpecificOperator {
  const SwitchGuardMutation();
  @override
  String get id => 'SWM-003';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.switches;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _GuardCollector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

abstract class _ResultsCollector extends OperatorCollector {
  _ResultsCollector(super.operator, super.source, this.unit);
  final ResolvedUnitResult unit;

  void exchange(AstNode owner, List<Expression> results) {
    if (results.length > 128) {
      skip(
        owner,
        'switch_branch_limit',
        'At most 128 branch results are compared.',
      );
      return;
    }
    // Donor portability depends on this switch owner, not the target arm.
    // Keep both caches local: another switch or analyzer overlay has its own
    // lexical bindings and must not inherit a previous donor's result.
    final portable = <Expression, bool>{};
    final textByExpression = <Expression, String>{};
    String textOf(Expression expression) => textByExpression.putIfAbsent(
      expression,
      () => source.substring(expression.offset, expression.end),
    );
    for (final target in results) {
      final targetType = target.staticType;
      if (!_usable(targetType)) continue;
      final seen = <String>{};
      for (final donor in results) {
        if (identical(target, donor) || _sameValue(target, donor)) continue;
        final donorType = donor.staticType;
        if (!_usable(donorType) ||
            !unit.libraryElement.typeSystem.isAssignableTo(
              donorType!,
              targetType!,
            )) {
          continue;
        }
        final safe = portable.putIfAbsent(donor, () {
          final references = _PortableReferences(owner);
          donor.accept(references);
          return references.safe;
        });
        if (!safe) {
          skip(
            donor,
            'branch_local_donor',
            'A donor must not depend on branch bindings, closures or branch-specific promotion.',
          );
          continue;
        }
        final text = textOf(donor);
        if (text == textOf(target) || !seen.add(text)) continue;
        // Result precedence must not change when it moves into a different arm.
        replace(target, '($text)');
      }
    }
  }
}

bool _usable(DartType? type) =>
    type != null &&
    type is! DynamicType &&
    type is! InvalidType &&
    type is! VoidType &&
    type is! NeverType;

bool _sameValue(Expression left, Expression right) {
  final a = left.unParenthesized, b = right.unParenthesized;
  if (a is IntegerLiteral && b is IntegerLiteral) {
    final value = literalIdentity(a);
    return value != null && value == literalIdentity(b);
  }
  if (a is DoubleLiteral && b is DoubleLiteral) return a.value == b.value;
  if (a is BooleanLiteral && b is BooleanLiteral) return a.value == b.value;
  if (a is SimpleStringLiteral && b is SimpleStringLiteral)
    return a.value == b.value;
  if (a is NullLiteral && b is NullLiteral) return true;
  return a is SimpleIdentifier &&
      b is SimpleIdentifier &&
      a.element != null &&
      a.element == b.element;
}

/// Local references are portable only when declared outside this switch and
/// used at their declared type. Branch promotion cannot be copied by spelling.
class _PortableReferences extends RecursiveAstVisitor<void> {
  _PortableReferences(this.owner);
  final AstNode owner;
  bool safe = true;
  @override
  void visitFunctionExpression(FunctionExpression node) {
    safe = false;
  }

  @override
  void visitSimpleIdentifier(SimpleIdentifier node) {
    final element = node.element;
    if (element is LocalVariableElement || element is FormalParameterElement) {
      final variable = element as VariableElement;
      final offset = variable.firstFragment.nameOffset;
      if (offset == null ||
          (offset >= owner.offset && offset < owner.end) ||
          node.staticType != variable.type)
        safe = false;
    }
    super.visitSimpleIdentifier(node);
  }
}

class _BranchCollector extends _ResultsCollector {
  _BranchCollector(super.operator, super.source, super.unit);
  @override
  void visitSwitchStatement(SwitchStatement node) {
    final results = <Expression>[];
    for (final member in node.members) {
      if (member.labels.isNotEmpty ||
          member.statements.length != 1 ||
          member.statements.single is! ReturnStatement) {
        skip(
          member,
          'unsupported_switch_result',
          'Only an unlabelled arm containing one direct return expression is a result donor.',
        );
        continue;
      }
      final value = (member.statements.single as ReturnStatement).expression;
      if (value != null) results.add(value);
    }
    exchange(node, results);
    super.visitSwitchStatement(node);
  }
}

class _ExpressionCollector extends _ResultsCollector {
  _ExpressionCollector(super.operator, super.source, super.unit);
  @override
  void visitSwitchExpression(SwitchExpression node) {
    if (node.inConstantContext) {
      skip(
        node,
        'constant_switch_expression',
        'Constant switch contexts are excluded.',
      );
    } else {
      exchange(node, node.cases.map((arm) => arm.expression).toList());
    }
    super.visitSwitchExpression(node);
  }
}

class _GuardCollector extends OperatorCollector {
  _GuardCollector(super.operator, super.source);
  @override
  void visitWhenClause(WhenClause node) {
    final owner = node.parent?.parent;
    if (owner is SwitchPatternCase || owner is SwitchExpressionCase) {
      final condition = node.expression;
      if (constantBoolGuard(condition) != null) {
        skip(
          condition,
          'constant_guard',
          'A statically known guard is excluded to preserve branch reachability.',
        );
      } else if (!hasRuntimeGuardDependency(condition)) {
        skip(
          condition,
          'runtime_independent_guard',
          'Guard inversion requires a nonconstant variable read or runtime invocation.',
        );
      } else if (isSdkBool(condition.staticType)) {
        replace(
          condition,
          '!(${source.substring(condition.offset, condition.end)})',
        );
      } else {
        skip(
          condition,
          'unsupported_guard_type',
          'Guard inversion requires a resolved nonnullable bool.',
        );
      }
    }
    super.visitWhenClause(node);
  }
}

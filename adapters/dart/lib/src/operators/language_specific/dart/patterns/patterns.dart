import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';

import '../common.dart';

const patternOperators = <LanguageSpecificOperator>[
  PatternConstraintMutation(),
  PatternGuardInversion(),
  LogicalPatternMutation(),
];

class PatternConstraintMutation extends LanguageSpecificOperator {
  const PatternConstraintMutation();
  @override
  String get id => 'PMM-001';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.patterns;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _ConstraintCollector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

class PatternGuardInversion extends LanguageSpecificOperator {
  const PatternGuardInversion();
  @override
  String get id => 'PMM-002';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.patterns;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _GuardCollector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

class LogicalPatternMutation extends LanguageSpecificOperator {
  const LogicalPatternMutation();
  @override
  String get id => 'PMM-003';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.patterns;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _LogicalCollector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _ConstraintCollector extends OperatorCollector {
  _ConstraintCollector(super.operator, super.source);
  @override
  void visitRelationalPattern(RelationalPattern node) {
    final element = node.element;
    final owner = element?.enclosingElement;
    final replacement = const {
      '>': '<=',
      '>=': '<',
      '<': '>=',
      '<=': '>',
      '==': '!=',
      '!=': '==',
    }[node.operator.lexeme];
    if (replacement != null &&
        element != null &&
        element.isOperator &&
        element.library.uri == Uri.parse('dart:core') &&
        owner is InterfaceElement &&
        const {
          'Object',
          'num',
          'int',
          'double',
          'String',
          'bool',
        }.contains(owner.name)) {
      replace(
        node,
        replacement,
        offset: node.operator.offset,
        length: node.operator.length,
      );
    } else {
      skip(
        node,
        'unsupported_pattern_operator',
        'Relational constraints require a resolved SDK operator; custom overloads are excluded.',
      );
    }
    super.visitRelationalPattern(node);
  }
}

class _GuardCollector extends OperatorCollector {
  _GuardCollector(super.operator, super.source);
  @override
  void visitWhenClause(WhenClause node) {
    // Switch guards belong to SWM-003. PMM-002 owns if-case guards, including
    // collection if-case elements, without changing their bound declarations.
    if (node.parent?.parent is CaseClause) {
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

class _Bindings extends GeneralizingAstVisitor<void> {
  bool found = false;
  @override
  void visitNode(AstNode node) {
    if (node is VariablePattern) found = true;
    super.visitNode(node);
  }
}

class _LogicalCollector extends OperatorCollector {
  _LogicalCollector(super.operator, super.source);
  void mutate(
    DartPattern node,
    DartPattern left,
    DartPattern right,
    int tokenOffset,
    int tokenEnd,
    String replacement,
  ) {
    final bindings = _Bindings();
    node.accept(bindings);
    if (bindings.found) {
      skip(
        node,
        'logical_pattern_bindings',
        'Switching logical operators would introduce duplicate or missing variable bindings.',
      );
      return;
    }
    if (node.matchedValueType == null) {
      skip(
        node,
        'unresolved_pattern',
        'A resolved matched-value type is required.',
      );
      return;
    }
    final before = source.substring(left.end, tokenOffset);
    final after = source.substring(tokenEnd, right.offset);
    replace(
      node,
      '((${source.substring(left.offset, left.end)})$before$replacement$after'
      '(${source.substring(right.offset, right.end)}))',
    );
  }

  @override
  void visitLogicalAndPattern(LogicalAndPattern node) {
    mutate(
      node,
      node.leftOperand,
      node.rightOperand,
      node.operator.offset,
      node.operator.end,
      '||',
    );
    super.visitLogicalAndPattern(node);
  }

  @override
  void visitLogicalOrPattern(LogicalOrPattern node) {
    mutate(
      node,
      node.leftOperand,
      node.rightOperand,
      node.operator.offset,
      node.operator.end,
      '&&',
    );
    super.visitLogicalOrPattern(node);
  }
}

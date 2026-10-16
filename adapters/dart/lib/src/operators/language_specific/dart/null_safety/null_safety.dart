import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:analyzer/dart/element/type.dart';

import '../common.dart';

const List<LanguageSpecificOperator> nullSafetyOperators = [
  NullCoalescingFallbackRemoval(),
  NullAwareAccessReplacement(),
  NullableValueReplacement(),
  NullAwareAssignmentMutation(),
];

abstract class _NullSafetyOperator extends LanguageSpecificOperator {
  const _NullSafetyOperator();

  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.nullSafety;

  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source, result);
    result.unit.accept(collector);
    return collector.result;
  }
}

/// Remove only the fallback and its separator, retaining the receiver once.
/// The publication pipeline checks the surrounding assignment/return context.
class NullCoalescingFallbackRemoval extends _NullSafetyOperator {
  const NullCoalescingFallbackRemoval();
  @override
  String get id => 'NSM-001';
}

/// Remove a null-aware token without inserting a non-null assertion.
class NullAwareAccessReplacement extends _NullSafetyOperator {
  const NullAwareAccessReplacement();
  @override
  String get id => 'NSM-002';
}

/// Replace initial values and ordinary assignment values in nullable storage.
class NullableValueReplacement extends _NullSafetyOperator {
  const NullableValueReplacement();
  @override
  String get id => 'NSM-003';
}

class NullAwareAssignmentMutation extends _NullSafetyOperator {
  const NullAwareAssignmentMutation();
  @override
  String get id => 'NSM-004';
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source, this.unit);
  final ResolvedUnitResult unit;

  bool _allowsNull(DartType? type) =>
      type != null &&
      type is! InvalidType &&
      type is! VoidType &&
      unit.typeSystem.isSubtypeOf(unit.typeProvider.nullType, type);

  bool _alreadyNull(Expression expression) {
    final value = expression.unParenthesized;
    if (value is NullLiteral || value.staticType?.isDartCoreNull == true) {
      return true;
    }
    final element = switch (value) {
      SimpleIdentifier() => value.element,
      PrefixedIdentifier() => value.element,
      PropertyAccess() => value.propertyName.element,
      _ => null,
    };
    final variable = element is PropertyAccessorElement
        ? element.variable
        : element;
    return variable is VariableElement &&
        variable.isConst &&
        variable.computeConstantValue()?.isNull == true;
  }

  @override
  void visitBinaryExpression(BinaryExpression node) {
    if (operator.id == 'NSM-001' && node.operator.lexeme == '??') {
      final left = node.leftOperand;
      final type = left.staticType;
      if (type != null &&
          type is! InvalidType &&
          type is! VoidType &&
          unit.typeSystem.isPotentiallyNullable(type)) {
        replace(node, '', offset: left.end, length: node.end - left.end);
      } else {
        skip(
          node,
          'nonnullable_coalescing_receiver',
          'Removing a fallback on a non-nullable receiver is redundant.',
        );
      }
    }
    super.visitBinaryExpression(node);
  }

  // Dynamic receivers preserve static validity and can observe the removal at
  // runtime. Ordinary nullable interface receivers would require an implicit
  // `!`, which is a different operator; non-nullable receivers are redundant.
  bool _supportsAccess(Expression node, Expression? receiver, bool cascaded) {
    if (cascaded || receiver == null) {
      skip(node, 'unsupported_null_aware_receiver', 'Cascades are excluded.');
      return false;
    }
    if (receiver.staticType is DynamicType) return true;
    skip(
      node,
      'unsupported_null_aware_receiver',
      'Access removal requires a dynamic receiver; no assertion is inserted.',
    );
    return false;
  }

  @override
  void visitPropertyAccess(PropertyAccess node) {
    if (operator.id == 'NSM-002' &&
        node.isNullAware &&
        _supportsAccess(node, node.target, node.isCascaded)) {
      replace(
        node,
        '.',
        offset: node.operator.offset,
        length: node.operator.length,
      );
    }
    super.visitPropertyAccess(node);
  }

  @override
  void visitMethodInvocation(MethodInvocation node) {
    final token = node.operator;
    if (operator.id == 'NSM-002' &&
        node.isNullAware &&
        token != null &&
        _supportsAccess(node, node.target, node.isCascaded)) {
      replace(node, '.', offset: token.offset, length: token.length);
    }
    super.visitMethodInvocation(node);
  }

  @override
  void visitIndexExpression(IndexExpression node) {
    final token = node.question;
    if (operator.id == 'NSM-002' &&
        node.isNullAware &&
        token != null &&
        _supportsAccess(node, node.target, node.isCascaded)) {
      replace(node, '', offset: token.offset, length: token.length);
    }
    super.visitIndexExpression(node);
  }

  @override
  void visitVariableDeclaration(VariableDeclaration node) {
    final initial = node.initializer;
    if (operator.id == 'NSM-003' &&
        initial != null &&
        !_alreadyNull(initial) &&
        _allowsNull(node.declaredFragment?.element.type)) {
      replace(initial, 'null');
    }
    super.visitVariableDeclaration(node);
  }

  @override
  void visitAssignmentExpression(AssignmentExpression node) {
    if (operator.id == 'NSM-003' &&
        node.operator.lexeme == '=' &&
        !_alreadyNull(node.rightHandSide) &&
        _allowsNull(node.writeType)) {
      replace(node.rightHandSide, 'null');
    }
    if (operator.id == 'NSM-004' && node.operator.lexeme == '??=') {
      final valueType = node.rightHandSide.staticType;
      final targetType = node.writeType;
      if (valueType != null &&
          valueType is! InvalidType &&
          targetType != null &&
          targetType is! InvalidType &&
          unit.typeSystem.isAssignableTo(valueType, targetType)) {
        replace(
          node,
          '=',
          offset: node.operator.offset,
          length: node.operator.length,
        );
      }
    }
    super.visitAssignmentExpression(node);
  }
}

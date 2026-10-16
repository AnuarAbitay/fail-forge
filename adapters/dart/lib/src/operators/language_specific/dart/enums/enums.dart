import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/element.dart';

import '../../../classic/returns/replacement_values.dart';
import '../common.dart';

const enumOperators = <LanguageSpecificOperator>[EnumValueReplacement()];

class EnumValueReplacement extends LanguageSpecificOperator {
  const EnumValueReplacement();
  @override
  String get id => 'ENM-001';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.enums;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source, result);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source, this.unit);
  final ResolvedUnitResult unit;
  final bindings = ValueTypeBindings();

  @override
  void visitSimpleIdentifier(SimpleIdentifier node) {
    final element = node.element;
    final field = element is PropertyAccessorElement
        ? element.variable
        : element;
    if (field is FieldElement &&
        field.isEnumConstant &&
        !node.inDeclarationContext()) {
      _constant(node, field);
    }
    super.visitSimpleIdentifier(node);
  }

  void _constant(SimpleIdentifier node, FieldElement field) {
    final parent = node.parent;
    final Expression expression = switch (parent) {
      PrefixedIdentifier() when parent.identifier == node => parent,
      PropertyAccess() when parent.propertyName == node => parent,
      DotShorthandPropertyAccess() when parent.propertyName == node => parent,
      _ => node,
    };
    for (
      AstNode? current = expression;
      current != null;
      current = current.parent
    ) {
      if (current is EnumConstantDeclaration ||
          current is DartPattern ||
          current is Annotation) {
        skip(
          expression,
          'enum_declaration_or_pattern',
          'Enum declarations, metadata and patterns are not value use sites.',
        );
        return;
      }
    }
    final owner = field.enclosingElement;
    final originalType = expression.staticType;
    if (owner is! EnumElement || originalType == null) return;
    for (final donor in owner.fields.where((value) => value.isEnumConstant)) {
      final name = donor.name;
      if (donor.baseElement == field.baseElement ||
          name == null ||
          !donor.isAccessibleIn(unit.libraryElement) ||
          !unit.typeSystem.isAssignableTo(donor.type, originalType)) {
        continue;
      }
      if (identical(expression, node) && bindings.shadowed(node, name)) {
        skip(
          expression,
          'shadowed_enum_value',
          'An unqualified replacement would bind a local declaration.',
        );
        continue;
      }
      // Changing only the resolved constant name preserves import prefixes,
      // enum qualifiers and surrounding comments, including dot shorthand.
      replace(expression, name, offset: node.offset, length: node.length);
    }
  }
}

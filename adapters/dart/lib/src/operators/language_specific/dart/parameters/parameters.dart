import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/constant/value.dart';
import 'package:analyzer/dart/element/type.dart';

import '../common.dart';

const List<LanguageSpecificOperator> parameterOperators = [
  DefaultParameterReplacement(),
  BooleanDefaultInversion(),
  NullableDefaultReplacement(),
];

abstract class _ParameterOperator extends LanguageSpecificOperator {
  const _ParameterOperator();

  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.parameters;

  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source, result);
    result.unit.accept(collector);
    return collector.result;
  }
}

/// Defaults stay compile-time constants compatible with the resolved parameter.
/// Boolean inversion and null replacement have their own catalogue operators.
class DefaultParameterReplacement extends _ParameterOperator {
  const DefaultParameterReplacement();
  @override
  String get id => 'DPM-001';
}

class BooleanDefaultInversion extends _ParameterOperator {
  const BooleanDefaultInversion();
  @override
  String get id => 'DPM-002';
}

class NullableDefaultReplacement extends _ParameterOperator {
  const NullableDefaultReplacement();
  @override
  String get id => 'DPM-003';
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source, this.unit);
  final ResolvedUnitResult unit;

  @override
  void visitFormalParameterDefaultClause(FormalParameterDefaultClause node) {
    final parent = node.parent;
    if (parent is FormalParameter) {
      final element = parent.declaredFragment?.element;
      final type = element?.type;
      final value = element?.computeConstantValue();
      if (type == null ||
          type is InvalidType ||
          type is VoidType ||
          type is NeverType ||
          value == null) {
        skip(
          node,
          'unresolved_parameter_default',
          'A resolved parameter type and constant default are required.',
        );
      } else {
        _emitDefaults(node.value, type, value);
      }
    }
    super.visitFormalParameterDefaultClause(node);
  }

  void _emitDefaults(Expression expression, DartType type, DartObject value) {
    final provider = unit.typeProvider;
    switch (operator.id) {
      case 'DPM-001':
        final alternatives = <(String, DartType)>[
          ('0', provider.intType),
          ('1', provider.intType),
          ('-1', provider.intType),
          ('0.0', provider.doubleType),
          ('1.0', provider.doubleType),
          ('-1.0', provider.doubleType),
          ("''", provider.stringType),
          ("'mutated'", provider.stringType),
        ];
        for (final (text, candidateType) in alternatives) {
          if (unit.typeSystem.isAssignableTo(candidateType, type) &&
              !_sameConstant(value, text)) {
            replace(expression, text);
          }
        }
      case 'DPM-002':
        final boolean = value.toBoolValue();
        if (boolean != null &&
            unit.typeSystem.isAssignableTo(provider.boolType, type)) {
          replace(expression, boolean ? 'false' : 'true');
        }
      case 'DPM-003':
        if (!value.isNull &&
            unit.typeSystem.isSubtypeOf(provider.nullType, type)) {
          replace(expression, 'null');
        }
    }
  }
}

bool _sameConstant(DartObject value, String replacement) {
  if (!value.hasKnownValue) return false;
  if (replacement == "''") return value.toStringValue() == '';
  if (replacement == "'mutated'") return value.toStringValue() == 'mutated';
  if (replacement.contains('.')) {
    final number = value.toDoubleValue();
    final target = double.parse(replacement);
    return number != null &&
        number == target &&
        (number != 0 || number.isNegative == target.isNegative);
  }
  final integer = value.toIntValue();
  return integer != null && integer.toString() == replacement;
}

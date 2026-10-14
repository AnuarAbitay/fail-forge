import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/element.dart';

import '../../common.dart';
import '../algebra_helpers.dart';
import 'lexical_variables.dart';

class VariableReplacement extends ClassicOperator {
  const VariableReplacement();
  @override
  String get id => 'VARIABLE_REPLACEMENT';
  @override
  ClassicalCategory get category => ClassicalCategory.variables;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final scope = LexicalScope();
    return scanAlgebra(this, result, source, (node, collector) {
      if (node is! SimpleIdentifier || !isValueExpression(node)) return;
      final element = node.element;
      if ((element is! LocalVariableElement &&
              element is! FormalParameterElement) ||
          element is! VariableElement ||
          element.isLate ||
          node.staticType == null ||
          !collector.acceptsContext(node))
        return;
      final types = result.libraryElement.typeSystem;
      for (final variable in scope.variablesAt(node)) {
        if (!identical(variable.element, element) &&
            replacementFits(types, variable.type, node.staticType!)) {
          collector.emit(node, variable.name);
        }
      }
    });
  }
}

import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../../classic/returns/replacement_values.dart';
import '../../../failure_modes/symbols.dart';
import '../common.dart';

const collectionOperators = <LanguageSpecificOperator>[
  CollectionIfInversion(),
  CollectionSpreadRemoval(),
  CollectionIfElementRemoval(),
  CollectionForRemoval(),
];

class CollectionIfInversion extends LanguageSpecificOperator {
  const CollectionIfInversion();
  @override
  String get id => 'CLM-001';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.collections;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _IfInversionCollector(this, source);
    result.unit.accept(collector);
    return collector.result;
  }
}

class _IfInversionCollector extends OperatorCollector {
  _IfInversionCollector(super.operator, super.source);

  @override
  void visitIfElement(IfElement node) {
    if (node.caseClause != null) {
      skip(
        node,
        'pattern_condition',
        'Collection-if patterns are not boolean conditions.',
      );
    } else if (!isSdkBool(node.expression.staticType)) {
      skip(
        node,
        'unsupported_condition_type',
        'The condition must resolve to non-nullable SDK bool.',
      );
    } else {
      replace(
        node.expression,
        '!(${source.substring(node.expression.offset, node.expression.end)})',
      );
    }
    super.visitIfElement(node);
  }
}

class CollectionSpreadRemoval extends LanguageSpecificOperator {
  const CollectionSpreadRemoval();
  @override
  String get id => 'CLM-002';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.collections;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      _removals(this, result, source, _Removal.spread);
}

class CollectionIfElementRemoval extends LanguageSpecificOperator {
  const CollectionIfElementRemoval();
  @override
  String get id => 'CLM-003';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.collections;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      _removals(this, result, source, _Removal.conditional);
}

class CollectionForRemoval extends LanguageSpecificOperator {
  const CollectionForRemoval();
  @override
  String get id => 'CLM-004';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.collections;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) =>
      _removals(this, result, source, _Removal.loop);
}

enum _Removal { spread, conditional, loop }

OperatorScan _removals(
  LanguageSpecificOperator operator,
  ResolvedUnitResult result,
  String source,
  _Removal kind,
) {
  final collector = _RemovalCollector(
    operator,
    source,
    SdkSymbols(result),
    kind,
  );
  result.unit.accept(collector);
  return collector.result;
}

class _RemovalCollector extends OperatorCollector {
  _RemovalCollector(super.operator, super.source, this.symbols, this.kind);
  final SdkSymbols symbols;
  final _Removal kind;
  final bindings = ValueTypeBindings();

  @override
  void visitSpreadElement(SpreadElement node) {
    if (kind == _Removal.spread && !node.isNullAware) {
      removeCollectionElement(this, node, symbols, bindings);
    }
    super.visitSpreadElement(node);
  }

  @override
  void visitIfElement(IfElement node) {
    if (kind == _Removal.conditional) {
      removeCollectionElement(this, node, symbols, bindings);
    }
    super.visitIfElement(node);
  }

  @override
  void visitForElement(ForElement node) {
    if (kind == _Removal.loop) {
      removeCollectionElement(this, node, symbols, bindings);
    }
    super.visitForElement(node);
  }
}

/// Removes one direct literal element. A trailing comma can remain legal; a
/// following comma belongs to this edit, while comments around it are retained.
/// Untyped set/map literals receive their original resolved type arguments so
/// deleting the element cannot turn an empty set into an empty map.
void removeCollectionElement(
  OperatorCollector collector,
  CollectionElement node,
  SdkSymbols symbols,
  ValueTypeBindings bindings,
) {
  final parent = node.parent;
  if (parent is! ListLiteral && parent is! SetOrMapLiteral) {
    collector.skip(
      node,
      'nested_collection_element',
      'Removing the sole body of another if/for element would break its grammar.',
    );
    return;
  }
  final literal = parent as TypedLiteral;
  final type = literal.staticType;
  if (type is! InterfaceType ||
      type.element.library.uri != Uri.parse('dart:core') ||
      !{'List', 'Set', 'Map'}.contains(type.element.name)) {
    collector.skip(
      node,
      'unsupported_collection_type',
      'The enclosing literal must resolve to an SDK collection.',
    );
    return;
  }
  var start = node.offset;
  var replacement = '';
  final next = node.endToken.next;
  var end = node.end;
  if (next != null && next.lexeme == ',') {
    replacement = collector.source.substring(node.end, next.offset);
    end = next.end;
  }
  if (parent is SetOrMapLiteral && parent.typeArguments == null) {
    final arguments = <String>[];
    for (final argument in type.typeArguments) {
      final text = visibleType(argument, symbols, node, bindings);
      if (text == null) {
        collector.skip(
          node,
          'collection_type_unavailable',
          'The inferred collection types cannot be named safely here.',
        );
        return;
      }
      arguments.add(text);
    }
    start = parent.leftBracket.offset;
    replacement =
        '<${arguments.join(', ')}>' +
        collector.source.substring(start, node.offset) +
        replacement;
  }
  collector.replace(node, replacement, offset: start, length: end - start);
}

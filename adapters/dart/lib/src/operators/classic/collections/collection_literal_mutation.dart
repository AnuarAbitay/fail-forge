import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/element/type.dart';

import '../../common.dart';
import '../../failure_modes/symbols.dart';
import '../returns/replacement_values.dart';

class CollectionLiteralMutation extends ClassicOperator {
  const CollectionLiteralMutation();
  @override
  String get id => 'COLLECTION_LITERAL_MUTATION';
  @override
  ClassicalCategory get category => ClassicalCategory.collections;
  @override
  OperatorScan discover(ResolvedUnitResult result, String source) {
    final collector = _Collector(this, source, SdkSymbols(result));
    result.unit.accept(collector);
    return collector.result;
  }
}

class _Collector extends OperatorCollector {
  _Collector(super.operator, super.source, this.symbols);
  final SdkSymbols symbols;
  final typeBindings = ValueTypeBindings();
  void _literal(TypedLiteral node, List<CollectionElement> elements) {
    if (!acceptsContext(node)) return;
    final type = node.staticType;
    if (type is! InterfaceType ||
        type.element.library.uri != Uri.parse('dart:core') ||
        !{'List', 'Set', 'Map'}.contains(type.element.name)) {
      skip(
        node,
        'unsupported_collection_type',
        'The literal must resolve to an SDK collection.',
      );
      return;
    }
    if (elements.any(
      (element) => element is! Expression && element is! MapLiteralEntry,
    )) {
      skip(
        node,
        'unsupported_collection_element',
        'Spread, if and for elements are unsupported.',
      );
      return;
    }
    if (elements.isEmpty) {
      skip(node, 'empty_collection', 'The collection is already empty.');
      return;
    }
    final empty = typedEmptyCollection(type, symbols, node, typeBindings);
    if (empty == null) {
      skip(
        node,
        'collection_type_unavailable',
        'The original element types cannot be named safely at this site.',
      );
      return;
    }
    replace(node, empty);
    if (elements.length == 1) return;
    for (var index = 0; index < elements.length; index++) {
      final start = index == elements.length - 1
          ? elements[index - 1].end
          : elements[index].offset;
      final end = index == elements.length - 1
          ? elements[index].end
          : elements[index + 1].offset;
      final original = source.substring(node.offset, node.end);
      replace(
        node,
        original.replaceRange(start - node.offset, end - node.offset, ''),
      );
    }
  }

  @override
  void visitListLiteral(ListLiteral node) {
    _literal(node, node.elements.toList());
    super.visitListLiteral(node);
  }

  @override
  void visitSetOrMapLiteral(SetOrMapLiteral node) {
    _literal(node, node.elements.toList());
    super.visitSetOrMapLiteral(node);
  }
}

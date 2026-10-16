import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../../../classic/returns/replacement_values.dart';
import '../../../failure_modes/symbols.dart';
import '../collections/collections.dart';
import '../common.dart';

const nullAwareSpreadOperators = <LanguageSpecificOperator>[
  NullAwareSpreadRemoval(),
];

class NullAwareSpreadRemoval extends LanguageSpecificOperator {
  const NullAwareSpreadRemoval();
  @override
  String get id => 'NSM-005';
  @override
  LanguageSpecificCategory get category => LanguageSpecificCategory.nullSafety;
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
  final bindings = ValueTypeBindings();

  @override
  void visitSpreadElement(SpreadElement node) {
    if (node.isNullAware) {
      removeCollectionElement(this, node, symbols, bindings);
    }
    super.visitSpreadElement(node);
  }
}

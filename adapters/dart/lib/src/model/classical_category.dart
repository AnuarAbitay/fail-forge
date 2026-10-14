import '../protocol/adapter_error.dart';

/// Categories describe operators within the classic family. They are catalogue
/// metadata; a mutant's canonical identity continues to use its operator ID.
enum ClassicalCategory {
  relational('relational', 'Relational'),
  logical('logical', 'Logical'),
  arithmetic('arithmetic', 'Arithmetic'),
  boolean('boolean', 'Boolean'),
  constants('constants', 'Constants'),
  returns('return', 'Return'),
  controlFlow('control_flow', 'ControlFlow'),
  calls('calls', 'Calls'),
  variables('variables', 'Variables'),
  bitwise('bitwise', 'Bitwise'),
  exceptions('exceptions', 'Exceptions'),
  collections('collections', 'Collections');

  const ClassicalCategory(this.wireName, this.displayName);
  final String wireName;
  final String displayName;

  static ClassicalCategory fromWire(Object? value) {
    for (final category in values) {
      if (category.wireName == value) return category;
    }
    throw AdapterException('invalid_category', 'Unknown classical category.');
  }
}

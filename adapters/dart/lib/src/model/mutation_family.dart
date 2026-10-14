import '../protocol/adapter_error.dart';

/// A mutation's semantic category. Wire names are stable protocol values and
/// participate in canonical mutant IDs independently of Dart enum names.
enum MutationFamily {
  classic('classic'),
  failureMode('failure_mode');

  const MutationFamily(this.wireName);

  final String wireName;

  static MutationFamily fromWire(Object? value) => switch (value) {
    'classic' => classic,
    'failure_mode' => failureMode,
    _ => throw AdapterException(
      'invalid_family',
      'family must be classic or failure_mode.',
    ),
  };
}

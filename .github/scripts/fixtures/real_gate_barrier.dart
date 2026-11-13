import 'dart:async';
import 'dart:convert';
import 'dart:io';

const gateBarrierMarkerKind = 'failforge_gate_barrier_observed';

/// Runs the actual test assertion only after the false mutant observes its
/// quality-stop proof. A journal read failure must never substitute for it.
Future<void> assertEnabledAfterGate({
  required bool value,
  required void Function(bool) assertion,
  required void Function(Map<String, Object?>) emitMarker,
  String? journal,
  String? nonce,
  Duration timeout = const Duration(seconds: 90),
  Duration pollInterval = const Duration(milliseconds: 10),
  bool? windows,
  String Function(String)? readJournal,
  Duration Function()? elapsed,
  Future<void> Function(Duration)? pause,
}) async {
  if (value) {
    assertion(value);
    return;
  }
  if (journal == null || journal.isEmpty || nonce == null || nonce.isEmpty) {
    throw ArgumentError('A false mutant requires a journal and barrier nonce');
  }
  if (timeout <= Duration.zero || pollInterval <= Duration.zero) {
    throw ArgumentError(
      'Barrier timeout and polling interval must be positive',
    );
  }
  final watch = Stopwatch()..start();
  final clock = elapsed ?? () => watch.elapsed;
  final delay = pause ?? Future<void>.delayed;
  final read = readJournal ?? (path) => File(path).readAsStringSync();
  final onWindows = windows ?? Platform.isWindows;
  final deadline = clock() + timeout;
  FileSystemException? lastLockError;
  while (true) {
    if (clock() >= deadline) {
      if (lastLockError != null) throw lastLockError;
      throw StateError('Expected gate proof was not published');
    }
    String? text;
    try {
      text = read(journal);
      lastLockError = null;
    } on PathNotFoundException catch (error) {
      // The fresh output journal may not have been created yet.
      final code = error.osError?.errorCode;
      if (code != 2 && !(onWindows && code == 3)) rethrow;
      lastLockError = null;
    } on FileSystemException catch (error) {
      final code = error.osError?.errorCode;
      if (!onWindows || (code != 32 && code != 33)) rethrow;
      // A concurrent Windows writer can temporarily deny sharing or hold a
      // byte-range lock. Access-denied and other IO errors remain failures.
      lastLockError = error;
    }
    if (text != null) {
      final proof = _proofFromCompleteLines(text);
      if (proof != null) {
        // Synchronous IO cannot be interrupted, but a late successful read
        // still must not turn an expired barrier into a positive observation.
        if (clock() >= deadline) {
          throw StateError('Expected gate proof was not published');
        }
        emitMarker({
          'kind': gateBarrierMarkerKind,
          'nonce': nonce,
          'family': proof['family'],
          'runId': proof['runId'],
          'sequence': proof['sequence'],
          'killed': proof['killed'],
          'survived': proof['survived'],
          'remaining': proof['remaining'],
          'minScore': proof['minScore'],
          'maxPossiblePercent': proof['maxPossiblePercent'],
        });
        assertion(value);
        return;
      }
    }
    final remaining = deadline - clock();
    if (remaining > Duration.zero) {
      await delay(remaining < pollInterval ? remaining : pollInterval);
    }
  }
}

Map<String, dynamic>? _proofFromCompleteLines(String text) {
  final lines = text.split('\n');
  // The final segment is uncommitted until the journal's trailing newline.
  Map<String, dynamic>? proof;
  for (final line in lines.take(lines.length - 1)) {
    if (line.trim().isEmpty) continue;
    final decoded = jsonDecode(line);
    if (decoded is! Map<String, dynamic>) {
      throw FormatException('Expected a JSON journal event');
    }
    if (decoded['kind'] != 'quality_gate_stopped') continue;
    if (proof != null) throw StateError('Duplicate quality-stop proof');
    if (decoded['version'] is! int ||
        decoded['version'] != 1 ||
        decoded['family'] != 'classic' ||
        decoded['runId'] is! String ||
        !RegExp(r'^[0-9a-f]{32}$').hasMatch(decoded['runId'] as String) ||
        decoded['sequence'] is! int ||
        (decoded['sequence'] as int) <= 0 ||
        decoded['killed'] is! int ||
        decoded['killed'] != 0 ||
        decoded['survived'] is! int ||
        decoded['survived'] != 1 ||
        decoded['remaining'] is! int ||
        decoded['remaining'] != 1 ||
        decoded['minScore'] != 80 ||
        decoded['maxPossiblePercent'] != 50) {
      throw StateError('Unexpected quality-stop proof for the two-mutant case');
    }
    proof = decoded;
  }
  return proof;
}

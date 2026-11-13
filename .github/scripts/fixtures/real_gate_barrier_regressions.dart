import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'real_gate_barrier.dart';
import 'real_gate_journal_reader.dart';
import 'real_gate_journal_reader_windows_regression.dart';

class ExpectedAssertion implements Exception {}

void check(bool condition, String message) {
  if (!condition) throw StateError(message);
}

Map<String, Object?> event() => {
  'version': 1,
  'kind': 'quality_gate_stopped',
  'family': 'classic',
  'runId': 'a' * 32,
  'sequence': 15,
  'killed': 0,
  'survived': 1,
  'remaining': 1,
  'minScore': 80.0,
  'maxPossiblePercent': 50.0,
};

String completeEvent() => '${jsonEncode(event())}\n';

class Probe {
  Duration time = Duration.zero;
  int reads = 0;
  final order = <String>[];
  final markers = <Map<String, Object?>>[];

  Future<void> invoke(
    String Function() read, {
    bool windows = true,
    bool value = false,
    String? nonce = 'regression-nonce',
  }) => assertEnabledAfterGate(
    value: value,
    journal: 'unused-by-injected-reader',
    nonce: nonce,
    windows: windows,
    timeout: const Duration(milliseconds: 3),
    pollInterval: const Duration(milliseconds: 1),
    elapsed: () => time,
    pause: (duration) async {
      time += duration;
    },
    readJournal: (_) {
      reads += 1;
      return read();
    },
    emitMarker: (marker) {
      order.add('marker');
      markers.add(marker);
    },
    assertion: (value) {
      order.add('assert:$value');
      if (!value) throw ExpectedAssertion();
    },
  );

  void noFalseSuccess() {
    check(markers.isEmpty, 'Failure emitted a proof marker');
    check(order.isEmpty, 'Failure reached the mutation assertion');
  }
}

Future<Object> failure(Future<void> Function() body) async {
  try {
    await body();
  } catch (error) {
    return error;
  }
  throw StateError('Expected the operation to fail');
}

Future<void> main() async {
  var passed = 0;
  final failures = <Map<String, String>>[];
  Future<void> run(String name, Future<void> Function() test) async {
    try {
      await test();
      passed += 1;
      print(jsonEncode({'test': name, 'status': 'passed'}));
    } catch (error, stack) {
      failures.add({'test': name, 'error': '$error', 'stack': '$stack'});
      print(jsonEncode({'test': name, 'status': 'failed', 'error': '$error'}));
    }
  }

  await run('original-and-survivor-assert-without-barrier', () async {
    final probe = Probe();
    await probe.invoke(
      () => throw StateError('Reader must not run'),
      value: true,
    );
    check(probe.reads == 0, 'True value read the barrier');
    check(probe.markers.isEmpty, 'True value emitted a barrier marker');
    check(probe.order.single == 'assert:true', 'True assertion did not run');
  });

  for (final code in [32, 33]) {
    await run(
      'windows-lock-$code-recovers-before-intended-assertion',
      () async {
        final probe = Probe();
        final lock = FileSystemException(
          'injected lock',
          'journal',
          OSError('lock', code),
        );
        final error = await failure(
          () => probe.invoke(() {
            if (probe.reads == 1) throw lock;
            return completeEvent();
          }),
        );
        check(
          error is ExpectedAssertion,
          'Lock error replaced intended assertion: $error',
        );
        check(probe.reads == 2, 'Transient lock did not retry exactly once');
        check(
          probe.time == const Duration(milliseconds: 1),
          'Unexpected retry interval',
        );
        check(
          probe.order.join(',') == 'marker,assert:false',
          'Marker/assertion order changed',
        );
        final marker = probe.markers.single;
        check(marker['kind'] == gateBarrierMarkerKind, 'Wrong marker kind');
        check(marker['nonce'] == 'regression-nonce', 'Wrong marker nonce');
        for (final key in [
          'family',
          'runId',
          'sequence',
          'killed',
          'survived',
          'remaining',
          'minScore',
          'maxPossiblePercent',
        ]) {
          check(
            marker[key] == event()[key],
            'Marker did not bind journal $key',
          );
        }
      },
    );

    await run(
      'persistent-windows-lock-$code-reaches-original-deadline',
      () async {
        final probe = Probe();
        final lock = FileSystemException(
          'persistent lock',
          'journal',
          OSError('lock', code),
        );
        final error = await failure(() => probe.invoke(() => throw lock));
        check(identical(error, lock), 'Persistent lock was hidden or replaced');
        check(
          probe.time == const Duration(milliseconds: 3),
          'Persistent lock exceeded its bound',
        );
        check(probe.reads == 3, 'Persistent lock retry count was unbounded');
        probe.noFalseSuccess();
      },
    );
  }

  for (final code in [5, 13]) {
    await run('windows-non-lock-error-$code-is-not-retried', () async {
      final probe = Probe();
      final denied = FileSystemException(
        'unexpected IO',
        'journal',
        OSError('denied', code),
      );
      final error = await failure(() => probe.invoke(() => throw denied));
      check(identical(error, denied), 'Unexpected IO was hidden');
      check(
        probe.reads == 1 && probe.time == Duration.zero,
        'Unexpected IO was retried',
      );
      probe.noFalseSuccess();
    });
  }

  await run('linux-error-32-is-not-treated-as-windows-sharing', () async {
    final probe = Probe();
    final error32 = FileSystemException(
      'other OS error',
      'journal',
      OSError('other', 32),
    );
    final error = await failure(
      () => probe.invoke(() => throw error32, windows: false),
    );
    check(identical(error, error32), 'Linux error was hidden');
    check(
      probe.reads == 1 && probe.time == Duration.zero,
      'Linux error was retried',
    );
    probe.noFalseSuccess();
  });

  await run('missing-journal-is-bounded-and-never-signals-proof', () async {
    final probe = Probe();
    final error = await failure(
      () => probe.invoke(
        () => throw PathNotFoundException('journal', OSError('missing', 2)),
      ),
    );
    check(
      error is StateError &&
          '$error'.contains('Expected gate proof was not published'),
      'Missing proof did not time out',
    );
    check(
      probe.time == const Duration(milliseconds: 3),
      'Missing journal exceeded deadline',
    );
    probe.noFalseSuccess();
  });

  await run('invalid-windows-path-is-not-a-missing-journal-retry', () async {
    final probe = Probe();
    final invalid = PathNotFoundException(
      'journal',
      OSError('invalid name', 123),
    );
    final error = await failure(() => probe.invoke(() => throw invalid));
    check(identical(error, invalid), 'Invalid path error was hidden');
    check(
      probe.reads == 1 && probe.time == Duration.zero,
      'Invalid path was retried',
    );
    probe.noFalseSuccess();
  });

  await run('partial-record-waits-for-committed-newline', () async {
    final probe = Probe();
    final error = await failure(
      () => probe.invoke(
        () => probe.reads == 1 ? jsonEncode(event()) : completeEvent(),
      ),
    );
    check(
      error is ExpectedAssertion,
      'Committed event did not reach intended assertion',
    );
    check(
      probe.reads == 2 && probe.markers.length == 1,
      'Partial record was accepted early',
    );
  });

  await run(
    'slow-successful-read-after-deadline-cannot-signal-proof',
    () async {
      final probe = Probe();
      final error = await failure(
        () => probe.invoke(() {
          probe.time = const Duration(milliseconds: 4);
          return completeEvent();
        }),
      );
      check(
        error is StateError,
        'Expired proof reached the intended assertion',
      );
      probe.noFalseSuccess();
    },
  );

  await run('unterminated-record-never-signals-proof', () async {
    final probe = Probe();
    final error = await failure(() => probe.invoke(() => jsonEncode(event())));
    check(error is StateError, 'Uncommitted proof did not time out');
    probe.noFalseSuccess();
  });

  await run('substring-lookalike-is-not-a-journal-proof', () async {
    final probe = Probe();
    final text =
        '${jsonEncode({'kind': 'note', 'text': '"kind":"quality_gate_stopped"'})}\n';
    final error = await failure(() => probe.invoke(() => text));
    check(error is StateError, 'Lookalike did not time out');
    probe.noFalseSuccess();
  });

  await run('malformed-committed-json-fails-without-marker', () async {
    final probe = Probe();
    final error = await failure(() => probe.invoke(() => '{"kind": broken}\n'));
    check(error is FormatException, 'Malformed record was ignored');
    check(probe.reads == 1, 'Malformed committed record was retried');
    probe.noFalseSuccess();
  });

  await run('wrong-active-proof-fails-without-marker', () async {
    final probe = Probe();
    final wrong = event()..['remaining'] = 0;
    final error = await failure(
      () => probe.invoke(() => '${jsonEncode(wrong)}\n'),
    );
    check(error is StateError, 'Wrong active proof was accepted');
    probe.noFalseSuccess();
  });

  await run('noninteger-proof-count-cannot-produce-marker', () async {
    final probe = Probe();
    final wrong = event()..['killed'] = 0.0;
    final error = await failure(
      () => probe.invoke(() => '${jsonEncode(wrong)}\n'),
    );
    check(error is StateError, 'Noninteger count was accepted');
    probe.noFalseSuccess();
  });

  await run('duplicate-proofs-fail-without-marker', () async {
    final probe = Probe();
    final error = await failure(
      () => probe.invoke(() => completeEvent() + completeEvent()),
    );
    check(error is StateError, 'Duplicate proofs were accepted');
    probe.noFalseSuccess();
  });

  await run('missing-nonce-cannot-bypass-proof', () async {
    final probe = Probe();
    final error = await failure(() => probe.invoke(completeEvent, nonce: null));
    check(error is ArgumentError, 'Missing nonce was accepted');
    check(probe.reads == 0, 'Missing nonce reached the journal');
    probe.noFalseSuccess();
  });

  await run('native-journal-read-with-windows-byte-lock', () async {
    final directory = Directory.systemTemp.createTempSync(
      'failforge-gate-barrier-',
    );
    final journal = File('${directory.path}/events.jsonl')
      ..writeAsStringSync(completeEvent());
    RandomAccessFile? lock;
    Timer? release;
    var nativeSharingErrors = 0;
    final nativeCodes = <int>[];
    final order = <String>[];
    try {
      if (Platform.isWindows) {
        lock = journal.openSync(mode: FileMode.append);
        lock.lockSync(FileLock.exclusive);
        release = Timer(const Duration(milliseconds: 30), () {
          lock!.unlockSync();
          lock!.closeSync();
          lock = null;
        });
      }
      final error = await failure(
        () => assertEnabledAfterGate(
          value: false,
          journal: journal.path,
          nonce: 'native-regression-nonce',
          timeout: const Duration(seconds: 5),
          pollInterval: const Duration(milliseconds: 2),
          readJournal: (path) {
            try {
              return readGateJournal(path);
            } on FileSystemException catch (error) {
              if (Platform.isWindows &&
                  [32, 33].contains(error.osError?.errorCode)) {
                nativeSharingErrors += 1;
                nativeCodes.add(error.osError!.errorCode);
              }
              rethrow;
            }
          },
          emitMarker: (marker) {
            check(
              marker['nonce'] == 'native-regression-nonce',
              'Native marker nonce changed',
            );
            order.add('marker');
          },
          assertion: (value) {
            check(!value, 'Native mutation value changed');
            order.add('assert:false');
            throw ExpectedAssertion();
          },
        ),
      );
      check(
        error is ExpectedAssertion,
        'Native IO replaced the intended assertion: $error',
      );
      check(
        order.join(',') == 'marker,assert:false',
        'Native marker/assertion order changed',
      );
      if (Platform.isWindows) {
        check(
          nativeSharingErrors > 0,
          'Windows byte lock never blocked a real journal read',
        );
      }
      print(
        jsonEncode({
          'kind': 'native_gate_barrier_probe',
          'platform': Platform.operatingSystem,
          'windowsByteLockExercised': Platform.isWindows,
          'sharingErrorsObserved': nativeSharingErrors,
          'nativeErrorCodes': nativeCodes,
        }),
      );
    } finally {
      release?.cancel();
      lock?.closeSync();
      directory.deleteSync(recursive: true);
    }
  });

  await run('native-retained-delete-writer-compatible-reader', () async {
    final directory = Directory.systemTemp.createTempSync(
      'failforge-shared-journal-',
    );
    final journal = File('${directory.path}/events.jsonl')
      ..writeAsStringSync(completeEvent());
    WindowsDeleteWriter? writer;
    int? legacyError;
    final order = <String>[];
    try {
      if (Platform.isWindows) {
        writer = WindowsDeleteWriter(journal.path);
        writer.writeBytes(utf8.encode(completeEvent()));
        try {
          journal.readAsStringSync();
        } on FileSystemException catch (error) {
          legacyError = error.osError?.errorCode;
        }
        check(
          legacyError == 32,
          'Legacy Dart reader did not reproduce DELETE-sharing error32',
        );
      }
      final error = await failure(
        () => assertEnabledAfterGate(
          value: false,
          journal: journal.path,
          nonce: 'retained-delete-writer-nonce',
          timeout: const Duration(seconds: 5),
          emitMarker: (marker) {
            check(
              marker['nonce'] == 'retained-delete-writer-nonce',
              'Shared reader nonce changed',
            );
            if (Platform.isWindows)
              check(writer!.isOpen, 'Writer closed before marker');
            order.add('marker');
          },
          assertion: (value) {
            if (Platform.isWindows)
              check(writer!.isOpen, 'Writer closed before assertion');
            check(!value, 'Mutant value changed');
            order.add('assert:false');
            throw ExpectedAssertion();
          },
        ),
      );
      check(
        error is ExpectedAssertion,
        'Compatible journal reader failed: $error',
      );
      check(
        order.join(',') == 'marker,assert:false',
        'Shared reader did not reach intended assertion',
      );
      print(
        jsonEncode({
          'kind': 'native_gate_shared_reader_probe',
          'platform': Platform.operatingSystem,
          'retainedDeleteWriterExercised': Platform.isWindows,
          'legacyErrorCode': legacyError,
          'compatibleReaderSucceededWhileWriterHeld':
              Platform.isWindows && writer!.isOpen,
          'portableReaderSucceeded': true,
        }),
      );
    } finally {
      writer?.close();
      directory.deleteSync(recursive: true);
    }
  });

  await run('native-reader-multichunk-unicode-is-not-aliased', () async {
    final directory = Directory.systemTemp.createTempSync(
      'failforge-multichunk-journal-',
    );
    try {
      final expected = '${'A' * 65530}第一🙂€${'B' * 65537}${'尾я' * 7000}';
      final bytes = utf8.encode(expected);
      check(
        bytes.length > 2 * 65536 && bytes.length < maxGateJournalBytes,
        'Wrong multichunk fixture size',
      );
      final file = File('${directory.path}/journal')..writeAsBytesSync(bytes);
      check(
        readGateJournal(file.path) == expected,
        'Multi-chunk reader aliased or corrupted UTF-8 bytes',
      );
      print(
        jsonEncode({
          'kind': 'native_gate_multichunk_probe',
          'platform': Platform.operatingSystem,
          'windowsNativeReaderExercised': Platform.isWindows,
          'bytes': bytes.length,
          'contentConfirmed': true,
        }),
      );
    } finally {
      directory.deleteSync(recursive: true);
    }
  });

  await run('native-reader-size-policy', () async {
    final directory = Directory.systemTemp.createTempSync(
      'failforge-capped-journal-',
    );
    try {
      final file = File('${directory.path}/journal')
        ..writeAsStringSync('x' * (maxGateJournalBytes + 1));
      if (Platform.isWindows) {
        final error = await failure(() async {
          readGateJournal(file.path);
        });
        check(
          error is FileSystemException && '$error'.contains('1 MiB limit'),
          'Windows reader accepted oversized journal',
        );
      } else {
        check(
          readGateJournal(file.path).length == maxGateJournalBytes + 1,
          'Ordinary non-Windows file reader changed',
        );
      }
      print(
        jsonEncode({
          'kind': 'native_gate_size_cap_probe',
          'platform': Platform.operatingSystem,
          'windowsSizeCapEnforced': Platform.isWindows,
          'inputBytes': maxGateJournalBytes + 1,
        }),
      );
    } finally {
      directory.deleteSync(recursive: true);
    }
  });

  print(
    jsonEncode({
      'kind': 'gate_barrier_regressions',
      'passed': passed,
      'failed': failures.length,
      'failures': failures,
    }),
  );
  if (failures.isNotEmpty) exitCode = 1;
}

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:path/path.dart' as p;
import 'package:test/test.dart';

String _shellQuote(String value) => "'${value.replaceAll("'", "'\"'\"'")}'";

void main() {
  final driver = File('test/support/prepare_adapter_driver.dart').absolute.path;
  final packageConfig = File('.dart_tool/package_config.json').absolute.path;
  late Directory temporary;

  setUp(() async {
    temporary = await Directory.systemTemp.createTemp('failforge pub signal ');
  });

  tearDown(() async => temporary.delete(recursive: true));

  Future<void> checkSignal(ProcessSignal signal) async {
    final marker = File(p.join(temporary.path, 'cleanup marker'));
    final fakeDart = File(p.join(temporary.path, 'fake dart'));
    final command = [
      Platform.resolvedExecutable,
      '--packages=$packageConfig',
      driver,
      'pub-child',
      marker.path,
    ].map(_shellQuote).join(' ');
    await fakeDart.writeAsString('#!/bin/sh\nexec $command\n');
    final permissions = await Process.run('chmod', ['755', fakeDart.path]);
    expect(permissions.exitCode, 0);

    final process = await Process.start(Platform.resolvedExecutable, [
      '--packages=$packageConfig',
      driver,
      fakeDart.path,
      temporary.path,
    ]);
    var exited = false;
    int? childPid;
    var childCleanedUp = false;
    unawaited(process.exitCode.then((_) => exited = true));
    addTearDown(() async {
      if (!exited) {
        process.kill(ProcessSignal.sigkill);
        await process.exitCode.timeout(const Duration(seconds: 5));
      }
      if (childPid != null && !childCleanedUp) {
        // Also clean up an orphan if an assertion interrupted the test before
        // the launcher had an opportunity to forward its cancellation signal.
        Process.killPid(childPid!, ProcessSignal.sigkill);
      }
    });

    final output = process.stdout.transform(utf8.decoder).join();
    final errors = <String>[];
    final ready = Completer<void>();
    final streamDone = Completer<void>();
    final subscription = process.stderr
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen(
          (line) {
            errors.add(line);
            if (line == 'cleanup:complete:1') childCleanedUp = true;
            if (line.startsWith('ready:') && !ready.isCompleted) {
              childPid = int.parse(line.substring('ready:'.length));
              ready.complete();
            }
          },
          onError: (Object error, StackTrace stack) {
            if (!ready.isCompleted) ready.completeError(error, stack);
            if (!streamDone.isCompleted) streamDone.completeError(error, stack);
          },
          onDone: () {
            if (!ready.isCompleted) {
              ready.completeError(
                StateError('Pub fixture exited before readiness.'),
              );
            }
            if (!streamDone.isCompleted) streamDone.complete();
          },
        );
    await ready.future.timeout(const Duration(seconds: 15));
    expect(await marker.exists(), isFalse);

    final elapsed = Stopwatch()..start();
    // Target only the launcher PID: the Pub child will receive nothing unless
    // prepareAdapter explicitly forwards this signal.
    expect(process.kill(signal), isTrue);
    expect(await process.exitCode.timeout(const Duration(seconds: 5)), 130);
    await streamDone.future.timeout(const Duration(seconds: 5));
    await subscription.cancel();

    expect(await output, 'cancelled:SetupCancelled\n');
    expect(errors, [
      'ready:$childPid',
      'signal:${signal.signalNumber}:1',
      'cleanup:complete:1',
    ]);
    expect(await marker.readAsString(), '${signal.signalNumber}:1');
    expect(
      elapsed.elapsed,
      greaterThanOrEqualTo(const Duration(milliseconds: 300)),
    );
  }

  for (final signal in [ProcessSignal.sigint, ProcessSignal.sigterm]) {
    test(
      'PID-directed $signal cancels Pub and waits for graceful cleanup',
      () => checkSignal(signal),
      skip: Platform.isWindows ? 'Unix shell and PID signal semantics.' : false,
    );
  }
}

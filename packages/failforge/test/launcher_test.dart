import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:io';

import 'package:test/test.dart';

void main() {
  final driver = File('test/support/launcher_driver.dart').absolute.path;
  final packageConfig = File('.dart_tool/package_config.json').absolute.path;
  late Directory compiledRoot;
  late String compiledDriver;
  late Directory temporary;

  setUpAll(() async {
    if (!Platform.isWindows) return;
    compiledRoot = await Directory.systemTemp.createTemp(
      'failforge native driver ',
    );
    compiledDriver = '${compiledRoot.path}/launcher.exe';
    final build = await Process.run(Platform.resolvedExecutable, [
      '--suppress-analytics',
      'compile',
      'exe',
      driver,
      '-o',
      compiledDriver,
    ]);
    expect(build.exitCode, 0, reason: '${build.stdout}\n${build.stderr}');
  });

  tearDownAll(() async {
    if (Platform.isWindows) await compiledRoot.delete(recursive: true);
  });

  setUp(() async {
    temporary = await Directory.systemTemp.createTemp('failforge launcher ');
  });

  tearDown(() async {
    await temporary.delete(recursive: true);
  });

  Future<Process> start(List<String> arguments) async {
    final process = await Process.start(
      Platform.isWindows ? compiledDriver : Platform.resolvedExecutable,
      [
        if (!Platform.isWindows) ...['--packages=$packageConfig', driver],
        ...arguments,
      ],
      workingDirectory: temporary.path,
      environment: {
        'FAILFORGE_TEST_DART': Platform.resolvedExecutable,
        'FAILFORGE_TEST_FIXTURE': File('test/support/process_fixture.dart')
            .absolute
            .path,
      },
    );
    var exited = false;
    unawaited(process.exitCode.then((_) => exited = true));
    addTearDown(() async {
      if (!exited) {
        process.kill(ProcessSignal.sigkill);
        await process.exitCode.timeout(const Duration(seconds: 5));
      }
    });
    return process;
  }

  test('preserves argv, working directory, stdio and exit status', () async {
    final arguments = [
      'space value',
      'semi;colon',
      r'$(touch must-not-exist)',
      '--flag=value',
      'данные🙂',
      '',
      'quote"apostrophe\'backslash\\',
    ];
    final input = <int>[0, 255, 128, ...utf8.encode('stdin\nПривет🙂\n')];
    final process = await start(['echo', ...arguments]);
    final output = process.stdout.transform(utf8.decoder).join();
    final errors = process.stderr.transform(utf8.decoder).join();
    process.stdin.add(input);
    await process.stdin.close();

    expect(await process.exitCode.timeout(const Duration(seconds: 15)), 37);
    final payload = jsonDecode(await output) as Map<String, dynamic>;
    final actualCwd = payload.remove('cwd') as String;
    expect(payload, {
      'arguments': arguments,
      'inputBase64': base64Encode(input),
    });
    if (Platform.isWindows) {
      // Windows can report the runner profile through its 8.3 alias.
      expect(FileSystemEntity.identicalSync(actualCwd, temporary.path), isTrue);
    } else {
      expect(actualCwd, temporary.resolveSymbolicLinksSync());
    }
    expect(await errors, 'fixture stderr: ошибка\n');
    expect(await File('${temporary.path}/must-not-exist').exists(), isFalse);
  });

  Future<void> checkSignal(ProcessSignal signal, {required bool group}) async {
    final process = await start([if (group) '--new-session', 'signal']);
    final output = <String>[];
    final ready = Completer<void>();
    final streamDone = Completer<void>();
    final subscription = process.stdout
        .transform(utf8.decoder)
        .transform(const LineSplitter())
        .listen(
          (line) {
            output.add(line);
            if (line == 'ready' && !ready.isCompleted) ready.complete();
          },
          onError: (Object error, StackTrace stack) {
            if (!ready.isCompleted) ready.completeError(error, stack);
            if (!streamDone.isCompleted) streamDone.completeError(error, stack);
          },
          onDone: () {
            if (!ready.isCompleted) {
              ready.completeError(
                StateError('Fixture exited before readiness.'),
              );
            }
            if (!streamDone.isCompleted) streamDone.complete();
          },
        );
    final errors = process.stderr.transform(utf8.decoder).join();
    await ready.future.timeout(const Duration(seconds: 15));

    final timer = Stopwatch()..start();
    if (group) {
      final kill = DynamicLibrary.process()
          .lookupFunction<Int32 Function(Int32, Int32), int Function(int, int)>(
            'kill',
          );
      expect(kill(-process.pid, signal.signalNumber), 0);
    } else {
      expect(process.kill(signal), isTrue);
    }

    expect(
      await process.exitCode.timeout(const Duration(seconds: 15)),
      128 + signal.signalNumber,
    );
    await streamDone.future;
    await subscription.cancel();
    expect(await errors, isEmpty);
    expect(output, [
      'ready',
      'signal:${signal.signalNumber}:1',
      'cleanup:complete:1',
    ]);
    expect(
      timer.elapsed,
      greaterThanOrEqualTo(const Duration(milliseconds: 300)),
    );
  }

  test(
    'PID-directed Ctrl+C reaches native process and waits for cleanup',
    () => checkSignal(ProcessSignal.sigint, group: false),
    skip: Platform.isWindows ? 'Unix signal semantics.' : false,
  );

  test(
    'terminal group Ctrl+C reaches native process exactly once',
    () => checkSignal(ProcessSignal.sigint, group: true),
    skip: Platform.isWindows ? 'Unix process groups.' : false,
  );

  test(
    'SIGTERM reaches native process and waits for cleanup',
    () => checkSignal(ProcessSignal.sigterm, group: false),
    skip: Platform.isWindows ? 'Unix signal semantics.' : false,
  );
}

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:path/path.dart' as p;
import 'package:test/test.dart';

void main() {
  group(
    'native Windows console cancellation',
    () {
      late Directory temporary;
      late String probe;
      late String launcher;

      Future<ProcessResult> runOwned(
        String executable,
        List<String> arguments, {
        Duration timeout = const Duration(seconds: 100),
      }) async {
        final process = await Process.start(executable, arguments);
        const decoder = Utf8Decoder(allowMalformed: true);
        final output = process.stdout.transform(decoder).join();
        final errors = process.stderr.transform(decoder).join();
        try {
          final code = await process.exitCode.timeout(timeout);
          return ProcessResult(process.pid, code, await output, await errors);
        } on TimeoutException {
          // The console controller retains a kill-on-close job for every host
          // and child it starts. Killing this owned controller closes that job.
          process.kill(ProcessSignal.sigkill);
          await process.exitCode;
          await Future.wait([output, errors]);
          rethrow;
        }
      }

      setUpAll(() async {
        temporary = await Directory.systemTemp.createTemp('failforge console ');
        probe = p.join(temporary.path, 'ConsoleProbe.exe');
        launcher = p.join(temporary.path, 'launcher.exe');
        final source = File('test/support/windows_console_probe.cs')
            .absolute
            .path;
        String psQuote(String value) => value.replaceAll("'", "''");
        final script =
            '''
\$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.UTF8Encoding]::new(\$false)
Add-Type -TypeDefinition ([IO.File]::ReadAllText('${psQuote(source)}')) -OutputAssembly '${psQuote(probe)}' -OutputType ConsoleApplication -ReferencedAssemblies @('System.dll','System.Core.dll','System.Web.Extensions.dll')
''';
        final encoded = base64Encode([
          for (final unit in script.codeUnits) ...[unit & 255, unit >> 8],
        ]);
        final compileProbe = await runOwned('powershell.exe', [
          '-NoLogo',
          '-NoProfile',
          '-NonInteractive',
          '-EncodedCommand',
          encoded,
        ]);
        expect(
          compileProbe.exitCode,
          0,
          reason: '${compileProbe.stdout}\n${compileProbe.stderr}',
        );
        final compileLauncher = await runOwned(Platform.resolvedExecutable, [
          '--suppress-analytics',
          'compile',
          'exe',
          File('test/support/windows_console_driver.dart').absolute.path,
          '-o',
          launcher,
        ]);
        expect(
          compileLauncher.exitCode,
          0,
          reason: '${compileLauncher.stdout}\n${compileLauncher.stderr}',
        );
      });

      tearDownAll(() async => temporary.delete(recursive: true));

      for (final event in [0, 1]) {
        for (final wrapped in [false, true]) {
          final name =
              '${wrapped ? 'launcher' : 'control'}-'
              '${event == 0 ? 'ctrl-c' : 'ctrl-break'}';
          test(
            '$name waits for cleanup and preserves the child status',
            () async {
              final directory = Directory(p.join(temporary.path, name));
              await directory.create();
              final config = File(p.join(directory.path, 'config.json'));
              await config.writeAsString(
                jsonEncode({
                  'name': name,
                  'directory': directory.path,
                  'workingDirectory': directory.path,
                  'event': event,
                  'file': wrapped ? launcher : probe,
                  'arguments': [
                    if (wrapped) probe,
                    'fixture-signal',
                    directory.path,
                  ],
                }),
              );
              final result = await runOwned(probe, ['controller', config.path]);
              expect(
                result.exitCode,
                0,
                reason: '${result.stdout}\n${result.stderr}',
              );
              final observed = jsonDecode(
                await File(p.join(directory.path, 'host-result.json'))
                    .readAsString(),
              ) as Map<String, dynamic>;
              final controller = jsonDecode(
                await File(p.join(directory.path, 'controller-result.json'))
                    .readAsString(),
              ) as Map<String, dynamic>;
              expect(observed['hostObserved'], 1);
              expect(observed['targetExitCode'], event == 0 ? 130 : 143);
              expect(observed['fixtureAliveAtTargetExit'], false);
              expect(observed['fixtureCleanupAtTargetExit'], true);
              expect(observed['fixtureCleanupFinal'], true);
              expect(
                observed['targetExitAfterSignalMs'],
                greaterThanOrEqualTo(1900),
              );
              expect((observed['fixtureCleanup'] as Map)['count'], 1);
              expect(controller['jobDrained'], true);
            },
          );
        }
      }
    },
    skip: !Platform.isWindows
        ? 'Windows console API and native executable.'
        : false,
    timeout: const Timeout(Duration(minutes: 3)),
  );
}

import 'dart:io';

import 'package:failforge/src/environment.dart';
import 'package:failforge/src/process_launcher.dart';
import 'package:failforge/src/release.dart';
import 'package:failforge/src/runtime_manager.dart';
import 'package:failforge/src/windows_console.dart';

Future<void> main(List<String> arguments) async {
  try {
    if (arguments.isNotEmpty &&
        arguments.first == 'setup' &&
        arguments.length != 1) {
      stderr.writeln('Usage: failforge setup');
      exitCode = 64;
      return;
    }
    ensureSupportedWindowsEntrypoint();
    final dart = resolveDartExecutable();
    final manager = RuntimeManager(
      release: currentRelease(),
      cacheRoot: resolveCacheDirectory(),
      dartExecutable: dart,
      log: stderr.writeln,
    );
    final executable = await manager.ensureReady();
    if (arguments.isNotEmpty && arguments.first == 'setup') {
      stdout.writeln(
        'FailForge ${manager.release.version} is ready at ${manager.installation.path}',
      );
      return;
    }
    await stdout.flush();
    await stderr.flush();
    exitCode = await runNative(
      executable,
      arguments,
      environment: nativeEnvironment(dart),
    );
  } on SetupCancelled catch (error) {
    stderr.writeln('failforge: $error');
    exitCode = 130;
  } catch (error) {
    stderr.writeln('failforge launcher: $error');
    exitCode = 125;
  }
}

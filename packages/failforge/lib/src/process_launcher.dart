import 'dart:ffi';
import 'dart:io';

import 'package:ffi/ffi.dart';

import 'windows_console.dart';

typedef _ExecNative = Int32 Function(
  Pointer<Utf8>,
  Pointer<Pointer<Utf8>>,
  Pointer<Pointer<Utf8>>,
);
typedef _Exec = int Function(
  Pointer<Utf8>,
  Pointer<Pointer<Utf8>>,
  Pointer<Pointer<Utf8>>,
);

/// Retain the native CLI's process identity and signal ownership on Unix.
/// In particular, a terminal Ctrl+C must reach it once, without an extra
/// forwarded signal escalating its graceful cancellation to forced cleanup.
Future<int> runNative(
  String executable,
  List<String> arguments, {
  Map<String, String>? environment,
}) async {
  ensureSupportedWindowsEntrypoint();
  final childEnvironment = environment ?? Platform.environment;
  if (!Platform.isWindows) {
    final values = [executable, ...arguments];
    final envValues = [
      for (final entry in childEnvironment.entries)
        '${entry.key}=${entry.value}',
    ];
    if ([...values, ...envValues].any((value) => value.contains('\u0000'))) {
      throw ArgumentError(
        'Process arguments and environment cannot contain NUL.',
      );
    }
    final argv = calloc<Pointer<Utf8>>(values.length + 1);
    final envp = calloc<Pointer<Utf8>>(envValues.length + 1);
    final file = executable.toNativeUtf8();
    try {
      for (var i = 0; i < values.length; i++) {
        argv[i] = values[i].toNativeUtf8();
      }
      for (var i = 0; i < envValues.length; i++) {
        envp[i] = envValues[i].toNativeUtf8();
      }
      final exec = DynamicLibrary.process().lookupFunction<_ExecNative, _Exec>(
        'execve',
      );
      exec(file, argv, envp);
      // Successful execve does not return; the native process now owns cleanup.
      throw ProcessException(
        executable,
        arguments,
        'Cannot execute native FailForge.',
      );
    } finally {
      calloc.free(file);
      for (var i = 0; i < values.length; i++) {
        calloc.free(argv[i]);
      }
      for (var i = 0; i < envValues.length; i++) {
        calloc.free(envp[i]);
      }
      calloc.free(argv);
      calloc.free(envp);
    }
  }

  // Native Windows installations contain no dart.exe broker. Both the launcher
  // and child receive console events; only the native CLI acts on cancellation.
  // Keep this process alive through Ctrl+C and Ctrl+Break until child cleanup
  // has finished, preserving its exact exit status.
  final restoreConsole = preserveWindowsConsoleSignals();
  try {
    final child = await Process.start(
      executable,
      arguments,
      mode: ProcessStartMode.inheritStdio,
      environment: childEnvironment,
      includeParentEnvironment: false,
    );
    return await child.exitCode;
  } finally {
    restoreConsole();
  }
}

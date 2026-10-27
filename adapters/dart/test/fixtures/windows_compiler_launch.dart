import 'dart:convert';
import 'dart:io';

import 'package:path/path.dart' as p;

String _bounded(String value, [int limit = 1024]) {
  final bytes = utf8.encode(value);
  if (bytes.length <= limit) return value;
  return '${utf8.decode(bytes.sublist(0, limit), allowMalformed: true)}'
      '...[truncated]';
}

Future<Map<String, Object?>> _launch(String executable) async {
  try {
    final result = await Process.run(
      executable,
      ['--version'],
      stdoutEncoding: utf8,
      stderrEncoding: utf8,
    );
    return {
      'exitCode': result.exitCode,
      'stdout': _bounded(result.stdout as String, 2048),
      'stderr': _bounded(result.stderr as String, 2048),
    };
  } catch (error) {
    return {
      'exceptionType': error.runtimeType.toString(),
      'exception': _bounded(error.toString()),
      if (error is ProcessException) 'errorCode': error.errorCode,
    };
  }
}

Future<Map<String, Object?>> _probe(
  String name,
  Directory directory,
  String runtime,
) async {
  Directory.current = directory;
  final cwd = Directory.current.path;
  final relative = p.relative(runtime);
  final rawCombined = '$cwd${p.separator}$relative';
  final resolved = p.normalize(p.absolute(relative));
  final relativeExists = File(relative).existsSync();
  final absoluteExists = File(resolved).existsSync();
  return {
    'name': name,
    'cwd': _bounded(cwd),
    'cwdUtf16Length': cwd.length,
    'relativeExecutable': _bounded(relative),
    'relativeUtf16Length': relative.length,
    'rawCombinedPath': _bounded(rawCombined),
    'rawCombinedUtf16Length': rawCombined.length,
    'rawCombinedExceedsMaxPath': rawCombined.length > 260,
    'resolvedTarget': _bounded(resolved),
    'resolvedUtf16Length': resolved.length,
    'relativeFileExists': relativeExists,
    'absoluteFileExists': absoluteExists,
    'relativeLaunch': await _launch(relative),
    'absoluteLaunch': await _launch(resolved),
  };
}

/// Diagnostic only: a relative launch failure is recorded, never required.
/// Absolute launches and cleanup must succeed on every supported SDK.
Future<void> main(List<String> arguments) async {
  final originalCwd = Directory.current.path;
  Directory? temporary;
  var passed = false;
  final report = <String, Object?>{
    'probe': 'windows_compiler_launch',
    'operatingSystem': Platform.operatingSystem,
    'dartVersion': _bounded(Platform.version),
    'cases': <Map<String, Object?>>[],
    'absoluteLaunchesPassed': false,
  };
  try {
    if (arguments.length != 2) {
      throw ArgumentError('Expected SDK root and destination parent.');
    }
    final sdk = p.normalize(p.absolute(arguments[0]));
    final parent = p.normalize(p.absolute(arguments[1]));
    final sameDrive =
        p.rootPrefix(sdk).toLowerCase() == p.rootPrefix(parent).toLowerCase();
    report.addAll({
      'sdkRoot': _bounded(sdk),
      'destinationParent': _bounded(parent),
      'sameDrive': sameDrive,
    });
    if (!sameDrive) {
      throw ArgumentError('Destination parent must be on the SDK drive.');
    }
    final runtime = p.join(
      sdk,
      'bin',
      Platform.isWindows ? 'dartaotruntime.exe' : 'dartaotruntime',
    );
    if (!File(runtime).existsSync()) {
      throw StateError('SDK AOT runtime does not exist: $runtime');
    }
    temporary = Directory(parent).createTempSync('ff_compiler_launch_');
    report['temporaryRoot'] = _bounded(temporary.path);
    final short = Directory(p.join(temporary.path, 'short'))..createSync();
    Directory longParent = temporary;
    for (var index = 0; index < 7; index++) {
      longParent = Directory(p.join(longParent.path, 'n$index'))..createSync();
    }
    final paddingLength = 225 - longParent.path.length - p.separator.length;
    if (paddingLength < 5) {
      throw StateError('Destination parent leaves no room for a 225-unit cwd.');
    }
    final longName = 'long_${List.filled(paddingLength - 5, 'x').join()}';
    final long = Directory(p.join(longParent.path, longName))..createSync();
    final cases = <Map<String, Object?>>[];
    report['cases'] = cases;
    for (final entry in [('short', short), ('long', long)]) {
      final result = await _probe(entry.$1, entry.$2, runtime);
      cases.add(result);
      if (entry.$1 == 'long' && result['cwdUtf16Length'] != 225) {
        throw StateError('The long cwd is not exactly 225 UTF-16 units.');
      }
      if (entry.$1 == 'long' && result['rawCombinedExceedsMaxPath'] != true) {
        throw StateError(
          'The long raw combined path does not exceed MAX_PATH.',
        );
      }
    }
    report['absoluteLaunchesPassed'] = cases.every(
      (result) =>
          (result['absoluteLaunch'] as Map<String, Object?>)['exitCode'] == 0,
    );
    passed =
        cases.every(
          (result) =>
              result['relativeFileExists'] == true &&
              result['absoluteFileExists'] == true,
        ) &&
        report['absoluteLaunchesPassed'] == true;
  } catch (error) {
    report['error'] = _bounded(error.toString());
  } finally {
    try {
      Directory.current = originalCwd;
      report['cwdRestored'] = Directory.current.path == originalCwd;
    } catch (error) {
      report['cwdRestored'] = false;
      report['restoreError'] = _bounded(error.toString());
    }
    try {
      if (temporary != null) temporary.deleteSync(recursive: true);
      report['temporaryRemoved'] = temporary == null || !temporary.existsSync();
    } catch (error) {
      report['temporaryRemoved'] = false;
      report['cleanupError'] = _bounded(error.toString());
    }
  }
  passed =
      passed &&
      report['error'] == null &&
      report['cwdRestored'] == true &&
      report['temporaryRemoved'] == true;
  report['allPassed'] = passed;
  stdout.writeln(jsonEncode(report));
  exitCode = passed ? 0 : 1;
}

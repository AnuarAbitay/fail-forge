import 'dart:io';

import 'package:path/path.dart' as p;

/// Finds the SDK Dart executable rather than an installed AOT launcher.
///
/// A running Dart VM supplies the most direct SDK location. When this launcher
/// has been compiled, explicit SDK and PATH locations provide the fallback.
String resolveDartExecutable({
  Map<String, String>? environment,
  String? resolvedExecutable,
}) {
  final values = environment ?? Platform.environment;
  final executableName = Platform.isWindows ? 'dart.exe' : 'dart';

  final running = _sdkDart(resolvedExecutable ?? Platform.resolvedExecutable);
  if (running != null) return running;

  final sdk = _value(values, 'DART_SDK');
  if (sdk != null) {
    final explicit = _sdkDart(p.join(sdk, 'bin', executableName));
    if (explicit != null) return explicit;
  }

  final searchPath = _value(values, 'PATH');
  final directories = searchPath == null
      ? <String>[]
      : searchPath.split(Platform.isWindows ? ';' : ':');
  for (final directory in directories) {
    if (directory.isEmpty) continue;
    final dart = _sdkDart(p.join(directory, executableName));
    if (dart != null) return dart;
  }

  final flutterName = Platform.isWindows ? 'flutter.bat' : 'flutter';
  for (final directory in directories) {
    if (directory.isEmpty) continue;
    try {
      final flutter = File(p.join(directory, flutterName))
          .resolveSymbolicLinksSync();
      final dart = _sdkDart(
        p.join(p.dirname(flutter), 'cache', 'dart-sdk', 'bin', executableName),
      );
      if (dart != null) return dart;
    } on FileSystemException {
      // A missing or broken PATH entry does not hide later SDK candidates.
    }
  }

  final flutterRoot = _value(values, 'FLUTTER_ROOT');
  if (flutterRoot != null) {
    final dart = _sdkDart(
      p.join(flutterRoot, 'bin', 'cache', 'dart-sdk', 'bin', executableName),
    );
    if (dart != null) return dart;
  }

  throw StateError(
    'Cannot find a Dart SDK. Install Dart >=3.13.0 <4.0.0 and add its bin '
    'directory to PATH, add Flutter to PATH, or set DART_SDK to the SDK root.',
  );
}

String? _sdkDart(String candidate) {
  if (candidate.isEmpty) return null;
  try {
    final dart = File(candidate).resolveSymbolicLinksSync();
    final expectedName = Platform.isWindows ? 'dart.exe' : 'dart';
    if (p.basename(dart) != expectedName ||
        p.basename(p.dirname(dart)) != 'bin' ||
        !File(dart).existsSync()) {
      return null;
    }
    final version = File(p.join(p.dirname(p.dirname(dart)), 'version'));
    if (!version.existsSync() || version.readAsStringSync().trim().isEmpty) {
      return null;
    }
    return dart;
  } on FileSystemException {
    return null;
  } on FormatException {
    return null;
  }
}

/// Chooses the runtime cache root without creating it.
Directory resolveCacheDirectory({
  Map<String, String>? environment,
  String? operatingSystem,
}) {
  final values = environment ?? Platform.environment;
  final os = operatingSystem ?? Platform.operatingSystem;
  final paths = p.Context(
    style: os == 'windows' ? p.Style.windows : p.Style.posix,
    current: Directory.current.path,
  );
  final custom = _value(values, 'FAILFORGE_CACHE', windows: os == 'windows');
  if (custom != null) return Directory(paths.absolute(custom));

  String requiredValue(String name) {
    final value = _value(values, name, windows: os == 'windows');
    if (value != null) return value;
    throw StateError(
      '$name is not set. Set FAILFORGE_CACHE to a writable runtime cache '
      'directory.',
    );
  }

  final String root;
  switch (os) {
    case 'macos':
      root = paths.join(
        requiredValue('HOME'),
        'Library',
        'Caches',
        'failforge',
      );
    case 'linux':
      final xdgCache = _value(values, 'XDG_CACHE_HOME');
      root = paths.join(
        xdgCache ?? paths.join(requiredValue('HOME'), '.cache'),
        'failforge',
      );
    case 'windows':
      root = paths.join(requiredValue('LOCALAPPDATA'), 'failforge', 'cache');
    default:
      throw UnsupportedError(
        'FailForge does not support operating system $os.',
      );
  }
  return Directory(paths.absolute(root));
}

/// Preserves SDK search priority and adds a fallback for a VM-only invocation.
Map<String, String> nativeEnvironment(
  String dartExecutable, {
  Map<String, String>? environment,
}) {
  final values = Map<String, String>.of(environment ?? Platform.environment);
  final pathKey = Platform.isWindows
      ? values.keys.firstWhere(
          (key) => key.toUpperCase() == 'PATH',
          orElse: () => 'PATH',
        )
      : 'PATH';
  final separator = Platform.isWindows ? ';' : ':';
  final originalPath = values[pathKey];
  final dartBin = p.dirname(p.absolute(dartExecutable));
  final present =
      originalPath
          ?.split(separator)
          .any(
            (entry) => entry.isNotEmpty && p.equals(p.absolute(entry), dartBin),
          ) ??
      false;
  if (!present) {
    values[pathKey] = originalPath == null || originalPath.isEmpty
        ? dartBin
        : '$originalPath$separator$dartBin';
  }
  return values;
}

String? _value(Map<String, String> environment, String name, {bool? windows}) {
  var value = environment[name];
  if (value == null && (windows ?? Platform.isWindows)) {
    for (final entry in environment.entries) {
      if (entry.key.toUpperCase() == name.toUpperCase()) {
        value = entry.value;
        break;
      }
    }
  }
  return value == null || value.isEmpty ? null : value;
}

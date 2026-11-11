import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart' as crypto;
import 'package:path/path.dart' as p;

import 'release.dart';
import 'runtime_archive.dart';

typedef Download = Future<void> Function(Uri uri, File destination);
typedef PrepareAdapter = Future<void> Function(String dart, Directory adapter);
typedef ExtractArchive = Future<void> Function(
  File archive,
  Directory destination, {
  required String version,
  required String target,
});

class SetupCancelled implements Exception {
  @override
  String toString() => 'Setup interrupted; run failforge setup to try again.';
}

/// Installs only the version and asset digests compiled into this launcher.
class RuntimeManager {
  RuntimeManager({
    required this.release,
    required this.cacheRoot,
    required this.dartExecutable,
    this.log,
    Download? download,
    PrepareAdapter? prepare,
    ExtractArchive? extract,
  }) : download = download ?? downloadRelease,
       prepare = prepare ?? prepareAdapter,
       extract = extract ?? extractRuntimeArchive;

  final ReleaseArtifact release;
  final Directory cacheRoot;
  final String dartExecutable;
  final void Function(String)? log;
  final Download download;
  final PrepareAdapter prepare;
  final ExtractArchive extract;

  // Pub's package_config contains absolute SDK/cache paths. Keep a separate
  // runtime for each preparation context so another shell cannot rewrite it
  // beneath a running native campaign after this install lock is released.
  late final Map<String, String> _context = _contextFingerprint();
  late final String installationKey =
      '${release.cacheKey}-'
      '${crypto.sha256.convert(utf8.encode(jsonEncode(_context))).toString().substring(0, 16)}';

  Directory get installation =>
      Directory(p.join(cacheRoot.path, installationKey));
  File get _prepared =>
      File(p.join(cacheRoot.path, '$installationKey.prepared.json'));

  Future<String> ensureReady() async {
    await cacheRoot.create(recursive: true);
    final lockFile = File(p.join(cacheRoot.path, '$installationKey.lock'));
    final lockType = await FileSystemEntity.type(
      lockFile.path,
      followLinks: false,
    );
    if (lockType != FileSystemEntityType.notFound &&
        lockType != FileSystemEntityType.file) {
      throw FileSystemException(
        'Runtime lock must be a regular file',
        lockFile.path,
      );
    }
    final lock = await lockFile.open(mode: FileMode.append);
    var locked = false;
    try {
      // Nonblocking attempts keep the event loop responsive. An OS lock is
      // released on process death; stale lock files themselves are harmless.
      final deadline = DateTime.now().add(const Duration(minutes: 5));
      while (!locked) {
        try {
          await lock.lock(FileLock.exclusive);
          locked = true;
        } on FileSystemException {
          if (DateTime.now().isAfter(deadline)) {
            throw FileSystemException(
              'Timed out waiting for another FailForge setup',
              lockFile.path,
            );
          }
          await Future<void>.delayed(const Duration(milliseconds: 100));
        }
      }

      if (!await _validInstallation()) {
        await _removeManaged(installation.path);
        await _removeManaged(_prepared.path);
        await _install();
      }

      final fingerprint = await _fingerprint();
      if (!await _isPrepared(fingerprint)) {
        await _removeManaged(_prepared.path);
        log?.call(
          'Preparing Dart adapter dependencies for FailForge ${release.version}...',
        );
        await prepare(
          dartExecutable,
          Directory(p.join(installation.path, 'adapters', 'dart')),
        );
        if (!await _packageRootsExist()) {
          throw StateError(
            'Adapter dependency preparation did not produce a usable package configuration.',
          );
        }
        // --enforce-lockfile must not change the pinned runtime payload.
        await _verifyInstallation();
        final pendingDirectory = await cacheRoot.createTemp('.prepared-');
        try {
          final pending = File(p.join(pendingDirectory.path, 'marker.json'));
          await pending.writeAsString(jsonEncode(fingerprint), flush: true);
          await pending.rename(_prepared.path);
        } finally {
          await pendingDirectory.delete(recursive: true);
        }
      }

      final suffix = release.target.contains('windows') ? '.exe' : '';
      return p.join(installation.path, 'bin', 'failforge$suffix');
    } finally {
      if (locked) await lock.unlock();
      await lock.close();
    }
  }

  Future<void> _install() async {
    final archive = File(p.join(cacheRoot.path, '$installationKey.download'));
    final staging = Directory(
      p.join(cacheRoot.path, '$installationKey.install'),
    );
    await _removeManaged(archive.path);
    await _removeManaged(staging.path);
    try {
      log?.call(
        'Downloading FailForge ${release.version} (${release.target})...',
      );
      await download(release.uri, archive);
      if (await archive.length() > maxDownloadBytes ||
          (await crypto.sha256.bind(archive.openRead()).first).toString() !=
              release.sha256) {
        throw const FormatException(
          'Native release archive SHA-256 does not match the pinned checksum.',
        );
      }
      await extract(
        archive,
        staging,
        version: release.version,
        target: release.target,
      );
      final manifest = File(p.join(staging.path, 'package-manifest.json'));
      if ((await crypto.sha256.bind(manifest.openRead()).first).toString() !=
          release.manifestSha256) {
        throw const FormatException(
          'Native release manifest SHA-256 does not match the pinned checksum.',
        );
      }
      await staging.rename(installation.path);
    } finally {
      await _removeManaged(archive.path);
      await _removeManaged(staging.path);
    }
  }

  Future<void> _verifyInstallation() async {
    if (await FileSystemEntity.type(installation.path, followLinks: false) !=
        FileSystemEntityType.directory) {
      throw const FormatException('Cached runtime is not a regular directory.');
    }
    final manifest = File(p.join(installation.path, 'package-manifest.json'));
    if (await FileSystemEntity.type(manifest.path, followLinks: false) !=
        FileSystemEntityType.file) {
      throw const FormatException(
        'Cached runtime manifest is not a regular file.',
      );
    }
    if (await manifest.length() > 4 * 1024 * 1024 ||
        (await crypto.sha256.bind(manifest.openRead()).first).toString() !=
            release.manifestSha256) {
      throw const FormatException(
        'Cached runtime manifest differs from the pinned release.',
      );
    }
    await verifyRuntimeManifest(
      installation,
      version: release.version,
      target: release.target,
      allowGenerated: true,
    );
  }

  Future<bool> _validInstallation() async {
    try {
      await _verifyInstallation();
      return true;
    } on FileSystemException {
      return false;
    } on FormatException {
      return false;
    } on RuntimeArchiveException {
      return false;
    }
  }

  Future<Map<String, String>> _fingerprint() async => _context;

  Map<String, String> _contextFingerprint() {
    final dart = File(dartExecutable).resolveSymbolicLinksSync();
    final versionFile = File(p.join(p.dirname(p.dirname(dart)), 'version'));
    return {
      'archiveSha256': release.sha256,
      'dart': dart,
      'sdkVersion': versionFile.readAsStringSync(),
      'pubCache': Platform.environment['PUB_CACHE'] == null
          ? 'default:${Platform.environment['HOME']}:${Platform.environment['LOCALAPPDATA']}'
          : p.normalize(p.absolute(Platform.environment['PUB_CACHE']!)),
    };
  }

  Future<bool> _isPrepared(Map<String, String> fingerprint) async {
    try {
      if (await FileSystemEntity.type(_prepared.path, followLinks: false) !=
          FileSystemEntityType.file)
        return false;
      if (await _prepared.length() > 16384) return false;
      final saved = jsonDecode(await _prepared.readAsString());
      if (saved is! Map ||
          saved.length != fingerprint.length ||
          fingerprint.entries.any((entry) => saved[entry.key] != entry.value))
        return false;
      return await _packageRootsExist();
    } on FileSystemException {
      return false;
    } on FormatException {
      return false;
    }
  }

  Future<bool> _packageRootsExist() async {
    try {
      final config = File(
        p.join(
          installation.path,
          'adapters',
          'dart',
          '.dart_tool',
          'package_config.json',
        ),
      );
      if (await config.length() > 4 * 1024 * 1024) return false;
      final data = jsonDecode(await config.readAsString());
      if (data is! Map ||
          data['configVersion'] != 2 ||
          data['packages'] is! List ||
          (data['packages'] as List).isEmpty)
        return false;
      for (final package in data['packages'] as List) {
        if (package is! Map || package['rootUri'] is! String) return false;
        final uri = config.uri.resolve(package['rootUri'] as String);
        if (uri.scheme != 'file' || !await Directory.fromUri(uri).exists())
          return false;
      }
      return true;
    } on FileSystemException {
      return false;
    } on FormatException {
      return false;
    }
  }
}

Future<void> _removeManaged(String path) async {
  final type = await FileSystemEntity.type(path, followLinks: false);
  if (type == FileSystemEntityType.directory) {
    await Directory(path).delete(recursive: true);
  } else if (type == FileSystemEntityType.link) {
    await Link(path).delete();
  } else if (type != FileSystemEntityType.notFound) {
    await File(path).delete();
  }
}

const maxDownloadBytes = 128 * 1024 * 1024;

/// No authentication, persisted cookies, or mutable "latest" release lookup.
Future<void> downloadRelease(Uri uri, File destination) async {
  final client = HttpClient()..connectionTimeout = const Duration(seconds: 30);
  client.userAgent = 'failforge-dart-launcher/0.1.0-beta.1';
  RandomAccessFile? output;
  try {
    var current = uri;
    for (var redirects = 0; redirects <= 5; redirects++) {
      if (current.scheme != 'https' || current.userInfo.isNotEmpty) {
        throw const FormatException(
          'Native releases must be downloaded over anonymous HTTPS.',
        );
      }
      final request = await client
          .getUrl(current)
          .timeout(const Duration(seconds: 30));
      request.followRedirects = false;
      final response = await request.close().timeout(
        const Duration(seconds: 30),
      );
      if ([301, 302, 303, 307, 308].contains(response.statusCode)) {
        final location = response.headers.value(HttpHeaders.locationHeader);
        if (location == null)
          throw const HttpException('Release redirect has no location.');
        current = current.resolve(location);
        continue;
      }
      if (response.statusCode != HttpStatus.ok) {
        throw HttpException(
          'Native release download returned HTTP ${response.statusCode}.',
        );
      }
      if (response.contentLength > maxDownloadBytes) {
        throw const FormatException(
          'Native release archive exceeds the download limit.',
        );
      }
      // Await every file operation. IOSink can report an asynchronous open or
      // write failure through its independent `done` future before a later
      // flush/close is awaited, bypassing this function's error handling.
      output = await destination.open(mode: FileMode.write);
      var received = 0;
      await for (final chunk in response.timeout(const Duration(seconds: 30))) {
        received += chunk.length;
        if (received > maxDownloadBytes) {
          throw const FormatException(
            'Native release archive exceeds the download limit.',
          );
        }
        await output.writeFrom(chunk);
      }
      await output.flush();
      return;
    }
    throw const HttpException('Too many native release redirects.');
  } finally {
    try {
      await output?.close();
    } finally {
      client.close(force: true);
    }
  }
}

Future<void> prepareAdapter(String dart, Directory adapter) async {
  var interrupted = false;
  Process? process;
  ProcessSignal? pendingSignal;
  Timer? cancellationDeadline;
  final signals = <StreamSubscription<ProcessSignal>>[];
  void cancel(ProcessSignal signal) {
    interrupted = true;
    pendingSignal = signal;
    final child = process;
    if (child == null) return;
    // Pub setup owns no mutation campaign. Forward PID-directed cancellation
    // too; a terminal may also deliver this signal directly to the Pub child.
    if (!Platform.isWindows) child.kill(signal);
    cancellationDeadline ??= Timer(const Duration(seconds: 10), () {
      child.kill(ProcessSignal.sigkill);
    });
  }

  try {
    signals.add(ProcessSignal.sigint.watch().listen(cancel));
    if (!Platform.isWindows) {
      signals.add(ProcessSignal.sigterm.watch().listen(cancel));
    }
    final child = await Process.start(dart, [
      '--suppress-analytics',
      'pub',
      'get',
      '--enforce-lockfile',
      '--directory',
      adapter.path,
    ]);
    process = child;
    if (pendingSignal != null) cancel(pendingSignal!);
    await child.stdin.close();
    // Setup diagnostics belong on stderr so first-run preparation does not
    // contaminate the native command's stdout (for example --version).
    final output = child.stdout.listen(stderr.add).asFuture<void>();
    final errors = child.stderr.listen(stderr.add).asFuture<void>();
    final code = await child.exitCode;
    await Future.wait([output, errors]);
    if (interrupted) throw SetupCancelled();
    if (code != 0) {
      throw StateError(
        'Adapter pub get failed (exit $code). Check network access and Dart SDK, then retry failforge setup.',
      );
    }
  } finally {
    cancellationDeadline?.cancel();
    for (final signal in signals) {
      await signal.cancel();
    }
  }
}

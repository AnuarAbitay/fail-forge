import 'dart:io';

import 'package:failforge/src/runtime_manager.dart';
import 'package:path/path.dart' as p;

import 'runtime_fixture.dart';

Future<void> main(List<String> arguments) async {
  final cache = Directory(p.join(arguments[0], 'cache'));
  final kind = arguments[1];
  final manager = RuntimeManager(
    release: fixtureRelease,
    cacheRoot: cache,
    dartExecutable: Platform.resolvedExecutable,
    download: (uri, file) =>
        file.writeAsBytes(fixtureArchiveBytes).then((_) {}),
    extract: extractFixture,
    prepare: (dart, adapter) => prepareFixture(
      adapter,
      Directory(p.join(arguments[0], 'dependencies')),
    ),
  );
  await cache.create(recursive: true);
  final String path;
  if (kind == 'manifest') {
    await manager.installation.create(recursive: true);
    path = p.join(manager.installation.path, 'package-manifest.json');
  } else if (kind == 'marker') {
    await manager.ensureReady();
    path = p.join(cache.path, '${manager.installationKey}.prepared.json');
    await File(path).delete();
  } else {
    path = p.join(cache.path, '${manager.installationKey}.lock');
  }
  final fifo = await Process.run('mkfifo', [path]);
  if (fifo.exitCode != 0) throw StateError('Cannot create FIFO fixture.');
  try {
    await manager.ensureReady();
    if (kind == 'lock') throw StateError('Special lock should be rejected.');
  } on FileSystemException {
    if (kind != 'lock') rethrow;
  }
  stdout.writeln('$kind:checked');
}

import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:failforge/src/release.dart';
import 'package:path/path.dart' as p;

const fixtureVersion = '0.1.0';
const fixtureTarget = 'aarch64-apple-darwin';
final fixtureArchiveBytes = utf8.encode('checked native archive fixture');
final fixtureFiles = <String, List<int>>{
  'bin/failforge': utf8.encode('native-cli'),
  'bin/failforge-adapter-campaign': utf8.encode('native-helper'),
  'adapters/dart/pubspec.yaml': utf8.encode('name: failforge_dart_adapter\n'),
  'adapters/dart/pubspec.lock': utf8.encode('packages: {}\n'),
  'adapters/dart/bin/failforge_dart_adapter.dart': utf8.encode(
    'void main() {}\n',
  ),
  'adapters/dart/lib/failforge_dart_adapter.dart': utf8.encode('library;\n'),
  'LICENSE': utf8.encode('license'),
  'NOTICE': utf8.encode('notice'),
};

final fixtureManifest = jsonEncode({
  'schemaVersion': 1,
  'archiveRoot': 'failforge-$fixtureVersion-$fixtureTarget',
  'version': fixtureVersion,
  'target': fixtureTarget,
  'files': [
    for (final file in fixtureFiles.entries)
      {
        'path': file.key,
        'bytes': file.value.length,
        'sha256': sha256.convert(file.value).toString(),
        'executable': file.key.startsWith('bin/'),
      },
  ],
});

final fixtureRelease = ReleaseArtifact(
  version: fixtureVersion,
  target: fixtureTarget,
  sha256: sha256.convert(fixtureArchiveBytes).toString(),
  manifestSha256: sha256.convert(utf8.encode(fixtureManifest)).toString(),
);

Future<void> extractFixture(
  File archive,
  Directory destination, {
  required String version,
  required String target,
}) async {
  if (version != fixtureVersion || target != fixtureTarget) {
    throw StateError('Unexpected release identity.');
  }
  await destination.create(recursive: true);
  for (final entry in fixtureFiles.entries) {
    final file = File(p.joinAll([destination.path, ...entry.key.split('/')]));
    await file.parent.create(recursive: true);
    await file.writeAsBytes(entry.value);
    if (!Platform.isWindows && entry.key.startsWith('bin/')) {
      final result = await Process.run('chmod', ['755', file.path]);
      if (result.exitCode != 0) {
        throw StateError('Cannot mark fixture executable.');
      }
    }
  }
  await File(p.join(destination.path, 'package-manifest.json'))
      .writeAsString(fixtureManifest);
}

Future<void> prepareFixture(
  Directory adapter,
  Directory dependencyCache,
) async {
  await dependencyCache.create(recursive: true);
  final config = File(
    p.join(adapter.path, '.dart_tool', 'package_config.json'),
  );
  await config.parent.create(recursive: true);
  await config.writeAsString(
    jsonEncode({
      'configVersion': 2,
      'packages': [
        {'name': 'test_dependency', 'rootUri': dependencyCache.uri.toString()},
      ],
    }),
  );
}

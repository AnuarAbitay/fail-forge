import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';
import 'package:failforge/src/runtime_archive.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

const _version = '0.1.0';
const _target = 'aarch64-apple-darwin';
const _root = 'failforge-$_version-$_target';

class _TarEntry {
  _TarEntry(this.name, List<int> bytes, {this.type = 48, this.size})
    : bytes = Uint8List.fromList(bytes);

  final String name;
  final Uint8List bytes;
  final int type;
  final int? size;
}

void _field(Uint8List header, int start, int length, String value) {
  final bytes = utf8.encode(value);
  if (bytes.length > length) throw ArgumentError('header field too long');
  header.setRange(start, start + bytes.length, bytes);
}

List<int> _tar(List<_TarEntry> entries, {bool endMarker = true}) {
  final result = BytesBuilder(copy: false);
  for (final entry in entries) {
    final header = Uint8List(512);
    _field(header, 0, 100, entry.name);
    _field(header, 100, 8, '0000644');
    _field(header, 108, 8, '0000000');
    _field(header, 116, 8, '0000000');
    _field(
      header,
      124,
      12,
      (entry.size ?? entry.bytes.length).toRadixString(8).padLeft(11, '0'),
    );
    _field(header, 136, 12, '00000000000');
    header.fillRange(148, 156, 32);
    header[156] = entry.type;
    _field(header, 257, 6, 'ustar');
    _field(header, 263, 2, '00');
    final sum = header.fold<int>(0, (a, b) => a + b);
    _field(header, 148, 8, '${sum.toRadixString(8).padLeft(6, '0')}\x00 ');
    result.add(header);
    result.add(entry.bytes);
    if (entry.bytes.length % 512 != 0) {
      result.add(Uint8List(512 - entry.bytes.length % 512));
    }
  }
  if (endMarker) result.add(Uint8List(1024));
  return result.takeBytes();
}

Map<String, List<int>> _files({String suffix = ''}) => {
  'bin/failforge$suffix': utf8.encode('native-cli'),
  'bin/failforge-adapter-campaign$suffix': utf8.encode('native-helper'),
  'adapters/dart/pubspec.yaml': utf8.encode('name: failforge_dart_adapter\n'),
  'adapters/dart/pubspec.lock': utf8.encode('packages: {}\n'),
  'adapters/dart/bin/failforge_dart_adapter.dart': utf8.encode(
    'void main() {}\n',
  ),
  'adapters/dart/lib/failforge_dart_adapter.dart': utf8.encode('library;\n'),
  'LICENSE': utf8.encode('license'),
  'NOTICE': utf8.encode('notice'),
};

List<_TarEntry> _release({
  Map<String, List<int>>? files,
  void Function(Map<String, dynamic>)? modifyManifest,
  String target = _target,
}) {
  final payload = files ?? _files();
  final root = 'failforge-$_version-$target';
  final manifest = <String, dynamic>{
    'schemaVersion': 1,
    'archiveRoot': root,
    'version': _version,
    'target': target,
    'files': [
      for (final file in payload.entries)
        {
          'path': file.key,
          'bytes': file.value.length,
          'sha256': sha256.convert(file.value).toString(),
          'executable': file.key.startsWith('bin/'),
        },
    ],
  };
  modifyManifest?.call(manifest);
  return [
    _TarEntry('$root/', [], type: 53),
    _TarEntry('$root/package-manifest.json', utf8.encode(jsonEncode(manifest))),
    for (final file in payload.entries)
      _TarEntry('$root/${file.key}', file.value),
  ];
}

String _paxRecord(String key, String value) {
  final content = '$key=$value\n';
  var length = utf8.encode(content).length + 2;
  while (true) {
    final record = '$length $content';
    final actual = utf8.encode(record).length;
    if (length == actual) return record;
    length = actual;
  }
}

void main() {
  late Directory temporary;
  late Directory destination;
  late File archive;

  setUp(() async {
    temporary = await Directory.systemTemp.createTemp('failforge-archive-');
    destination = Directory(p.join(temporary.path, 'runtime'));
    archive = File(p.join(temporary.path, 'runtime.tar.gz'));
  });

  tearDown(() async => temporary.delete(recursive: true));

  Future<void> extract(
    List<_TarEntry> entries, {
    String target = _target,
  }) async {
    await archive.writeAsBytes(gzip.encode(_tar(entries)));
    await extractRuntimeArchive(
      archive,
      destination,
      version: _version,
      target: target,
    );
  }

  Future<void> rejects(List<_TarEntry> entries) async {
    await expectLater(
      extract(entries),
      throwsA(isA<RuntimeArchiveException>()),
    );
    expect(await destination.exists(), isFalse);
  }

  test(
    'installs a checked release and preserves executable permissions',
    () async {
      await extract(_release());
      expect(
        await File(p.join(destination.path, 'bin', 'failforge')).readAsString(),
        'native-cli',
      );
      expect(
        await File(p.join(destination.path, 'package-manifest.json')).exists(),
        isTrue,
      );
      if (!Platform.isWindows) {
        final executable = await File(
          p.join(destination.path, 'bin', 'failforge'),
        ).stat();
        final source = await File(p.join(destination.path, 'LICENSE')).stat();
        expect(executable.mode & 0x1ff, 0x1ed);
        expect(source.mode & 0x1ff, 0x1a4);
      }
    },
  );

  test('installs Windows .exe payload names', () async {
    const target = 'x86_64-pc-windows-msvc';
    await extract(
      _release(
        files: _files(suffix: '.exe'),
        target: target,
      ),
      target: target,
    );
    expect(
      await File(p.join(destination.path, 'bin', 'failforge.exe')).exists(),
      isTrue,
    );
  });

  test(
    'reverifies cached payload and allows only Pub generated state',
    () async {
      await extract(_release());
      Future<void> verify({bool generated = false}) => verifyRuntimeManifest(
        destination,
        version: _version,
        target: _target,
        allowGenerated: generated,
      );
      await verify();
      final generated = File(
        p.join(
          destination.path,
          'adapters/dart/.dart_tool/package_config.json',
        ),
      );
      await generated.parent.create();
      await generated.writeAsString('{}');
      await expectLater(verify(), throwsA(isA<RuntimeArchiveException>()));
      await verify(generated: true);
      await File(p.join(destination.path, 'unexpected'))
          .writeAsString('unexpected');
      await expectLater(
        verify(generated: true),
        throwsA(isA<RuntimeArchiveException>()),
      );
    },
  );

  test(
    'cached verification rejects changed payload and executable permissions',
    () async {
      await extract(_release());
      final license = File(p.join(destination.path, 'LICENSE'));
      await license.writeAsString('changed');
      await expectLater(
        verifyRuntimeManifest(destination, version: _version, target: _target),
        throwsA(isA<RuntimeArchiveException>()),
      );
      await license.writeAsString('license');
      if (!Platform.isWindows) {
        await Process.run('chmod', [
          '644',
          p.join(destination.path, 'bin', 'failforge'),
        ]);
        await expectLater(
          verifyRuntimeManifest(
            destination,
            version: _version,
            target: _target,
          ),
          throwsA(isA<RuntimeArchiveException>()),
        );
      }
    },
  );

  test(
    'cached verification rejects symlinks even in generated state',
    () async {
      if (Platform.isWindows) return;
      await extract(_release());
      final generated = Directory(
        p.join(destination.path, 'adapters/dart/.dart_tool'),
      );
      await generated.create();
      await Link(p.join(generated.path, 'link')).create(archive.path);
      await expectLater(
        verifyRuntimeManifest(
          destination,
          version: _version,
          target: _target,
          allowGenerated: true,
        ),
        throwsA(isA<RuntimeArchiveException>()),
      );
    },
  );

  test('refuses an existing destination without changing it', () async {
    await destination.create();
    final marker = File(p.join(destination.path, 'keep'));
    await marker.writeAsString('keep');
    await expectLater(
      extract(_release()),
      throwsA(isA<RuntimeArchiveException>()),
    );
    expect(await marker.readAsString(), 'keep');
  });

  for (final name in [
    '../escape',
    '/absolute',
    'C:/escape',
    '$_root/../escape',
    '$_root/./escape',
    '$_root//escape',
    '$_root/back\\slash',
    '$_root/drive:name',
    '$_root/NUL.txt',
    '$_root/CON',
    '$_root/file.',
    '$_root/file ',
    '$_root/line\nbreak',
    'wrong-root/file',
  ]) {
    test('rejects unsafe path ${jsonEncode(name)} before writing', () async {
      await rejects([
        ..._release(),
        _TarEntry(name, [1]),
      ]);
    });
  }

  for (final type in [49, 50, 51, 52, 54, 55, 103]) {
    test('rejects links and special tar type $type', () async {
      await rejects([..._release(), _TarEntry('$_root/link', [], type: type)]);
    });
  }

  test('rejects duplicate paths including case aliases', () async {
    await rejects([
      ..._release(),
      _TarEntry('$_root/LICENSE', [1]),
    ]);
    await rejects([
      ..._release(),
      _TarEntry('$_root/license', [1]),
    ]);
  });

  test('rejects a regular-file ancestor of another payload', () async {
    await rejects([
      ..._release(),
      _TarEntry('$_root/adapters', [1]),
    ]);
  });

  test('rejects missing manifest and unlisted files', () async {
    await rejects([
      _TarEntry('$_root/LICENSE', [1]),
    ]);
    await rejects([
      ..._release(),
      _TarEntry('$_root/unlisted', [1]),
    ]);
  });

  test('rejects a payload whose bytes differ from the manifest', () async {
    final entries = _release();
    final index = entries.indexWhere((entry) => entry.name == '$_root/LICENSE');
    entries[index] = _TarEntry('$_root/LICENSE', utf8.encode('changed'));
    await rejects(entries);
  });

  test(
    'rejects wrong release identity, incomplete and duplicate manifests',
    () async {
      for (final modify in <void Function(Map<String, dynamic>)>[
        (manifest) => manifest['version'] = '0.2.0',
        (manifest) => manifest['target'] = 'other',
        (manifest) => manifest['archiveRoot'] = 'other',
        (manifest) => manifest['schemaVersion'] = 2,
        (manifest) => (manifest['files'] as List).removeLast(),
        (manifest) =>
            (manifest['files'] as List).add((manifest['files'] as List).first),
        (manifest) => (manifest['files'] as List).first['executable'] = false,
        (manifest) => (manifest['files'] as List).first['bytes'] = -1,
      ]) {
        await rejects(_release(modifyManifest: modify));
      }
      final payload = _files()
        ..remove('adapters/dart/bin/failforge_dart_adapter.dart');
      await rejects(_release(files: payload));
    },
  );

  test('accepts a PAX long path used by native tar', () async {
    final path = 'docs/${'long' * 25}.md';
    final payload = _files()..[path] = utf8.encode('long documentation');
    final entries = _release(files: payload);
    entries.removeLast();
    entries.add(
      _TarEntry(
        'PaxHeader',
        utf8.encode(_paxRecord('path', '$_root/$path')),
        type: 120,
      ),
    );
    entries.add(_TarEntry('placeholder', payload[path]!));
    await extract(entries);
    expect(
      await File(p.join(destination.path, path)).readAsString(),
      'long documentation',
    );
  });

  test('validates PAX paths and refuses link metadata', () async {
    for (final record in [
      _paxRecord('path', '$_root/../../escape'),
      _paxRecord('linkpath', '/escape'),
      _paxRecord('GNU.sparse.name', 'sparse'),
      '999 path=truncated\n',
    ]) {
      await rejects([
        ..._release(),
        _TarEntry('PaxHeader', utf8.encode(record), type: 120),
        _TarEntry('$_root/extra', [1]),
      ]);
    }
  });

  test(
    'rejects an oversized tar entry without allocating its payload',
    () async {
      await rejects([_TarEntry('$_root/huge', [], size: 513 * 1024 * 1024)]);
    },
  );

  test('rejects broken checksums, truncated tar and invalid gzip', () async {
    final damaged = Uint8List.fromList(_tar(_release()));
    damaged[0] ^= 1;
    for (final bytes in [
      gzip.encode(damaged),
      gzip.encode(_tar(_release(), endMarker: false)),
      gzip.encode(_tar(_release()).sublist(0, 1024)),
      [1, 2, 3],
    ]) {
      await archive.writeAsBytes(bytes);
      await expectLater(
        extractRuntimeArchive(
          archive,
          destination,
          version: _version,
          target: _target,
        ),
        throwsA(isA<RuntimeArchiveException>()),
      );
      expect(await destination.exists(), isFalse);
    }
  });
}

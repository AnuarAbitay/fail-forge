import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';
import 'package:path/path.dart' as p;

/// A release archive failed its structural or payload checks.
class RuntimeArchiveException implements Exception {
  RuntimeArchiveException(this.message);

  final String message;

  @override
  String toString() => 'Invalid FailForge runtime archive: $message';
}

const _maxCompressedBytes = 128 * 1024 * 1024;
const _maxUnpackedBytes = 512 * 1024 * 1024;
const _maxEntries = 10000;
const _blockSize = 512;

/// Extracts a checked release into a new [destination] directory.
///
/// The caller must verify the release archive's pinned SHA-256 before invoking
/// this function. Only regular files and directories are installed. Payload
/// hashes in the embedded manifest are checked before any file is written.
Future<void> extractRuntimeArchive(
  File archive,
  Directory destination, {
  required String version,
  required String target,
}) async {
  if (await FileSystemEntity.type(destination.path, followLinks: false) !=
      FileSystemEntityType.notFound) {
    throw RuntimeArchiveException('destination already exists');
  }
  if (await archive.length() > _maxCompressedBytes) {
    throw RuntimeArchiveException('compressed archive exceeds 128 MiB');
  }

  final builder = BytesBuilder(copy: false);
  var unpacked = 0;
  try {
    await for (final chunk in gzip.decoder.bind(archive.openRead())) {
      unpacked += chunk.length;
      if (unpacked > _maxUnpackedBytes) {
        throw RuntimeArchiveException('unpacked archive exceeds 512 MiB');
      }
      builder.add(chunk);
    }
  } on FormatException catch (error) {
    throw RuntimeArchiveException('invalid gzip data: ${error.message}');
  }
  final root = 'failforge-$version-$target';
  final entries = _readTar(builder.takeBytes(), root);
  final payload = _verifyManifest(entries, root, version, target);

  var created = false;
  try {
    // The parent is prepared by the installer's private staging directory.
    await destination.create();
    created = true;
    final permissions = <bool, List<String>>{false: [], true: []};
    for (final entry in entries.values) {
      if (entry.directory) continue;
      final file = File(
        p.joinAll([destination.path, ...entry.path.split('/')]),
      );
      await file.parent.create(recursive: true);
      await file.writeAsBytes(entry.bytes, flush: true);
      if (!Platform.isWindows) {
        final executable = payload[entry.path]?['executable'] == true;
        permissions[executable]!.add(file.path);
      }
    }
    if (!Platform.isWindows) {
      for (final group in permissions.entries) {
        // Batches avoid a process per source file and bound argument sizes.
        for (var offset = 0; offset < group.value.length; offset += 100) {
          final end = offset + 100 < group.value.length
              ? offset + 100
              : group.value.length;
          final result = await Process.run('chmod', [
            group.key ? '755' : '644',
            ...group.value.sublist(offset, end).map((path) => p.absolute(path)),
          ]);
          if (result.exitCode != 0) {
            throw RuntimeArchiveException(
              'cannot set runtime file permissions',
            );
          }
        }
      }
    }
  } catch (_) {
    if (created && await destination.exists()) {
      await destination.delete(recursive: true);
    }
    rethrow;
  }
}

/// Rechecks a cached runtime without following links or changing permissions.
///
/// The caller separately checks the manifest's pinned SHA-256. After Pub setup,
/// [allowGenerated] permits files below the adapter's `.dart_tool/` directory.
Future<void> verifyRuntimeManifest(
  Directory runtime, {
  required String version,
  required String target,
  bool allowGenerated = false,
}) async {
  if (await FileSystemEntity.type(runtime.path, followLinks: false) !=
      FileSystemEntityType.directory) {
    _invalid('runtime is not a regular directory');
  }
  final entries = <String, _Entry>{};
  final names = <String>{};
  var size = 0;
  var count = 0;
  await for (final entity in runtime.list(
    recursive: true,
    followLinks: false,
  )) {
    if (++count > _maxEntries) _invalid('too many cached runtime entries');
    final type = await FileSystemEntity.type(entity.path, followLinks: false);
    if (type != FileSystemEntityType.directory &&
        type != FileSystemEntityType.file) {
      _invalid('cached runtime contains a link or special file');
    }
    final relative = p
        .split(p.relative(entity.path, from: runtime.path))
        .join('/');
    _safePath(relative);
    if (!names.add(relative.toLowerCase())) {
      _invalid('duplicate cached runtime path');
    }
    if (allowGenerated &&
        (relative == 'adapters/dart/.dart_tool' ||
            relative.startsWith('adapters/dart/.dart_tool/'))) {
      continue;
    }
    if (type == FileSystemEntityType.directory) {
      entries[relative] = _Entry(relative, true, Uint8List(0));
    } else {
      final file = File(entity.path);
      size += await file.length();
      if (size > _maxUnpackedBytes) _invalid('cached runtime exceeds 512 MiB');
      final data = await file.readAsBytes();
      entries[relative] = _Entry(relative, false, data);
    }
  }
  final manifest = _verifyManifest(
    entries,
    'failforge-$version-$target',
    version,
    target,
  );
  if (!Platform.isWindows) {
    for (final row in manifest.entries) {
      final file = File(p.joinAll([runtime.path, ...row.key.split('/')]));
      final executable = (await file.stat()).mode & 0x49 != 0;
      if (executable != row.value['executable']) {
        _invalid('cached runtime permissions differ: ${row.key}');
      }
    }
  }
}

class _Entry {
  _Entry(this.path, this.directory, this.bytes);

  final String path;
  final bool directory;
  final Uint8List bytes;
}

Never _invalid(String message) => throw RuntimeArchiveException(message);

String _text(Uint8List bytes, int offset, int length) {
  final value = bytes.sublist(offset, offset + length);
  final end = value.indexOf(0);
  try {
    return utf8.decode(end < 0 ? value : value.sublist(0, end));
  } on FormatException {
    return _invalid('invalid UTF-8 in tar header');
  }
}

int _octal(Uint8List bytes, int offset, int length) {
  final raw = _text(bytes, offset, length).trim();
  if (raw.isEmpty) return 0;
  if (!RegExp(r'^[0-7]+$').hasMatch(raw)) {
    _invalid('invalid numeric tar header');
  }
  return int.parse(raw, radix: 8);
}

String _safePath(String name, {bool directory = false}) {
  if (directory && name.endsWith('/')) {
    name = name.substring(0, name.length - 1);
  }
  if (name.isEmpty ||
      name.contains('\\') ||
      name.contains(':') ||
      RegExp(r'[\x00-\x1f\x7f]').hasMatch(name)) {
    _invalid('unsafe archive path');
  }
  for (final component in name.split('/')) {
    if (component.isEmpty ||
        component == '.' ||
        component == '..' ||
        component.endsWith('.') ||
        component.endsWith(' ') ||
        RegExp(
          r'^(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\.|$)',
          caseSensitive: false,
        ).hasMatch(component)) {
      _invalid('unsafe archive path: $name');
    }
  }
  return name;
}

Map<String, String> _pax(Uint8List bytes) {
  final fields = <String, String>{};
  var offset = 0;
  while (offset < bytes.length) {
    var space = offset;
    while (space < bytes.length && bytes[space] != 32) {
      if (bytes[space] < 48 || bytes[space] > 57) {
        _invalid('invalid PAX length');
      }
      space++;
    }
    if (space == offset || space >= bytes.length) {
      _invalid('invalid PAX header');
    }
    final length = int.tryParse(ascii.decode(bytes.sublist(offset, space)));
    if (length == null ||
        length <= space - offset + 2 ||
        offset + length > bytes.length) {
      _invalid('invalid PAX record size');
    }
    final end = offset + length;
    if (bytes[end - 1] != 10) _invalid('unterminated PAX record');
    String record;
    try {
      record = utf8.decode(bytes.sublist(space + 1, end - 1));
    } on FormatException {
      return _invalid('invalid UTF-8 in PAX header');
    }
    final equal = record.indexOf('=');
    if (equal < 1) _invalid('invalid PAX record');
    final key = record.substring(0, equal);
    if (fields.containsKey(key)) _invalid('duplicate PAX key');
    if (key == 'linkpath' || key.startsWith('GNU.sparse')) {
      _invalid('links and sparse tar files are unsupported');
    }
    fields[key] = record.substring(equal + 1);
    offset = end;
  }
  return fields;
}

Map<String, _Entry> _readTar(Uint8List bytes, String root) {
  final entries = <String, _Entry>{};
  final names = <String>{};
  var offset = 0;
  var count = 0;
  Map<String, String>? pendingPax;
  String? longName;
  var terminated = false;
  while (offset + _blockSize <= bytes.length) {
    final header = Uint8List.sublistView(bytes, offset, offset + _blockSize);
    if (header.every((byte) => byte == 0)) {
      if (bytes.length - offset < _blockSize * 2 ||
          bytes.sublist(offset).any((byte) => byte != 0)) {
        _invalid('invalid tar end marker');
      }
      terminated = true;
      break;
    }
    if (++count > _maxEntries) _invalid('too many archive entries');
    var sum = 0;
    for (var index = 0; index < header.length; index++) {
      sum += index >= 148 && index < 156 ? 32 : header[index];
    }
    if (sum != _octal(header, 148, 8)) _invalid('tar header checksum differs');
    final type = header[156];
    var size = _octal(header, 124, 12);
    if (type != 120 && type != 76 && pendingPax?['size'] != null) {
      size = int.tryParse(pendingPax!['size']!) ?? -1;
    }
    if (size < 0 || size > _maxUnpackedBytes) {
      _invalid('invalid tar entry size');
    }
    offset += _blockSize;
    final paddedSize = ((size + _blockSize - 1) ~/ _blockSize) * _blockSize;
    if (offset + paddedSize > bytes.length) _invalid('truncated tar entry');
    final content = Uint8List.sublistView(bytes, offset, offset + size);
    offset += paddedSize;
    if (type == 120) {
      if (pendingPax != null) _invalid('repeated PAX header');
      pendingPax = _pax(content);
      continue;
    }
    if (type == 76) {
      if (longName != null || size == 0 || content.last != 0) {
        _invalid('invalid GNU long path');
      }
      try {
        longName = utf8.decode(content.sublist(0, size - 1));
      } on FormatException {
        _invalid('invalid UTF-8 in long path');
      }
      continue;
    }
    if (type != 0 && type != 48 && type != 53) {
      _invalid('only regular files and directories are supported');
    }
    final directory = type == 53;
    if (directory && size != 0) _invalid('directory contains file data');
    final prefix = _text(header, 345, 155);
    var name = _text(header, 0, 100);
    if (prefix.isNotEmpty) name = '$prefix/$name';
    name = _safePath(
      pendingPax?['path'] ?? longName ?? name,
      directory: directory,
    );
    pendingPax = null;
    longName = null;
    if (!names.add(name.toLowerCase())) {
      _invalid('duplicate archive path: $name');
    }
    if (name == root) {
      if (!directory) _invalid('archive root must be a directory');
      continue;
    }
    if (!name.startsWith('$root/')) _invalid('unexpected archive root');
    final relative = name.substring(root.length + 1);
    final folded = relative.toLowerCase();
    for (final existing in entries.values) {
      final existingFolded = existing.path.toLowerCase();
      if ((!existing.directory && folded.startsWith('$existingFolded/')) ||
          (!directory && existingFolded.startsWith('$folded/'))) {
        _invalid('file and directory paths conflict');
      }
    }
    entries[relative] = _Entry(relative, directory, content);
  }
  if (!terminated || pendingPax != null || longName != null) {
    _invalid('incomplete tar archive');
  }
  return entries;
}

Map<String, Map<String, dynamic>> _verifyManifest(
  Map<String, _Entry> entries,
  String root,
  String version,
  String target,
) {
  final entry = entries['package-manifest.json'];
  if (entry == null ||
      entry.directory ||
      entry.bytes.length > 4 * 1024 * 1024) {
    _invalid('missing or oversized package manifest');
  }
  dynamic manifest;
  try {
    manifest = jsonDecode(utf8.decode(entry.bytes));
  } on FormatException {
    _invalid('invalid package manifest JSON');
  }
  if (manifest is! Map<String, dynamic> ||
      manifest['schemaVersion'] != 1 ||
      manifest['version'] != version ||
      manifest['target'] != target ||
      manifest['archiveRoot'] != root ||
      manifest['files'] is! List) {
    _invalid('package manifest does not match this release');
  }
  final listed = <String, Map<String, dynamic>>{};
  for (final dynamic row in manifest['files'] as List) {
    if (row is! Map<String, dynamic> ||
        row['path'] is! String ||
        row['bytes'] is! int ||
        row['bytes'] < 0 ||
        row['sha256'] is! String ||
        !RegExp(r'^[0-9a-f]{64}$').hasMatch(row['sha256'] as String) ||
        row['executable'] is! bool) {
      _invalid('invalid manifest file record');
    }
    final name = _safePath(row['path'] as String);
    if (name == 'package-manifest.json' || listed.containsKey(name)) {
      _invalid('duplicate or recursive manifest file record');
    }
    final actual = entries[name];
    if (actual == null ||
        actual.directory ||
        actual.bytes.length != row['bytes'] ||
        sha256.convert(actual.bytes).toString() != row['sha256']) {
      _invalid('payload differs from manifest: $name');
    }
    listed[name] = row;
  }
  for (final actual in entries.values) {
    if (!actual.directory &&
        actual.path != 'package-manifest.json' &&
        !listed.containsKey(actual.path)) {
      _invalid('payload file omitted from manifest: ${actual.path}');
    }
  }
  final suffix = target.endsWith('-windows-msvc') ? '.exe' : '';
  for (final name in [
    'bin/failforge$suffix',
    'bin/failforge-adapter-campaign$suffix',
    'adapters/dart/pubspec.yaml',
    'adapters/dart/pubspec.lock',
    'adapters/dart/bin/failforge_dart_adapter.dart',
    'adapters/dart/lib/failforge_dart_adapter.dart',
    'LICENSE',
    'NOTICE',
  ]) {
    if (!listed.containsKey(name)) {
      _invalid('missing required runtime file: $name');
    }
  }
  for (final name in [
    'bin/failforge$suffix',
    'bin/failforge-adapter-campaign$suffix',
  ]) {
    if (listed[name]!['executable'] != true) {
      _invalid('native executable lacks executable permission: $name');
    }
  }
  return listed;
}

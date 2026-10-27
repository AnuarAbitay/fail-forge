import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:path/path.dart' as p;

import '../patches/patch.dart';
import '../protocol/adapter_error.dart';
import '../scope/scope_rules.dart';
import 'source_file.dart';

class SafeProject {
  SafeProject._(
    this.workspaceRoot,
    this.packageRoot,
    this.dartSdkPath,
    this.flutterSdkPath,
    this.packageConfigPath,
    this.analysisOptionsPath,
    this._dependencySourceRoots,
  );

  factory SafeProject.open(Map<String, Object?> payload) {
    final workspace = _directory(
      jsonString(payload['workspaceRoot'], 'workspaceRoot'),
      'workspace',
    );
    final relativePackage = canonicalTarget(
      jsonString(payload['packageRoot'], 'packageRoot'),
      allowDot: true,
    );
    final package = _directory(p.join(workspace, relativePackage), 'package');
    if (!_within(workspace, package)) {
      throw AdapterException(
        'unsafe_path',
        'package root escapes the workspace',
      );
    }
    final sdk = _directory(
      jsonString(payload['dartSdkPath'], 'dartSdkPath'),
      'Dart SDK',
    );
    _readableFile(p.join(sdk, 'lib', 'core', 'core.dart'), 'sdk_missing');
    _readableFile(
      p.join(sdk, 'bin', Platform.isWindows ? 'dart.exe' : 'dart'),
      'sdk_missing',
    );
    if (_within(sdk, package)) {
      throw AdapterException(
        'unsafe_path',
        'the SDK cannot be selected as a mutation package',
      );
    }
    String? flutter;
    String? flutterPackage;
    if (payload.containsKey('flutterSdkPath')) {
      flutter = _directory(
        jsonString(payload['flutterSdkPath'], 'flutterSdkPath'),
        'Flutter SDK',
      );
      _readableFile(
        p.join(flutter, 'bin', Platform.isWindows ? 'flutter.bat' : 'flutter'),
        'sdk_missing',
      );
      flutterPackage = _directory(
        p.join(flutter, 'packages', 'flutter'),
        'Flutter SDK package',
      );
      _readableFile(p.join(flutterPackage, 'pubspec.yaml'), 'sdk_missing');
      final bundledSdk = _directory(
        p.join(flutter, 'bin', 'cache', 'dart-sdk'),
        'Flutter Dart SDK',
      );
      _readableFile(
        p.join(bundledSdk, 'lib', 'core', 'core.dart'),
        'sdk_missing',
      );
      if (sdk != bundledSdk) {
        throw AdapterException(
          'sdk_mismatch',
          'Flutter analysis requires the selected Flutter installation bundled Dart SDK',
        );
      }
      if (_within(flutter, package)) {
        throw AdapterException(
          'unsafe_path',
          'the Flutter SDK cannot be selected as a mutation package',
        );
      }
      _readableFile(
        p.join(
          flutter,
          'packages',
          'flutter',
          'lib',
          'src',
          'services',
          'platform_channel.dart',
        ),
        'sdk_missing',
      );
    }
    _readableFile(p.join(package, 'pubspec.yaml'), 'pubspec_missing');
    var current = package;
    String? config;
    String? options;
    while (_within(workspace, current)) {
      final candidate = p.join(current, '.dart_tool', 'package_config.json');
      if (config == null && File(candidate).existsSync()) config = candidate;
      final optionCandidate = p.join(current, 'analysis_options.yaml');
      if (options == null && File(optionCandidate).existsSync()) {
        _readableFile(optionCandidate, 'analysis_options_unreadable');
        options = optionCandidate;
      }
      if (current == workspace) break;
      current = p.dirname(current);
    }
    if (config == null) {
      throw AdapterException(
        'package_config_missing',
        'run dependency resolution before creating the snapshot',
      );
    }
    _readableFile(config, 'package_config_unreadable');
    if (File(config).lengthSync() > 8 * 1024 * 1024) {
      throw AdapterException(
        'package_config_invalid',
        'package configuration exceeds its byte limit',
      );
    }
    Map<String, Object?> parsed;
    try {
      parsed = jsonObject(
        jsonDecode(File(config).readAsStringSync()),
        'package configuration',
      );
    } on FormatException {
      throw AdapterException(
        'package_config_invalid',
        'package configuration is not valid JSON',
      );
    }
    if (parsed['configVersion'] != 2 || parsed['packages'] is! List<Object?>) {
      throw AdapterException(
        'package_config_invalid',
        'package configuration version 2 is required',
      );
    }
    final packages = parsed['packages'] as List<Object?>;
    if (packages.length > 10000) {
      throw AdapterException(
        'package_config_invalid',
        'package configuration exceeds its package limit',
      );
    }
    final configUri = Uri.file(config);
    final dependencies = <String>[];
    var selectedPackagePresent = false;
    var flutterPackagePresent = false;
    final names = <String>{};
    for (final raw in packages) {
      final entry = jsonObject(raw, 'package configuration entry');
      final name = jsonString(entry['name'], 'package name');
      if (!names.add(name)) {
        throw AdapterException(
          'package_config_invalid',
          'duplicate package name',
        );
      }
      var rootUri = configUri.resolve(
        jsonString(entry['rootUri'], 'package rootUri'),
      );
      // Package-config directory roots commonly omit the final slash. URI
      // resolution must still interpret packageUri relative to that directory.
      if (!rootUri.path.endsWith('/'))
        rootUri = rootUri.replace(path: '${rootUri.path}/');
      final packageUri = rootUri.resolve(
        entry.containsKey('packageUri')
            ? jsonString(entry['packageUri'], 'packageUri')
            : '',
      );
      if (rootUri.scheme != 'file' || packageUri.scheme != 'file') {
        throw AdapterException(
          'package_config_invalid',
          'package resources must be local files',
        );
      }
      final root = _directory(rootUri.toFilePath(), 'dependency');
      final library = _directory(packageUri.toFilePath(), 'dependency library');
      if (name == 'flutter' && flutterPackage != null) {
        flutterPackagePresent = true;
        final trustedLibrary = _directory(
          p.join(flutterPackage, 'lib'),
          'Flutter SDK library',
        );
        if (root != flutterPackage || library != trustedLibrary) {
          throw AdapterException(
            'sdk_mismatch',
            'package:flutter must resolve to the selected Flutter SDK package and lib directory',
          );
        }
      }
      if (entry.containsKey('languageVersion') &&
          !RegExp(
            r'^\d+\.\d+$',
          ).hasMatch(jsonString(entry['languageVersion'], 'languageVersion'))) {
        throw AdapterException(
          'package_config_invalid',
          'package languageVersion is malformed',
        );
      }
      if (root == package) {
        selectedPackagePresent = true;
      } else {
        dependencies.add(library);
      }
    }
    if (!selectedPackagePresent) {
      throw AdapterException(
        'package_config_invalid',
        'selected package is missing from package configuration',
      );
    }
    if (flutterPackage != null && !flutterPackagePresent) {
      throw AdapterException(
        'package_config_invalid',
        'selected Flutter SDK is absent from package configuration',
      );
    }
    return SafeProject._(
      workspace,
      package,
      sdk,
      flutter,
      config,
      options,
      dependencies,
    );
  }

  final String workspaceRoot;
  final String packageRoot;
  final String dartSdkPath;
  final String? flutterSdkPath;
  final String packageConfigPath;
  final String? analysisOptionsPath;
  final List<String> _dependencySourceRoots;
  final List<Map<String, Object?>> diagnostics = [];

  String targetPath(String relative) {
    canonicalTarget(relative);
    if (!relative.endsWith('.dart') ||
        ScopeRules.fromJson({}).isExcluded(relative)) {
      throw AdapterException(
        'unsafe_target',
        'mutation target is excluded or is not a Dart file',
      );
    }
    var component = packageRoot;
    for (final segment in relative.split('/')) {
      component = p.join(component, segment);
      if (FileSystemEntity.typeSync(component, followLinks: false) ==
          FileSystemEntityType.link) {
        throw AdapterException(
          'unsafe_path',
          'mutation target paths cannot contain symlinks',
        );
      }
    }
    final lexical = p.join(
      packageRoot,
      p.posix.split(relative).join(p.separator),
    );
    String canonical;
    try {
      canonical = File(lexical).resolveSymbolicLinksSync();
    } on FileSystemException {
      throw AdapterException(
        'target_unreadable',
        'mutation target is missing or unreadable',
      );
    }
    if (!_within(packageRoot, canonical) ||
        _within(dartSdkPath, canonical) ||
        (flutterSdkPath != null && _within(flutterSdkPath!, canonical)) ||
        _dependencySourceRoots.any((root) => _within(root, canonical))) {
      throw AdapterException(
        'unsafe_path',
        'mutation target escapes the selected package or enters a dependency',
      );
    }
    return canonical;
  }

  SourceFile readTarget(String relative) {
    final path = targetPath(relative);
    try {
      final file = File(path);
      if (file.lengthSync() > SourceFile.maxSourceBytes) {
        throw AdapterException(
          'source_too_large',
          'source exceeds its byte limit',
        );
      }
      final handle = file.openSync();
      final bytes = BytesBuilder(copy: false);
      try {
        while (bytes.length <= SourceFile.maxSourceBytes) {
          final chunk = handle.readSync(
            SourceFile.maxSourceBytes + 1 - bytes.length,
          );
          if (chunk.isEmpty) break;
          bytes.add(chunk);
        }
      } finally {
        handle.closeSync();
      }
      return SourceFile(relative, path, bytes.takeBytes());
    } on FileSystemException {
      throw AdapterException(
        'target_unreadable',
        'mutation target cannot be read',
      );
    }
  }

  List<SourceFile> discoverFiles({List<String>? files, ScopeRules? rules}) {
    final scope = rules ?? ScopeRules.fromJson({});
    final names = <String>{};
    if (files != null) {
      if (files.length > 2000) {
        throw AdapterException(
          'target_limit_exceeded',
          'target file list exceeds its limit',
        );
      }
      for (final file in files) {
        canonicalTarget(file);
        // Validate containment even if a rule would otherwise skip this name.
        targetPath(file);
        names.add(file);
      }
    } else {
      final lib = Directory(p.join(packageRoot, 'lib'));
      if (!lib.existsSync()) {
        throw AdapterException(
          'targets_missing',
          'selected package has no lib directory',
        );
      }
      final pending = [lib.path];
      final seenDirectories = <String>{};
      var visited = 0;
      while (pending.isNotEmpty) {
        final directory = Directory(pending.removeLast());
        if (FileSystemEntity.typeSync(directory.path, followLinks: false) ==
            FileSystemEntityType.link) {
          throw AdapterException(
            'unsafe_path',
            'mutation target directories cannot be symlinks',
          );
        }
        String canonical;
        try {
          canonical = directory.resolveSymbolicLinksSync();
        } on FileSystemException {
          throw AdapterException(
            'target_unreadable',
            'target directory cannot be read',
          );
        }
        if (!_within(packageRoot, canonical)) {
          throw AdapterException(
            'unsafe_path',
            'target directory symlink escapes the selected package',
          );
        }
        if (!seenDirectories.add(canonical)) continue;
        List<FileSystemEntity> entries;
        try {
          entries = directory.listSync(followLinks: false);
        } on FileSystemException {
          throw AdapterException(
            'target_unreadable',
            'target directory cannot be enumerated',
          );
        }
        for (final entry in entries) {
          if (++visited > 100000) {
            throw AdapterException(
              'target_limit_exceeded',
              'target directory scan exceeds its entry limit',
            );
          }
          final relative = p
              .relative(entry.path, from: packageRoot)
              .split(p.separator)
              .join('/');
          if (scope.isExcluded(relative)) continue;
          final type = FileSystemEntity.typeSync(
            entry.path,
            followLinks: false,
          );
          if (type == FileSystemEntityType.link) {
            throw AdapterException(
              'unsafe_path',
              'mutation target paths cannot contain symlinks',
            );
          }
          if (type == FileSystemEntityType.directory) {
            pending.add(entry.path);
          } else if (type == FileSystemEntityType.file &&
              relative.endsWith('.dart')) {
            targetPath(relative);
            names.add(relative);
            if (names.length > 2000) {
              throw AdapterException(
                'target_limit_exceeded',
                'target file count exceeds its limit',
              );
            }
          } else if (entry is Link) {
            throw AdapterException(
              'target_unreadable',
              'target symlink is dangling or unsupported',
            );
          }
        }
      }
    }
    final sorted = names.toList()..sort();
    final result = <SourceFile>[];
    final eligible = <String>[];
    var totalBytes = 0;
    for (final name in sorted) {
      if (!scope.includesFile(name)) {
        diagnostics.add({'code': 'excluded_target', 'file': name});
        continue;
      }
      if (scope.isGenerated(name, '')) {
        diagnostics.add({'code': 'generated_target', 'file': name});
        continue;
      }
      try {
        final size = File(targetPath(name)).lengthSync();
        if (size > SourceFile.maxSourceBytes) {
          throw AdapterException(
            'source_too_large',
            'source exceeds its byte limit',
          );
        }
        totalBytes += size;
      } on FileSystemException {
        throw AdapterException(
          'target_unreadable',
          'mutation target cannot be inspected',
        );
      }
      if (totalBytes > 64 * 1024 * 1024) {
        throw AdapterException(
          'target_limit_exceeded',
          'aggregate source bytes exceed their limit',
        );
      }
      eligible.add(name);
    }
    for (final name in eligible) {
      final source = readTarget(name);
      if (scope.isGenerated(name, source.text)) {
        diagnostics.add({'code': 'generated_target', 'file': name});
        continue;
      }
      result.add(source);
    }
    return result;
  }
}

bool _within(String root, String child) =>
    root == child || p.isWithin(root, child);

String _directory(String path, String kind) {
  if (!p.isAbsolute(path) || path.contains('\u0000')) {
    throw AdapterException('unsafe_path', '$kind path must be absolute');
  }
  try {
    final directory = Directory(path);
    final canonical = directory.resolveSymbolicLinksSync();
    if (!Directory(canonical).existsSync()) {
      throw AdapterException('resource_missing', '$kind directory is missing');
    }
    directory.listSync(followLinks: false);
    return canonical;
  } on FileSystemException {
    throw AdapterException(
      kind.contains('SDK') ? 'sdk_missing' : 'resource_missing',
      '$kind directory is missing or unreadable',
    );
  }
}

void _readableFile(String path, String code) {
  try {
    final file = File(path).openSync();
    file.closeSync();
  } on FileSystemException {
    throw AdapterException(
      code,
      'required project or SDK resource is missing or unreadable',
    );
  }
}

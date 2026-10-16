import 'dart:convert';
import 'dart:io';

import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:crypto/crypto.dart';
import 'package:path/path.dart' as p;

import '../analysis/analysis_workspace.dart';
import '../patches/patch.dart';
import '../project/safe_project.dart';
import '../project/source_file.dart';
import '../protocol/adapter_error.dart';

/// Read-only, source-bound prefix edits for actual SDK package URI resolvers.
/// Headers and any unrecognized body literals remain the caller's responsibility.
Future<Map<String, Object?>> planNamespace(Map<String, Object?> payload) async {
  onlyKeys(payload, {
    'workspaceRoot',
    'packageRoot',
    'dartSdkPath',
    'flutterSdkPath',
    'packageName',
    'alias',
    'files',
  }, 'namespace payload');
  final own = jsonString(payload['packageName'], 'packageName');
  final alias = jsonString(payload['alias'], 'alias');
  final packageName = RegExp(r'^[a-z_][a-z0-9_]*$');
  if (own.length > 100 ||
      alias.length > 100 ||
      !packageName.hasMatch(own) ||
      !packageName.hasMatch(alias) ||
      own == alias) {
    throw AdapterException(
      'invalid_payload',
      'Namespace package names must be distinct canonical names.',
    );
  }
  final files = jsonStrings(payload['files'], 'files');
  if (files.length > 2000 || files.toSet().length != files.length) {
    throw AdapterException(
      'invalid_payload',
      'Namespace files must be unique and at most 2000.',
    );
  }
  final project = SafeProject.open(payload);
  final configFile = File(project.packageConfigPath);
  final configBytes = configFile.readAsBytesSync();
  final configHash = sha256.convert(configBytes).toString();
  final config = jsonObject(
    jsonDecode(utf8.decode(configBytes)),
    'package configuration',
  );
  final packages = (config['packages'] as List).cast<Map>();
  final selected = packages.where((entry) => entry['name'] == own).toList();
  if (selected.length != 1 || packages.any((entry) => entry['name'] == alias)) {
    throw AdapterException(
      'invalid_payload',
      'Namespace package names do not identify one unaliased project.',
    );
  }
  final ownRoot = File(project.packageConfigPath).uri
      .resolve(selected.single['rootUri'] as String);
  if (ownRoot.scheme != 'file' ||
      Directory(ownRoot.toFilePath()).resolveSymbolicLinksSync() !=
          project.packageRoot) {
    throw AdapterException(
      'invalid_payload',
      'The selected package name must identify this project.',
    );
  }
  final sources = <SourceFile>[];
  var totalBytes = 0;
  for (final name in files..sort()) {
    final source = _readSource(project, name);
    totalBytes += source.rawBytes.length;
    if (totalBytes > 64 * 1024 * 1024) {
      throw AdapterException(
        'source_too_large',
        'Namespace source inventory exceeds 64 MiB.',
      );
    }
    sources.add(source);
  }
  final workspace = AnalysisWorkspace.open(project, sources);
  final results = <Map<String, Object?>>[];
  final unsupported = <Map<String, Object?>>[];
  var totalEdits = 0;
  try {
    for (final source in sources) {
      final library = await workspace.library(source);
      if (workspace.hasErrors(library)) {
        throw AdapterException(
          'namespace_analysis_errors',
          'Namespace planning requires resolved, valid author sources.',
          details: {'file': source.relativePath},
        );
      }
      final unit = library.unitWithPath(source.absolutePath);
      if (unit == null || unit.content != source.analysisText) {
        throw AdapterException(
          'source_changed',
          'Analyzer content differs from the bound namespace source.',
        );
      }
      final collector = _RuntimeUris(source, own, alias);
      unit.unit.accept(collector);
      totalEdits += collector.edits.length;
      if (totalEdits > 10000) {
        throw AdapterException(
          'namespace_limit_exceeded',
          'Namespace aggregate edit limit exceeded.',
        );
      }
      unsupported.addAll(collector.unsupported);
      if (unsupported.length > 1000) {
        throw AdapterException(
          'namespace_limit_exceeded',
          'Namespace unsupported-site limit exceeded.',
        );
      }
      results.add({
        'file': source.relativePath,
        'sourceHash': source.sourceHash,
        'edits': collector.edits,
      });
    }
    for (final source in sources) {
      if (_readSource(project, source.relativePath).sourceHash !=
          source.sourceHash) {
        throw AdapterException(
          'source_changed',
          'Namespace source changed during analysis.',
        );
      }
    }
    if (sha256.convert(configFile.readAsBytesSync()).toString() != configHash) {
      throw AdapterException(
        'source_changed',
        'Prepared package configuration changed during namespace planning.',
      );
    }
    return {
      'version': 1,
      'kind': 'dart_namespace_plan',
      'packageName': own,
      'alias': alias,
      'packageConfigHash': configHash,
      'files': results,
      'unsupported': unsupported,
    };
  } finally {
    await workspace.dispose();
  }
}

SourceFile _readSource(SafeProject project, String relative) {
  canonicalTarget(relative);
  if ((!relative.startsWith('test/') && !relative.startsWith('lib/')) ||
      !relative.endsWith('.dart')) {
    throw AdapterException(
      'unsafe_path',
      'Namespace files must be Dart sources in test/ or lib/.',
    );
  }
  var path = project.packageRoot;
  for (final segment in relative.split('/')) {
    path = p.join(path, segment);
    if (FileSystemEntity.typeSync(path, followLinks: false) ==
        FileSystemEntityType.link) {
      throw AdapterException(
        'unsafe_path',
        'Namespace sources cannot contain symlinks.',
      );
    }
  }
  if (FileSystemEntity.typeSync(path, followLinks: false) !=
      FileSystemEntityType.file) {
    throw AdapterException(
      'target_unreadable',
      'Namespace source is not a regular file.',
    );
  }
  final handle = File(path).openSync();
  try {
    return SourceFile(
      relative,
      path,
      handle.readSync(SourceFile.maxSourceBytes + 1),
    );
  } finally {
    handle.closeSync();
  }
}

class _RuntimeUris extends RecursiveAstVisitor<void> {
  _RuntimeUris(this.source, this.own, this.alias);
  final SourceFile source;
  final String own;
  final String alias;
  final edits = <Map<String, Object?>>[];
  final unsupported = <Map<String, Object?>>[];

  bool _sdkMethod(
    MethodInvocation node,
    String library,
    String owner,
    Set<String> names,
  ) {
    final element = node.methodName.element;
    final enclosing = element?.enclosingElement;
    return element is MethodElement &&
        element.isStatic &&
        names.contains(element.name) &&
        element.library.uri.toString() == library &&
        enclosing is ClassElement &&
        enclosing.name == owner;
  }

  void _unsupported(AstNode node, String code) {
    if (unsupported.length >= 1000) {
      throw AdapterException(
        'namespace_limit_exceeded',
        'Namespace unsupported-site limit exceeded.',
      );
    }
    unsupported.add({
      'file': source.relativePath,
      'code': code,
      'offsetBytes': source.byteOffset(
        node.offset + (source.text.startsWith('\ufeff') ? 1 : 0),
      ),
      'message': 'An SDK package URI resolver needs a direct canonical Uri.parse string literal.',
    });
  }

  @override
  void visitMethodInvocation(MethodInvocation node) {
    if (_sdkMethod(node, 'dart:isolate', 'Isolate', {
      'resolvePackageUri',
      'resolvePackageUriSync',
    })) {
      _resolver(node);
    }
    super.visitMethodInvocation(node);
  }

  @override
  void visitSimpleIdentifier(SimpleIdentifier node) {
    final element = node.element;
    final enclosing = element?.enclosingElement;
    final parent = node.parent;
    if (element is MethodElement &&
        element.isStatic &&
        {'resolvePackageUri', 'resolvePackageUriSync'}.contains(element.name) &&
        element.library.uri.toString() == 'dart:isolate' &&
        enclosing is ClassElement &&
        enclosing.name == 'Isolate' &&
        !(parent is MethodInvocation && identical(parent.methodName, node))) {
      _unsupported(node, 'indirect_resolver_reference');
    }
    super.visitSimpleIdentifier(node);
  }

  void _resolver(MethodInvocation node) {
    final arguments = node.argumentList.arguments;
    if (arguments.length != 1) {
      _unsupported(node, 'unsupported_resolver_arguments');
      return;
    }
    final parse = arguments.single.argumentExpression.unParenthesized;
    if (parse is! MethodInvocation ||
        !_sdkMethod(parse, 'dart:core', 'Uri', {'parse'}) ||
        parse.argumentList.arguments.length != 1) {
      _unsupported(node, 'indirect_package_uri');
      return;
    }
    final argument =
        parse.argumentList.arguments.single.argumentExpression.unParenthesized;
    if (argument is! SimpleStringLiteral) {
      _unsupported(argument, 'computed_package_uri');
      return;
    }
    final value = argument.value;
    final raw = source.analysisText.substring(
      argument.contentsOffset,
      argument.contentsEnd,
    );
    final uri = Uri.tryParse(value);
    if (raw != value ||
        raw.contains(r'\') ||
        raw.contains('%') ||
        raw.codeUnits.any((value) => value <= 0x20 || value == 0x7f) ||
        uri == null ||
        !uri.hasScheme ||
        uri.toString() != value ||
        !value.startsWith('${uri.scheme}:')) {
      _unsupported(argument, 'noncanonical_package_uri');
      return;
    }
    final prefix = 'package:$own/';
    if (!value.startsWith(prefix)) return;
    if (edits.length >= 10000) {
      throw AdapterException(
        'namespace_limit_exceeded',
        'Namespace edit limit exceeded.',
      );
    }
    final edit = source.editFromAnalyzer(
      argument.contentsOffset,
      prefix.length,
      'package:$alias/',
    );
    if (edit.expected != prefix) {
      throw AdapterException(
        'invalid_range',
        'Namespace edit does not identify the literal package prefix.',
      );
    }
    edits.add({'kind': 'runtime_package_uri', ...edit.toJson()});
  }
}

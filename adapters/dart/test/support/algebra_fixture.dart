import 'dart:convert';
import 'dart:io';

import 'package:analyzer/dart/analysis/analysis_context_collection.dart';
import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/file_system/overlay_file_system.dart';
import 'package:analyzer/file_system/physical_file_system.dart';
import 'package:path/path.dart' as p;

/// A real analyzer context over an untouched temporary package. No pub get or
/// global SDK/settings changes are performed by these tests.
class AlgebraFixture {
  AlgebraFixture._(this.directory, this.path, this.overlay, this.collection);
  final Directory directory;
  final String path;
  final OverlayResourceProvider overlay;
  final AnalysisContextCollection collection;
  var _stamp = 0;

  static Future<AlgebraFixture> open(String source) async {
    final directory = await Directory.systemTemp.createTemp('ff algebra ');
    final file = File(p.join(directory.path, 'lib', 'source.dart'));
    await file.parent.create(recursive: true);
    await file.writeAsString(source);
    await File(p.join(directory.path, 'pubspec.yaml')).writeAsString(
      'name: algebra_fixture\nenvironment:\n  sdk: ">=3.13.0 <3.14.0"\n',
    );
    final config = File(
      p.join(directory.path, '.dart_tool', 'package_config.json'),
    );
    await config.parent.create(recursive: true);
    await config.writeAsString(
      jsonEncode({
        'configVersion': 2,
        'packages': [
          {
            'name': 'algebra_fixture',
            'rootUri': '../',
            'packageUri': 'lib/',
            'languageVersion': '3.13',
          },
        ],
      }),
    );
    final overlay = OverlayResourceProvider(PhysicalResourceProvider.INSTANCE);
    final collection = AnalysisContextCollection(
      includedPaths: [directory.path],
      sdkPath: p.dirname(p.dirname(Platform.resolvedExecutable)),
      resourceProvider: overlay,
    );
    return AlgebraFixture._(directory, file.path, overlay, collection);
  }

  Future<ResolvedUnitResult> resolve(String source) async {
    final context = collection.contextFor(path);
    overlay.setOverlay(path, content: source, modificationStamp: ++_stamp);
    context.changeFile(path);
    await context.applyPendingFileChanges();
    final result = await context.currentSession.getResolvedUnit(path);
    if (result is! ResolvedUnitResult)
      throw StateError('Algebra fixture did not resolve.');
    return result;
  }

  /// Executes a bounded, finite SDK-only test program in an independent file;
  /// operators themselves never run subprocesses.
  Future<List<String>> execute(String source) async {
    final file = File(p.join(directory.path, 'finite_program.dart'));
    await file.writeAsString(source);
    final process = await Process.start(Platform.resolvedExecutable, [
      '--packages=${p.join(directory.path, '.dart_tool', 'package_config.json')}',
      file.path,
    ], workingDirectory: directory.path);
    var exited = false;
    final exitCode = process.exitCode.then((value) {
      exited = true;
      return value;
    });
    final output = process.stdout.transform(utf8.decoder).join();
    final errors = process.stderr.transform(utf8.decoder).join();
    try {
      final result = await Future.wait<Object>([exitCode, output, errors])
          .timeout(const Duration(seconds: 10));
      if (result[0] != 0)
        throw StateError('Finite fixture failed: ${result[2]}');
      return (result[1] as String).trim().split('\n');
    } finally {
      if (!exited) {
        process.kill(ProcessSignal.sigkill);
        await exitCode.timeout(const Duration(seconds: 5));
      }
    }
  }

  Future<void> close() async {
    await collection.dispose();
    await directory.delete(recursive: true);
  }
}

import 'dart:convert';
import 'dart:io';

import 'package:analyzer/dart/analysis/analysis_context_collection.dart';
import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/diagnostic/diagnostic.dart';
import 'package:analyzer/file_system/overlay_file_system.dart';
import 'package:analyzer/file_system/physical_file_system.dart';
import 'package:failforge_dart_adapter/src/operators/common.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

import 'semantic_output.dart';

/// A real resolved SDK-only package, never pub get. Overlay checks do not write
/// the original file. Finite VM probes execute only test-owned source copies.
class ControlFlowFixture {
  ControlFlowFixture._(
    this.directory,
    this.path,
    this.overlay,
    this.collection,
  );
  final Directory directory;
  final String path;
  final OverlayResourceProvider overlay;
  final AnalysisContextCollection collection;
  int stamp = 0;

  static Future<ControlFlowFixture> open(String source) async {
    final directory = await Directory.systemTemp.createTemp(
      'ff classical flow ',
    );
    final path = p.normalize(p.join(directory.path, 'lib/source.dart'));
    await File(path).parent.create(recursive: true);
    await File(path).writeAsBytes(utf8.encode(source));
    await File(p.join(directory.path, 'pubspec.yaml')).writeAsString(
      'name: flow_fixture\nenvironment:\n  sdk: ">=3.13.0 <4.0.0"\n',
    );
    final config = File(
      p.normalize(p.join(directory.path, '.dart_tool/package_config.json')),
    );
    await config.parent.create(recursive: true);
    await config.writeAsString(
      jsonEncode({
        'configVersion': 2,
        'packages': [
          {
            'name': 'flow_fixture',
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
    return ControlFlowFixture._(directory, path, overlay, collection);
  }

  Future<ResolvedUnitResult> resolve(String source) async {
    final context = collection.contextFor(path);
    overlay.setOverlay(path, content: source, modificationStamp: ++stamp);
    context.changeFile(path);
    await context.applyPendingFileChanges();
    final result = await context.currentSession.getResolvedUnit(path);
    if (result is! ResolvedUnitResult)
      throw StateError('Fixture unit not resolved.');
    return result;
  }

  Future<OperatorScan> scan(MutationOperator operator, String source) async {
    final unit = await resolve(source);
    expectNoErrors(unit);
    return operator.discover(unit, unit.content);
  }

  Future<void> validMutants(
    String source,
    Iterable<CandidateDraft> candidates,
  ) async {
    for (final candidate in candidates) {
      expect(candidate.offset, greaterThanOrEqualTo(0));
      expect(candidate.length, greaterThan(0));
      expect(candidate.replacement, isNot(original(source, candidate)));
      expect(candidate.family, MutationFamily.classic);
      expectNoErrors(await resolve(apply(source, candidate)));
      expectNoErrors(await resolve(source));
    }
    expect(await File(path).readAsBytes(), utf8.encode(source));
  }

  Future<List<String>> execute(String source) async {
    final file = File(p.join(directory.path, 'finite_probe.dart'));
    await file.writeAsBytes(utf8.encode(source));
    final process = await Process.start(Platform.resolvedExecutable, [
      '--packages=${p.join(directory.path, '.dart_tool/package_config.json')}',
      file.path,
    ], workingDirectory: directory.path);
    var exited = false;
    final exitCode = process.exitCode.then((code) {
      exited = true;
      return code;
    });
    final output = process.stdout.transform(utf8.decoder).join();
    final errors = process.stderr.transform(utf8.decoder).join();
    try {
      final result = await Future.wait<Object>([exitCode, output, errors])
          .timeout(const Duration(seconds: 10));
      expect(result[0], 0, reason: result[2] as String);
      return semanticStdout(result[1] as String).trim().split('\n');
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

String original(String source, CandidateDraft candidate) =>
    source.substring(candidate.offset, candidate.offset + candidate.length);
String apply(String source, CandidateDraft candidate) => source.replaceRange(
  candidate.offset,
  candidate.offset + candidate.length,
  candidate.replacement,
);
Iterable<dynamic> errors(ResolvedUnitResult unit) => unit.diagnostics.where(
  (diagnostic) => diagnostic.severity == Severity.error,
);
void expectNoErrors(ResolvedUnitResult unit) => expect(
  errors(unit),
  isEmpty,
  reason: unit.diagnostics.map((diagnostic) => diagnostic.message).join('\n'),
);

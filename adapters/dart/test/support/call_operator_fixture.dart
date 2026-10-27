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

import 'project_fixture.dart';
import 'semantic_output.dart';

class CallOperatorFixture {
  CallOperatorFixture._(
    this.package,
    this.path,
    this.original,
    this.overlay,
    this.collection,
  );
  final ProjectFixture package;
  final String path;
  final String original;
  final OverlayResourceProvider overlay;
  final AnalysisContextCollection collection;
  int _stamp = 0;

  static CallOperatorFixture open(String source) {
    final package = ProjectFixture.create({'lib/source.dart': source});
    final path = p.normalize(p.join(package.root, 'lib/source.dart'));
    final overlay = OverlayResourceProvider(PhysicalResourceProvider.INSTANCE);
    overlay.setOverlay(path, content: source, modificationStamp: 0);
    final collection = AnalysisContextCollection(
      includedPaths: [package.root],
      sdkPath: package.sdk,
      resourceProvider: overlay,
    );
    return CallOperatorFixture._(package, path, source, overlay, collection);
  }

  Future<ResolvedUnitResult> resolve([String? source]) async {
    final context = collection.contextFor(path);
    overlay.setOverlay(
      path,
      content: source ?? original,
      modificationStamp: ++_stamp,
    );
    context.changeFile(path);
    await context.applyPendingFileChanges();
    final result = await context.currentSession.getResolvedUnit(path);
    if (result is! ResolvedUnitResult)
      throw StateError('Cannot resolve the real analyzer fixture.');
    return result;
  }

  static void noErrors(ResolvedUnitResult unit) {
    expect(
      unit.diagnostics.where(
        (diagnostic) => diagnostic.severity == Severity.error,
      ),
      isEmpty,
      reason: unit.diagnostics
          .map((diagnostic) => diagnostic.message)
          .join('\n'),
    );
  }

  String before(CandidateDraft candidate) =>
      original.substring(candidate.offset, candidate.offset + candidate.length);
  String apply(CandidateDraft candidate) => original.replaceRange(
    candidate.offset,
    candidate.offset + candidate.length,
    candidate.replacement,
  );

  Future<void> validateAll(Iterable<CandidateDraft> candidates) async {
    final edits = <(int, int, String)>{};
    for (final candidate in candidates) {
      expect(candidate.replacement, isNot(before(candidate)));
      expect(
        edits.add((candidate.offset, candidate.length, candidate.replacement)),
        isTrue,
        reason: 'Duplicate concrete edit',
      );
      final changed = apply(candidate);
      expect(
        changed.substring(0, candidate.offset),
        original.substring(0, candidate.offset),
      );
      expect(
        changed.substring(candidate.offset + candidate.replacement.length),
        original.substring(candidate.offset + candidate.length),
      );
      noErrors(await resolve(changed));
      noErrors(await resolve());
    }
    expect(await File(path).readAsBytes(), utf8.encode(original));
  }

  /// Tests alone execute finite SDK-only programs. The adapter never does so.
  Future<String> execute(String source) async {
    final file = File(p.join(package.root, 'finite_mutant.dart'));
    await file.writeAsString(source);
    final process = await Process.start(Platform.resolvedExecutable, [
      '--packages=${p.join(package.root, '.dart_tool/package_config.json')}',
      file.path,
    ], workingDirectory: package.root);
    var exited = false;
    final exit = process.exitCode.then((value) {
      exited = true;
      return value;
    });
    final stdout = process.stdout.transform(utf8.decoder).join();
    final stderr = process.stderr.transform(utf8.decoder).join();
    try {
      final values = await Future.wait<Object>([exit, stdout, stderr])
          .timeout(const Duration(seconds: 10));
      expect(values[0], 0, reason: values[2] as String);
      return semanticStdout(values[1] as String);
    } finally {
      if (!exited) {
        process.kill(ProcessSignal.sigkill);
        await exit.timeout(const Duration(seconds: 5));
      }
    }
  }

  Future<void> close() async {
    await collection.dispose();
    package.close();
  }
}

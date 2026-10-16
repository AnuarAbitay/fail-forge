import 'dart:convert';

import 'package:analyzer/dart/analysis/analysis_context_collection.dart';
import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/diagnostic/diagnostic.dart';
import 'package:analyzer/error/error.dart';
import 'package:analyzer/file_system/overlay_file_system.dart';
import 'package:analyzer/file_system/physical_file_system.dart';
import 'package:path/path.dart' as p;

import '../patches/patch.dart';
import '../project/safe_project.dart';
import '../project/source_file.dart';
import '../protocol/adapter_error.dart';
import '../model/mutation_family.dart';

/// A request owns its analysis contexts. No hash-only semantic cache survives a
/// request; SDK, dependency and options changes are observed by the next one.
class AnalysisWorkspace {
  final SafeProject project;
  final OverlayResourceProvider overlay;
  final AnalysisContextCollection collection;
  int _stamp = 1;

  AnalysisWorkspace._(this.project, this.overlay, this.collection);

  factory AnalysisWorkspace.open(
    SafeProject project,
    List<SourceFile> sources,
  ) {
    final overlay = OverlayResourceProvider(PhysicalResourceProvider.INSTANCE);
    for (final source in sources) {
      overlay.setOverlay(
        source.absolutePath,
        content: source.text,
        modificationStamp: 0,
      );
    }
    final collection = AnalysisContextCollection(
      includedPaths: [project.packageRoot],
      sdkPath: project.dartSdkPath,
      resourceProvider: overlay,
    );
    return AnalysisWorkspace._(project, overlay, collection);
  }

  Future<ResolvedLibraryResult> library(SourceFile source) async {
    final context = collection.contextFor(source.absolutePath);
    final result = await context.currentSession.getResolvedLibraryContaining(
      source.absolutePath,
    );
    if (result is! ResolvedLibraryResult) {
      throw AdapterException(
        'analysis_unavailable',
        'Cannot resolve the library containing ${source.relativePath}: ${result.runtimeType}',
      );
    }
    return result;
  }

  List<Map<String, Object?>> diagnostics(ResolvedLibraryResult library) => [
    for (final unit in library.units)
      for (final diagnostic in unit.diagnostics)
        {
          'file': p.relative(unit.path, from: project.packageRoot),
          'code': diagnostic.diagnosticCode.lowerCaseUniqueName,
          'severity': diagnostic.severity.name,
          'message': diagnostic.message,
          'offsetUtf16': diagnostic.offset,
          'lengthUtf16': diagnostic.length,
        },
  ];

  bool hasErrors(ResolvedLibraryResult library) => library.units.any(
    (unit) => unit.diagnostics.any(
      (diagnostic) => diagnostic.severity == Severity.error,
    ),
  );

  bool syntaxValid(ResolvedLibraryResult library) => !library.units.any(
    (unit) => unit.diagnostics.any(
      (diagnostic) =>
          diagnostic.diagnosticCode.type == DiagnosticType.SYNTACTIC_ERROR,
    ),
  );

  Future<Map<String, Object?>> validate(
    SourceFile source,
    Mutant mutant,
  ) async {
    final timer = Stopwatch()..start();
    final bytes = mutant.edit.apply(source);
    final baseline = await library(source);
    final baselineDiagnostics = diagnostics(baseline);
    if (hasErrors(baseline)) {
      return {
        'mutantId': mutant.id,
        'applicable': true,
        'valid': false,
        'validation': 'invalid',
        'syntaxValid': syntaxValid(baseline),
        'semanticValid': false,
        'compileChecked': false,
        'reason': 'baseline_analysis_errors',
        'baselineDiagnostics': baselineDiagnostics,
        'diagnostics': <Object?>[],
        'durationMicros': timer.elapsedMicroseconds,
      };
    }
    final context = collection.contextFor(source.absolutePath);
    try {
      overlay.setOverlay(
        source.absolutePath,
        content: utf8.decode(bytes),
        modificationStamp: _stamp++,
      );
      context.changeFile(source.absolutePath);
      await context.applyPendingFileChanges();
      final changed = await library(source);
      final changedDiagnostics = diagnostics(changed);
      final unreachable =
          mutant.family == MutationFamily.languageSpecific &&
          _addsUnreachableDiagnostic(baselineDiagnostics, changedDiagnostics);
      final valid = !hasErrors(changed) && !unreachable;
      final parses = syntaxValid(changed);
      return {
        'mutantId': mutant.id,
        'applicable': true,
        'valid': valid,
        'validation': valid ? 'valid' : 'invalid',
        'syntaxValid': parses,
        'semanticValid': valid,
        'compileChecked': false,
        'baselineDiagnostics': baselineDiagnostics,
        'diagnostics': changedDiagnostics,
        if (unreachable) 'reason': 'new_unreachable_branch',
        'durationMicros': timer.elapsedMicroseconds,
      };
    } finally {
      // Restore exact original text, then invalidate the changed library and
      // its dependents before another candidate can be analyzed.
      overlay.setOverlay(
        source.absolutePath,
        content: source.text,
        modificationStamp: _stamp++,
      );
      context.changeFile(source.absolutePath);
      await context.applyPendingFileChanges();
    }
  }

  Future<void> dispose() => collection.dispose();
}

/// Existing warnings are permitted; a semantic mutation must not add an
/// unreachable branch. Offsets are excluded because an edit can shift them.
bool _addsUnreachableDiagnostic(
  List<Map<String, Object?>> before,
  List<Map<String, Object?>> after,
) {
  const codes = {
    'dead_code',
    'unreachable_switch_case',
    'unreachable_switch_default',
  };
  final available = <String, int>{};
  String? signature(Map<String, Object?> diagnostic) {
    final code = (diagnostic['code'] as String).split('.').last;
    return codes.contains(code)
        ? jsonEncode([diagnostic['file'], code, diagnostic['message']])
        : null;
  }

  for (final diagnostic in before) {
    final key = signature(diagnostic);
    if (key != null) available[key] = (available[key] ?? 0) + 1;
  }
  for (final diagnostic in after) {
    final key = signature(diagnostic);
    if (key == null) continue;
    final remaining = available[key] ?? 0;
    if (remaining == 0) return true;
    available[key] = remaining - 1;
  }
  return false;
}

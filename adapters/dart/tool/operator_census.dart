/// Read-only applicability census before cross-operator/result-byte deduplication.
/// Uses the same resolved AST, scope and byte-coordinate APIs as discovery.
library;

import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:failforge_dart_adapter/src/analysis/adapter_engine.dart'
    show symbolFor;
import 'package:failforge_dart_adapter/src/analysis/analysis_workspace.dart';
import 'package:failforge_dart_adapter/src/operators/registry.dart';
import 'package:failforge_dart_adapter/src/project/safe_project.dart';
import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';
import 'package:failforge_dart_adapter/src/scope/scope_rules.dart';

Future<void> main(List<String> args) async {
  if (args.length != 1) throw ArgumentError('PAYLOAD_JSON_PATH');
  final file = File(args.single);
  if (file.lengthSync() > 8 * 1024 * 1024)
    throw ArgumentError('Payload exceeds protocol input limit.');
  final payload = jsonObject(jsonDecode(file.readAsStringSync()), 'payload');
  final project = SafeProject.open(payload);
  final rules = ScopeRules.fromJson(
    jsonObject(payload['rules'] ?? <String, Object?>{}, 'rules'),
  );
  final files = payload.containsKey('files')
      ? jsonStrings(payload['files'], 'files')
      : null;
  final sources = project.discoverFiles(files: files, rules: rules);
  final workspace = AnalysisWorkspace.open(project, sources);
  final seenLibraries = <String>{};
  final counts = <String, Map<String, Object?>>{
    for (final operator in defaultClassicOperators)
      operator.id: {
        'category': operator.category!.wireName,
        'family': operator.family.wireName,
        'rawFilteredCandidates': 0,
        'diagnosedSkips': 0,
        'scopeRejectedCandidates': 0,
        'unchangedEdits': 0,
        'scannedFiles': 0,
      },
  };
  void increment(Map<String, Object?> row, String field, [int amount = 1]) =>
      row[field] = (row[field] as int) + amount;
  final timer = Stopwatch()..start();
  try {
    for (final source in sources) {
      final library = await workspace.library(source);
      if (seenLibraries.add(library.element.uri.toString()) &&
          workspace.hasErrors(library)) {
        throw AdapterException(
          'baseline_analysis_errors',
          'Census requires resolved baseline libraries.',
          details: {'diagnostics': workspace.diagnostics(library)},
        );
      }
      final unit = library.unitWithPath(source.absolutePath);
      if (unit == null || unit.content != source.analysisText)
        throw AdapterException(
          'snapshot_mismatch',
          'Census AST differs from source bytes.',
        );
      final allowed = rules.operatorIdsForFile(source.relativePath);
      for (final operator in defaultClassicOperators) {
        if (!allowed.contains(operator.id)) continue;
        final row = counts[operator.id]!;
        increment(row, 'scannedFiles');
        final scan = operator.discover(unit, source.analysisText);
        increment(row, 'diagnosedSkips', scan.skips.length);
        for (final draft in scan.candidates) {
          final symbol = symbolFor(draft.anchor);
          final matched = rules.match(
            file: source.relativePath,
            operatorId: draft.operatorId,
            className: symbol.$1,
            methodName: symbol.$2,
          );
          if (!matched.allowed) {
            increment(row, 'scopeRejectedCandidates');
            continue;
          }
          final edit = source.editFromAnalyzer(
            draft.offset,
            draft.length,
            draft.replacement,
          );
          if (edit.expected == edit.replacement) {
            increment(row, 'unchangedEdits');
            continue;
          }
          increment(row, 'rawFilteredCandidates');
        }
      }
    }
    for (final source in sources) {
      if (sha256
              .convert(File(source.absolutePath).readAsBytesSync())
              .toString() !=
          source.sourceHash) {
        throw AdapterException(
          'snapshot_mismatch',
          'A source changed during census.',
        );
      }
    }
    stdout.writeln(
      jsonEncode({
        'recordKind': 'operator_applicability_census',
        'deduplicated': false,
        'sourceFiles': sources.length,
        'libraries': seenLibraries.length,
        'allRawSum': counts.values.fold<int>(
          0,
          (sum, row) => sum + (row['rawFilteredCandidates'] as int),
        ),
        'rawApplicableOperatorIds': counts.entries
            .where((entry) => (entry.value['rawFilteredCandidates'] as int) > 0)
            .map((entry) => entry.key)
            .toList(),
        'zeroRawCandidateOperatorIds': counts.entries
            .where((entry) => entry.value['rawFilteredCandidates'] == 0)
            .map((entry) => entry.key)
            .toList(),
        'operators': counts,
        'sourceHashes': {
          for (final source in sources) source.relativePath: source.sourceHash,
        },
        'sourceFilesUnchanged': true,
        'elapsedMicros': timer.elapsedMicroseconds,
      }),
    );
  } finally {
    await workspace.dispose();
  }
}

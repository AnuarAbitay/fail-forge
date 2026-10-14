import 'dart:convert';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';
import 'package:analyzer/dart/ast/ast.dart';

import '../operators/common.dart';
import '../operators/registry.dart';
import '../patches/patch.dart';
import '../project/safe_project.dart';
import '../project/source_file.dart';
import '../protocol/adapter_error.dart';
import '../scope/scope_rules.dart' hide classicOperators;
import 'analysis_workspace.dart';

const _maxDedupWorkBytes = 256 * 1024 * 1024;

/// Sequential development-tool requests. Test execution belongs to Rust Core.
class AdapterEngine {
  Future<Map<String, Object?>> handle(
    String command,
    Map<String, Object?> payload,
  ) async {
    switch (command) {
      case 'handshake':
        if (payload.isNotEmpty) {
          throw AdapterException(
            'invalid_payload',
            'Handshake payload must be empty.',
          );
        }
        return {
          'adapterVersion': '0.1.0',
          'protocolVersion': 1,
          'operators': [
            for (final group in operatorsByFamily.values)
              for (final operator in group)
                {
                  'id': operator.id,
                  'version': operator.version,
                  'family': operator.family.wireName,
                  if (operator.category != null)
                    'category': operator.category!.wireName,
                  if (operator.isLegacy) 'legacy': true,
                },
          ],
          'operatorTree': {
            'classic': {
              for (final group in operatorsByClassicalCategory.entries)
                group.key.wireName: [
                  for (final operator in group.value)
                    if (!operator.isLegacy) operator.id,
                ],
            },
            'failure_mode': [
              for (final operator in failureModeOperators) operator.id,
            ],
          },
          'limits': {
            'maxLineBytes': 8 * 1024 * 1024,
            'maxMutants': 10000,
            'maxDedupWorkBytes': _maxDedupWorkBytes,
          },
          'validation': 'whole_library_static_analysis',
          'testExecution': false,
        };
      case 'discover':
        return _discover(payload);
      case 'validate':
        return _validate(payload);
      default:
        throw AdapterException(
          'unknown_command',
          'Unsupported command: $command',
        );
    }
  }

  void _checkPayload(Map<String, Object?> payload, {required bool validate}) {
    final allowed = {
      'workspaceRoot',
      'packageRoot',
      'dartSdkPath',
      'flutterSdkPath',
      'files',
      'rules',
      'limits',
      if (validate) 'mutant',
    };
    if (payload.keys.any((key) => !allowed.contains(key))) {
      throw AdapterException(
        'invalid_payload',
        'Unknown request payload field.',
      );
    }
    if (payload.containsKey('rules') && payload['rules'] is! Map) {
      throw AdapterException('invalid_payload', 'rules must be an object.');
    }
    final rules = payload['rules'] as Map? ?? {};
    final scopes = [
      rules,
      if (rules['components'] is List) ...(rules['components'] as List),
    ];
    final known = operatorsByFamily.values
        .expand((operators) => operators)
        .map((operator) => operator.id)
        .toSet();
    for (final scope in scopes) {
      if (scope is! Map) continue;
      final operators = scope['operators'];
      if (operators is List && operators.any((id) => !known.contains(id))) {
        throw AdapterException(
          'unknown_operator',
          'An operator is not supported by this adapter.',
        );
      }
    }
  }

  List<MutationOperator> _operators(
    Map<String, Object?> payload, {
    ScopeRules? rules,
    String? file,
  }) {
    final enabled = (rules ?? _rules(payload)).operatorIdsForFile(file);
    return [
      for (final group in operatorsByFamily.values)
        ...group.where((operator) => enabled.contains(operator.id)),
    ];
  }

  void _checkSdk(SafeProject project, Map<String, Object?> payload) {
    if (_operators(payload)
            .any((operator) => operator.id == 'METHOD_CHANNEL_ERROR') &&
        project.flutterSdkPath == null) {
      throw AdapterException(
        'flutter_sdk_required',
        'METHOD_CHANNEL_ERROR requires an explicitly supplied trusted Flutter SDK.',
      );
    }
  }

  ScopeRules _rules(Map<String, Object?> payload) => ScopeRules.fromJson(
    Map<String, Object?>.from(payload['rules'] as Map? ?? {}),
  );

  int _maxMutants(Map<String, Object?> payload) {
    final limits = payload['limits'];
    if (payload.containsKey('limits') && limits is! Map) {
      throw AdapterException('invalid_payload', 'limits must be an object.');
    }
    final map = limits as Map? ?? {};
    if (map.keys.any((key) => key != 'maxMutants')) {
      throw AdapterException('invalid_payload', 'Unknown limits field.');
    }
    final max = map.containsKey('maxMutants') ? map['maxMutants'] : 1000;
    if (max is! int || max < 0 || max > 10000) {
      throw AdapterException(
        'invalid_payload',
        'maxMutants must be between 0 and 10000.',
      );
    }
    return max;
  }

  List<String>? _files(Map<String, Object?> payload) {
    final files = payload['files'];
    if (!payload.containsKey('files')) return null;
    if (files is! List || files.any((file) => file is! String)) {
      throw AdapterException(
        'invalid_payload',
        'files must be a list of relative paths.',
      );
    }
    return files.cast<String>();
  }

  Future<Map<String, Object?>> _discover(Map<String, Object?> payload) async {
    _checkPayload(payload, validate: false);
    final timer = Stopwatch()..start();
    final project = SafeProject.open(payload);
    _checkSdk(project, payload);
    final rules = _rules(payload);
    final max = _maxMutants(payload);
    final sources = project.discoverFiles(files: _files(payload), rules: rules);
    final workspace = AnalysisWorkspace.open(project, sources);
    try {
      final diagnostics = <Map<String, Object?>>[...project.diagnostics];
      final mutants = <Mutant>[];
      var editUnits = 0;
      final seenLibraries = <String>{};
      for (final source in sources) {
        final library = await workspace.library(source);
        if (seenLibraries.add(library.element.uri.toString())) {
          final baselineDiagnostics = workspace.diagnostics(library);
          diagnostics.addAll(baselineDiagnostics);
          if (workspace.hasErrors(library)) {
            throw AdapterException(
              'baseline_analysis_errors',
              'Resolve baseline analysis errors before discovering mutants.',
              details: {'diagnostics': baselineDiagnostics},
            );
          }
        }
        final unit = library.unitWithPath(source.absolutePath);
        if (unit == null || unit.content != source.analysisText) {
          throw AdapterException(
            'snapshot_mismatch',
            'Analyzer content differs from source bytes.',
          );
        }
        if (diagnostics.length > 10000) {
          throw AdapterException(
            'diagnostic_limit_exceeded',
            'Discovery exceeds its diagnostic budget.',
          );
        }
        for (final operator in _operators(
          payload,
          rules: rules,
          file: source.relativePath,
        )) {
          final scan = operator.discover(unit, source.analysisText);
          for (final skip in scan.skips) {
            if (diagnostics.length >= 10000) {
              throw AdapterException(
                'diagnostic_limit_exceeded',
                'Discovery exceeds its diagnostic budget.',
              );
            }
            diagnostics.add({
              'file': source.relativePath,
              'operatorId': skip.operatorId,
              'code': skip.code,
              'message': skip.message,
              'offsetUtf16': skip.anchor.offset,
              'severity': 'info',
            });
          }
          for (final draft in scan.candidates) {
            final symbol = symbolFor(draft.anchor);
            final matched = rules.match(
              file: source.relativePath,
              operatorId: draft.operatorId,
              className: symbol.$1,
              methodName: symbol.$2,
            );
            if (!matched.allowed) continue;
            if (draft.family == MutationFamily.failureMode &&
                !rules.failureModeExplicitlySelected(
                  file: source.relativePath,
                  operatorId: draft.operatorId,
                  className: symbol.$1,
                  methodName: symbol.$2,
                )) {
              if (diagnostics.length >= 10000)
                throw AdapterException(
                  'diagnostic_limit_exceeded',
                  'Discovery exceeds its diagnostic budget.',
                );
              diagnostics.add({
                'code': 'explicit_scope_required',
                'file': source.relativePath,
                'operatorId': draft.operatorId,
                'severity': 'info',
              });
              continue;
            }
            final edit = source.editFromAnalyzer(
              draft.offset,
              draft.length,
              draft.replacement,
            );
            if (edit.expected == edit.replacement) continue;
            editUnits += edit.expected.length + edit.replacement.length;
            if (mutants.length >= 50000 || editUnits > 16 * 1024 * 1024) {
              throw AdapterException(
                'candidate_limit_exceeded',
                'Discovery exceeds its candidate memory budget.',
              );
            }
            mutants.add(
              Mutant.create(
                file: source.relativePath,
                sourceHash: source.sourceHash,
                operatorId: draft.operatorId,
                operatorVersion: draft.operatorVersion,
                family: draft.family,
                edit: edit,
                components: matched.components,
                className: symbol.$1,
                methodName: symbol.$2,
              ),
            );
          }
        }
      }
      mutants.sort(compareMutants);
      final unique = <String, Mutant>{};
      final sourceByPath = {
        for (final source in sources) source.relativePath: source,
      };
      var dedupWorkBytes = 0;
      for (final mutant in mutants) {
        dedupWorkBytes +=
            sourceByPath[mutant.file]!.rawBytes.length -
            mutant.edit.lengthBytes +
            utf8.encode(mutant.edit.replacement).length;
      }
      if (dedupWorkBytes > _maxDedupWorkBytes) {
        throw AdapterException(
          'candidate_work_limit_exceeded',
          'Discovery deduplication exceeds its work budget; narrow files or operators.',
          details: {
            'maxDedupWorkBytes': _maxDedupWorkBytes,
            'estimatedDedupWorkBytes': dedupWorkBytes,
          },
        );
      }
      for (final mutant in mutants) {
        final key = jsonEncode([
          mutant.family.wireName,
          mutant.file,
          mutant.sourceHash,
          _patchedHash(sourceByPath[mutant.file]!, mutant.edit),
        ]);
        final previous = unique[key];
        if (previous == null) {
          unique[key] = mutant;
        } else {
          final representative = _preferredRepresentative(previous, mutant);
          unique[key] = Mutant.create(
            file: representative.file,
            sourceHash: representative.sourceHash,
            operatorId: representative.operatorId,
            operatorVersion: representative.operatorVersion,
            family: representative.family,
            edit: representative.edit,
            components: {...previous.components, ...mutant.components}.toList()
              ..sort(),
            className: representative.className,
            methodName: representative.methodName,
          );
        }
      }
      final result = unique.values.toList()..sort(compareMutants);
      return {
        'mutants': result.take(max).map((mutant) => mutant.toJson()).toList(),
        'diagnostics': diagnostics,
        'truncated': result.length > max,
        'totalCandidates': result.length,
        'rawCandidates': mutants.length,
        'deduplicatedCandidates': mutants.length - result.length,
        'returnedCandidates': result.length < max ? result.length : max,
        'maxMutants': max,
        'metrics': {
          'discoveryMicros': timer.elapsedMicroseconds,
          'files': sources.length,
          'dedupHashedBytes': dedupWorkBytes,
        },
      };
    } finally {
      await workspace.dispose();
    }
  }

  Future<Map<String, Object?>> _validate(Map<String, Object?> payload) async {
    _checkPayload(payload, validate: true);
    _maxMutants(payload);
    final value = payload['mutant'];
    if (value is! Map)
      throw AdapterException('invalid_payload', 'mutant must be an object.');
    final mutant = Mutant.fromJson(Map<String, Object?>.from(value));
    final project = SafeProject.open(payload);
    _checkSdk(project, payload);
    final rules = _rules(payload);
    final eligible = project.discoverFiles(
      files: _files(payload) ?? [mutant.file],
      rules: rules,
    );
    final selected = eligible
        .where((source) => source.relativePath == mutant.file)
        .toList();
    if (selected.length != 1) {
      throw AdapterException(
        'excluded_target',
        'The mutant file is excluded by target rules.',
      );
    }
    final source = selected.single;
    if (source.sourceHash != mutant.sourceHash) {
      throw AdapterException(
        'source_hash_mismatch',
        'Source bytes changed after discovery.',
      );
    }
    mutant.edit.validateAgainst(source);
    final workspace = AnalysisWorkspace.open(project, eligible);
    try {
      final library = await workspace.library(source);
      final unit = library.unitWithPath(source.absolutePath);
      if (unit == null)
        throw AdapterException(
          'analysis_unavailable',
          'No resolved mutation unit.',
        );
      final operators = _operators(
        payload,
        rules: rules,
        file: source.relativePath,
      ).where((operator) => operator.id == mutant.operatorId);
      var authentic = false;
      for (final operator in operators) {
        if (operator.version != mutant.operatorVersion ||
            operator.family != mutant.family)
          continue;
        for (final draft
            in operator.discover(unit, source.analysisText).candidates) {
          final symbol = symbolFor(draft.anchor);
          final match = rules.match(
            file: source.relativePath,
            operatorId: draft.operatorId,
            className: symbol.$1,
            methodName: symbol.$2,
          );
          if (!match.allowed ||
              (draft.family == MutationFamily.failureMode &&
                  !rules.failureModeExplicitlySelected(
                    file: source.relativePath,
                    operatorId: draft.operatorId,
                    className: symbol.$1,
                    methodName: symbol.$2,
                  )))
            continue;
          final expected = Mutant.create(
            file: source.relativePath,
            sourceHash: source.sourceHash,
            operatorId: draft.operatorId,
            operatorVersion: draft.operatorVersion,
            family: draft.family,
            edit: source.editFromAnalyzer(
              draft.offset,
              draft.length,
              draft.replacement,
            ),
            className: symbol.$1,
            methodName: symbol.$2,
          );
          if (expected.id == mutant.id &&
              expected.className == mutant.className &&
              expected.methodName == mutant.methodName &&
              jsonEncode(match.components) == jsonEncode(mutant.components))
            authentic = true;
        }
      }
      if (!authentic) {
        throw AdapterException(
          'unsupported_candidate',
          'Patch does not match a supported AST mutation.',
        );
      }
      return await workspace.validate(source, mutant);
    } finally {
      await workspace.dispose();
    }
  }
}

(String?, String?) symbolFor(AstNode node) {
  String? className;
  String? methodName;
  for (AstNode? current = node; current != null; current = current.parent) {
    if (current is ClassDeclaration)
      className ??= current.namePart.typeName.lexeme;
    if (current is MethodDeclaration) methodName ??= current.name.lexeme;
    if (current is FunctionDeclaration) methodName ??= current.name.lexeme;
  }
  return (className, methodName);
}

int compareMutants(Mutant a, Mutant b) {
  var comparison = a.file.compareTo(b.file);
  if (comparison != 0) return comparison;
  comparison = a.edit.offsetBytes.compareTo(b.edit.offsetBytes);
  if (comparison != 0) return comparison;
  comparison = a.edit.lengthBytes.compareTo(b.edit.lengthBytes);
  if (comparison != 0) return comparison;
  comparison = a.operatorId.compareTo(b.operatorId);
  if (comparison != 0) return comparison;
  return a.edit.replacement.compareTo(b.edit.replacement);
}

const _originalOperatorIds = {
  'BOOLEAN_LITERAL_FLIP',
  'CONDITIONAL_NEGATION',
  'RELATIONAL_BOUNDARY',
  'LOGICAL_CONNECTOR_SWAP',
};

Mutant _preferredRepresentative(Mutant a, Mutant b) {
  final aOriginal = _originalOperatorIds.contains(a.operatorId);
  final bOriginal = _originalOperatorIds.contains(b.operatorId);
  if (aOriginal != bOriginal) return aOriginal ? a : b;
  return compareMutants(a, b) <= 0 ? a : b;
}

/// Deduplicate identical resulting bytes, including edits with different ranges.
/// Chunked hashing avoids allocating a complete changed file per candidate.
String _patchedHash(SourceFile source, ByteEdit edit) {
  final sink = _DigestSink();
  final hash = sha256.startChunkedConversion(sink);
  hash.add(Uint8List.sublistView(source.rawBytes, 0, edit.offsetBytes));
  hash.add(utf8.encode(edit.replacement));
  hash.add(
    Uint8List.sublistView(source.rawBytes, edit.offsetBytes + edit.lengthBytes),
  );
  hash.close();
  return sink.value!.toString();
}

class _DigestSink implements Sink<Digest> {
  Digest? value;
  @override
  void add(Digest data) => value = data;
  @override
  void close() {}
}

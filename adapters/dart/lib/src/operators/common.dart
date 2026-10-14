import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/nullability_suffix.dart';
import 'package:analyzer/dart/element/type.dart';

import '../model/mutation_family.dart';
import '../model/classical_category.dart';
import '../protocol/adapter_error.dart';

export '../model/mutation_family.dart';
export '../model/classical_category.dart';

/// Operators produce UTF-16 AST ranges. The adapter's source/patch layer owns
/// conversion to UTF-8 byte ranges, hashes, scope filtering and stable IDs.
abstract class MutationOperator {
  const MutationOperator();

  String get id;
  int get version => 1;
  MutationFamily get family;
  ClassicalCategory? get category => null;
  bool get isLegacy => false;

  /// [source] must use the resolved analyzer's coordinate space, normally
  /// [ResolvedUnitResult.content]. A leading physical UTF-8 BOM is excluded;
  /// conversion back to snapshot coordinates belongs to the source layer.
  OperatorScan discover(ResolvedUnitResult result, String source);
}

/// Every classic operator declares its semantic category explicitly.
abstract class ClassicOperator extends MutationOperator {
  const ClassicOperator();
  @override
  MutationFamily get family => MutationFamily.classic;
  @override
  ClassicalCategory get category;
}

class CandidateDraft {
  const CandidateDraft({
    required this.operatorId,
    required this.operatorVersion,
    required this.family,
    required this.anchor,
    required this.offset,
    required this.length,
    required this.replacement,
  });

  final String operatorId;
  final int operatorVersion;
  final MutationFamily family;
  final AstNode anchor;
  final int offset;
  final int length;
  final String replacement;
}

class OperatorSkip {
  const OperatorSkip({
    required this.operatorId,
    required this.anchor,
    required this.code,
    required this.message,
  });

  final String operatorId;
  final AstNode anchor;
  final String code;
  final String message;
}

class OperatorScan {
  OperatorScan({
    required Iterable<CandidateDraft> candidates,
    required Iterable<OperatorSkip> skips,
  }) : candidates = List.unmodifiable(candidates),
       skips = List.unmodifiable(skips);

  final List<CandidateDraft> candidates;
  final List<OperatorSkip> skips;
}

final _dartCoreUri = Uri.parse('dart:core');

bool isSdkInterface(DartType? type, Set<String> names) =>
    type is InterfaceType &&
    type.nullabilitySuffix == NullabilitySuffix.none &&
    type.element.library.uri == _dartCoreUri &&
    names.contains(type.element.name);

bool isSdkBool(DartType? type) => isSdkInterface(type, const {'bool'});

bool isSdkNumeric(DartType? type) =>
    isSdkInterface(type, const {'num', 'int', 'double'});

/// P0 supports expressions inside executable bodies, including closures.
/// Initializers outside a body, defaults, metadata, patterns and constant
/// contexts are deliberately excluded rather than treated as executable code.
String? unsupportedExpressionContext(Expression expression) {
  if (expression.isSynthetic) return 'synthetic_expression';
  for (
    AstNode? current = expression;
    current != null;
    current = current.parent
  ) {
    if (current is Annotation) return 'metadata_context';
    if (current is DartPattern ||
        current is GuardedPattern ||
        current is WhenClause ||
        current is CaseClause) {
      return 'pattern_context';
    }
    if (current is FormalParameterDefaultClause) return 'default_parameter';
    if (current is IfStatement &&
        current.caseClause != null &&
        _contains(current.expression, expression)) {
      return 'pattern_condition';
    }
    if (current is IfElement &&
        current.caseClause != null &&
        _contains(current.expression, expression)) {
      return 'pattern_condition';
    }
    if (current is FunctionBody) {
      return expression.inConstantContext ? 'constant_context' : null;
    }
  }
  return 'non_executable_context';
}

bool _contains(AstNode outer, AstNode inner) =>
    inner.offset >= outer.offset && inner.end <= outer.end;

abstract class OperatorCollector extends RecursiveAstVisitor<void> {
  OperatorCollector(
    this.operator,
    this.source, {
    this.maxItems = 50000,
    this.maxReplacementUnits = 8 * 1024 * 1024,
  });

  final MutationOperator operator;
  final String source;
  final int maxItems;
  final int maxReplacementUnits;
  final candidates = <CandidateDraft>[];
  final skips = <OperatorSkip>[];
  int _replacementUnits = 0;

  OperatorScan get result => OperatorScan(candidates: candidates, skips: skips);

  bool acceptsContext(Expression expression) {
    final reason = unsupportedExpressionContext(expression);
    if (reason == null) return true;
    skip(
      expression,
      reason,
      'The operator excludes this expression context: $reason.',
    );
    return false;
  }

  void skip(AstNode node, String code, String message) {
    _reserveItem();
    skips.add(
      OperatorSkip(
        operatorId: operator.id,
        anchor: node,
        code: code,
        message: message,
      ),
    );
  }

  void replace(AstNode anchor, String replacement, {int? offset, int? length}) {
    final start = offset ?? anchor.offset;
    final count = length ?? anchor.length;
    if (start < 0 || count <= 0 || start > source.length - count) {
      skip(anchor, 'invalid_ast_range', 'The resolved AST range is invalid.');
      return;
    }
    if (source.substring(start, start + count) == replacement) {
      skip(
        anchor,
        'unchanged_replacement',
        'The replacement changes no bytes.',
      );
      return;
    }
    _reserveItem();
    if (replacement.length > maxReplacementUnits - _replacementUnits) {
      throw AdapterException(
        'candidate_limit_exceeded',
        'Operator replacement text exceeds its aggregate UTF-16 limit.',
      );
    }
    _replacementUnits += replacement.length;
    candidates.add(
      CandidateDraft(
        operatorId: operator.id,
        operatorVersion: operator.version,
        family: operator.family,
        anchor: anchor,
        offset: start,
        length: count,
        replacement: replacement,
      ),
    );
  }

  void _reserveItem() {
    if (candidates.length + skips.length >= maxItems) {
      throw AdapterException(
        'candidate_limit_exceeded',
        'Operator candidates and diagnostics exceed their item limit.',
      );
    }
  }
}

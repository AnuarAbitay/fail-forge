import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';
import 'package:analyzer/dart/element/type.dart';
import 'package:analyzer/dart/element/type_system.dart';

/// A replacement refers to this resolved element, never to an identifier with
/// merely the same spelling. Names are used only to resolve shadowing within
/// lexical scopes and to render the already selected element.
class LexicalVariable {
  const LexicalVariable(this.element);
  final VariableElement element;
  String get name => element.name!;
  DartType get type => element.type;
}

/// Conservative definitely-initialized locals and parameters of the nearest
/// function. Captures, loop/catch/pattern bindings, fields and getters are not
/// inferred. Uninitialized/late/later declarations still shadow outer names.
class LexicalScope {
  // Each scan owns this cache. Resolved elements never survive a unit/request.
  final _blocks = <Block, List<_Binding>>{};

  List<LexicalVariable> variablesAt(AstNode anchor) {
    final visible = <LexicalVariable>[];
    final shadowed = <String>{};
    for (AstNode? scope = anchor; scope != null; scope = scope.parent) {
      if (scope is ForStatement ||
          scope is ForElement ||
          scope is CatchClause ||
          scope is SwitchExpression ||
          scope is SwitchStatement ||
          scope is IfStatement && scope.caseClause != null ||
          scope is IfElement && scope.caseClause != null) {
        return const [];
      }
      if (scope is Block) {
        final block = scope;
        final bindings = _blocks.putIfAbsent(block, () {
          final visitor = _BlockBindings();
          for (final statement in block.statements) {
            if (statement is VariableDeclarationStatement ||
                statement is PatternVariableDeclarationStatement ||
                statement is FunctionDeclarationStatement) {
              statement.accept(visitor);
            }
          }
          return visitor.bindings;
        });
        for (final binding in bindings) {
          final name = binding.name;
          if (name == null || !shadowed.add(name)) continue;
          if (name == '_') continue;
          final variable = binding.variable;
          final element = variable?.declaredFragment?.element;
          if (element is LocalVariableElement &&
              !element.isLate &&
              variable!.initializer != null &&
              variable.initializer!.end <= anchor.offset) {
            visible.add(LexicalVariable(element));
          }
        }
      }
      FormalParameterList? parameters;
      var functionBoundary = false;
      if (scope is FunctionExpression) {
        parameters = scope.parameters;
        functionBoundary = true;
      } else if (scope is MethodDeclaration) {
        parameters = scope.parameters;
        functionBoundary = true;
      } else if (scope is ConstructorDeclaration) {
        parameters = scope.parameters;
        functionBoundary = true;
      }
      if (functionBoundary) {
        for (final parameter in parameters?.parameters ?? <FormalParameter>[]) {
          final element = parameter.declaredFragment?.element;
          final name = element?.name;
          if (element != null &&
              name != null &&
              name != '_' &&
              shadowed.add(name) &&
              !element.isLate &&
              parameter is! FieldFormalParameter &&
              parameter is! SuperFormalParameter) {
            visible.add(LexicalVariable(element));
          }
        }
        return visible;
      }
    }
    return const [];
  }
}

/// Subtyping avoids implicit dynamic downcasts. Dynamic can be a declared
/// parameter destination, but an unknown dynamic source is not proof of safety.
bool replacementFits(TypeSystem types, DartType source, DartType target) =>
    source is! DynamicType &&
    source is! InvalidType &&
    target is! InvalidType &&
    types.isSubtypeOf(source, target);

class _Binding {
  const _Binding(this.name, [this.variable]);
  final String? name;
  final VariableDeclaration? variable;
}

class _BlockBindings extends RecursiveAstVisitor<void> {
  final bindings = <_Binding>[];
  @override
  void visitVariableDeclaration(VariableDeclaration node) {
    bindings.add(_Binding(node.declaredFragment?.element.name, node));
    // Initializers can introduce closures with distinct lexical scopes.
  }

  @override
  void visitDeclaredVariablePattern(DeclaredVariablePattern node) {
    bindings.add(_Binding(node.name.lexeme));
  }

  @override
  void visitFunctionDeclaration(FunctionDeclaration node) {
    bindings.add(_Binding(node.name.lexeme));
    // Do not visit the function's parameters or body.
  }
}

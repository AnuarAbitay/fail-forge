import 'package:analyzer/dart/analysis/results.dart';
import 'package:analyzer/dart/ast/ast.dart';
import 'package:analyzer/dart/ast/visitor.dart';
import 'package:analyzer/dart/element/element.dart';

/// Finds an existing visible SDK class without adding imports. Namespace lookup
/// handles show/hide and re-exports; lexical/member checks prevent the inserted
/// expression from binding to a parameter, local, type parameter or getter.
class SdkSymbols {
  SdkSymbols(this.result);

  final ResolvedUnitResult result;

  /// Owned by one operator scan only: AST objects from later analyzer overlays
  /// must never reuse lexical bindings from a previous resolved unit.
  final _bindings = <AstNode, Set<String>>{};
  late final _prefixes = _visiblePrefixes();

  String? available(
    AstNode anchor,
    String className,
    Uri libraryUri, {
    required bool replacingBody,
  }) {
    bool isClass(Element? element) =>
        element is ClassElement &&
        element.name == className &&
        element.library.uri == libraryUri;

    final scope = result.libraryFragment.scope;
    if (isClass(scope.lookup(className).getter) &&
        !_isShadowed(anchor, className, replacingBody: replacingBody)) {
      return className;
    }
    for (final prefix in _prefixes) {
      final name = prefix.name;
      if (name == null ||
          prefix.fragments.any((fragment) => fragment.isDeferred) ||
          scope.lookup(name).getter?.baseElement != prefix.baseElement ||
          !isClass(prefix.scope.lookup(className).getter) ||
          _isShadowed(anchor, name, replacingBody: replacingBody)) {
        continue;
      }
      return '$name.$className';
    }
    return null;
  }

  List<PrefixElement> _visiblePrefixes() {
    final prefixes = <PrefixElement>{};
    for (
      LibraryFragment? fragment = result.libraryFragment;
      fragment != null;
      fragment = fragment.enclosingFragment
    ) {
      prefixes.addAll(fragment.prefixes);
    }
    return prefixes.toList()
      ..sort((a, b) => (a.name ?? '').compareTo(b.name ?? ''));
  }

  bool _declares(AstNode? node, String name) {
    if (node == null) return false;
    return _bindings
        .putIfAbsent(node, () {
          final visitor = _Bindings();
          node.accept(visitor);
          return visitor.names;
        })
        .contains(name);
  }

  bool _isShadowed(AstNode anchor, String name, {required bool replacingBody}) {
    for (AstNode? node = anchor; node != null; node = node.parent) {
      if (node is MethodDeclaration) {
        if (_declares(node.parameters, name) ||
            _declares(node.typeParameters, name) ||
            (!replacingBody && _declares(node.body, name))) {
          return true;
        }
        final owner = node.declaredFragment?.element.enclosingElement;
        if (owner is InstanceElement) {
          if (owner.typeParameters.any((parameter) => parameter.name == name) ||
              owner.lookUpGetter(name: name, library: result.libraryElement) !=
                  null ||
              owner.lookUpMethod(name: name, library: result.libraryElement) !=
                  null) {
            return true;
          }
        }
      } else if (node is FunctionExpression) {
        if (_declares(node.parameters, name) ||
            _declares(node.typeParameters, name) ||
            (!replacingBody && _declares(node.body, name))) {
          return true;
        }
      }
    }
    return false;
  }
}

/// Deliberately conservative for insertion inside a body: even a binding in an
/// unrelated nested block rejects an unqualified name. Static validation remains
/// a separate gate, rather than the mechanism for recognizing SDK symbols.
class _Bindings extends GeneralizingAstVisitor<void> {
  final names = <String>{};

  @override
  void visitNode(AstNode node) {
    final declaredName = switch (node) {
      FormalParameter() => node.name?.lexeme,
      VariableDeclaration() => node.name.lexeme,
      DeclaredVariablePattern() => node.name.lexeme,
      TypeParameter() => node.name.lexeme,
      CatchClauseParameter() => node.name.lexeme,
      FunctionDeclaration() => node.name.lexeme,
      _ => null,
    };
    if (declaredName != null) names.add(declaredName);
    super.visitNode(node);
  }
}

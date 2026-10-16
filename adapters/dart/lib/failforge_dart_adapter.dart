/// FailForge development tooling; never import into an application.
library;

export 'src/analysis/adapter_engine.dart';
export 'src/model/mutation_family.dart';
export 'src/model/classical_category.dart';
export 'src/model/mutation_category.dart';
export 'src/operators/language_specific/dart/common.dart'
    show LanguageSpecificCategory, LanguageSpecificOperator;
export 'src/patches/patch.dart';
export 'src/project/safe_project.dart';
export 'src/project/source_file.dart';
export 'src/protocol/adapter_error.dart';
export 'src/protocol/protocol_server.dart';
export 'src/scope/scope_rules.dart';

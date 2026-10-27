import 'dart:convert';
import 'dart:io';

import 'package:failforge_dart_adapter/src/project/safe_project.dart';
import 'package:failforge_dart_adapter/src/project/source_file.dart';
import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';
import 'package:failforge_dart_adapter/src/scope/scope_rules.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

void main() {
  late Directory temporary;
  late String workspace;
  late String package;
  late String sdk;
  late Map<String, Object?> payload;

  void write(String path, String content) {
    File(path).parent.createSync(recursive: true);
    File(path).writeAsStringSync(content);
  }

  void configure([List<Map<String, Object?>> dependencies = const []]) {
    write(
      p.join(package, '.dart_tool', 'package_config.json'),
      jsonEncode({
        'configVersion': 2,
        'packages': [
          {
            'name': 'fixture',
            'rootUri': '../',
            'packageUri': 'lib/',
            'languageVersion': '3.11',
          },
          ...dependencies,
        ],
      }),
    );
  }

  setUp(() {
    temporary = Directory.systemTemp.createTempSync('ff-project-');
    workspace = p.join(temporary.path, 'workspace root');
    package = p.join(workspace, 'packages', 'pkg space');
    sdk = p.join(temporary.path, 'sdk');
    write(
      p.join(sdk, 'bin', Platform.isWindows ? 'dart.exe' : 'dart'),
      'fake SDK executable; never run by this layer',
    );
    write(p.join(sdk, 'lib', 'core', 'core.dart'), '// fake SDK resource');
    write(p.join(package, 'pubspec.yaml'), 'name: fixture\n');
    write(p.join(package, 'lib', 'a.dart'), 'bool f() => true;\r\n');
    write(p.join(package, 'bin', 'local.dart'), 'bool f() => false;\n');
    configure();
    payload = {
      'workspaceRoot': workspace,
      'packageRoot': 'packages/pkg space',
      'dartSdkPath': sdk,
    };
  });
  tearDown(() {
    temporary.deleteSync(recursive: true);
  });

  test(
    'explicit package, spaces, defaults and exact bytes remain read-only',
    () {
      final before = File(p.join(package, 'lib', 'a.dart')).readAsBytesSync();
      final project = SafeProject.open(payload);
      expect(
        project.packageRoot,
        Directory(package).resolveSymbolicLinksSync(),
      );
      expect(project.discoverFiles().map((file) => file.relativePath), [
        'lib/a.dart',
      ]);
      expect(
        project.discoverFiles(files: ['bin/local.dart']).single.relativePath,
        'bin/local.dart',
      );
      expect(File(p.join(package, 'lib', 'a.dart')).readAsBytesSync(), before);
      expect(project.analysisOptionsPath, isNull);
      expect(project.flutterSdkPath, isNull);
    },
  );

  test(
    'workspace package config and analysis options resolve without pub get',
    () {
      File(p.join(package, '.dart_tool', 'package_config.json')).deleteSync();
      write(
        p.join(workspace, '.dart_tool', 'package_config.json'),
        jsonEncode({
          'configVersion': 2,
          'packages': [
            {
              'name': 'fixture',
              'rootUri': '../packages/pkg%20space/',
              'packageUri': 'lib/',
            },
          ],
        }),
      );
      write(
        p.join(workspace, 'analysis_options.yaml'),
        'analyzer:\n  errors:\n    unused_import: ignore\n',
      );
      final project = SafeProject.open(payload);
      expect(
        project.packageConfigPath,
        File(p.join(workspace, '.dart_tool', 'package_config.json'))
            .resolveSymbolicLinksSync(),
      );
      expect(
        project.analysisOptionsPath,
        File(p.join(workspace, 'analysis_options.yaml'))
            .resolveSymbolicLinksSync(),
      );
    },
  );

  test(
    'dependency rootUri without trailing slash resolves packageUri correctly',
    () {
      final dependency = p.join(temporary.path, 'cache', 'dep');
      write(p.join(dependency, 'lib', 'dep.dart'), 'class Dep {}');
      configure([
        {
          'name': 'dep',
          'rootUri': Uri.directory(dependency)
              .toString()
              .replaceFirst(RegExp(r'/$'), ''),
          'packageUri': 'lib/',
        },
      ]);
      expect(SafeProject.open(payload).discoverFiles(), hasLength(1));
      expect(
        File(p.join(dependency, 'lib', 'dep.dart')).readAsStringSync(),
        'class Dep {}',
      );
    },
  );

  test(
    'generated patterns and headers skip targets with explicit diagnostics',
    () {
      write(
        p.join(package, 'lib', 'generated.g.dart'),
        'bool generated = true;',
      );
      write(
        p.join(package, 'lib', 'header.dart'),
        '// GENERATED CODE - DO NOT MODIFY BY HAND\nbool x = true;',
      );
      write(p.join(package, 'lib', 'custom.dart'), 'bool x = true;');
      final project = SafeProject.open(payload);
      expect(
        project
            .discoverFiles(
              rules: ScopeRules.fromJson({
                'generatedPatterns': ['lib/custom.dart'],
              }),
            )
            .map((file) => file.relativePath),
        ['lib/a.dart'],
      );
      expect(
        project.diagnostics.where(
          (diagnostic) => diagnostic['code'] == 'generated_target',
        ),
        hasLength(3),
      );
    },
  );

  test('Flutter SDK resources, bundled Dart identity and package resolution are verified', () {
    final flutter = p.join(temporary.path, 'flutter-sdk');
    final bundled = p.join(flutter, 'bin', 'cache', 'dart-sdk');
    final flutterPackage = p.join(flutter, 'packages', 'flutter');
    write(
      p.join(flutter, 'bin', Platform.isWindows ? 'flutter.bat' : 'flutter'),
      '// fake tooling entry; never executed',
    );
    write(
      p.join(bundled, 'bin', Platform.isWindows ? 'dart.exe' : 'dart'),
      '// fake SDK entry; never executed',
    );
    write(p.join(bundled, 'lib', 'core', 'core.dart'), '// fake core resource');
    write(p.join(flutterPackage, 'pubspec.yaml'), 'name: flutter');
    write(
      p.join(flutterPackage, 'lib', 'src', 'services', 'platform_channel.dart'),
      '// fake resource',
    );
    final selected = {
      ...payload,
      'dartSdkPath': bundled,
      'flutterSdkPath': flutter,
    };
    final entry = <String, Object?>{
      'name': 'flutter',
      'rootUri': Uri.directory(flutterPackage).toString(),
      'packageUri': 'lib/',
    };
    configure([entry]);
    expect(
      SafeProject.open(selected).flutterSdkPath,
      Directory(flutter).resolveSymbolicLinksSync(),
    );
    expect(
      () => SafeProject.open({...selected, 'dartSdkPath': sdk}),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'sdk_mismatch',
        ),
      ),
    );
    final rogue = p.join(temporary.path, 'rogue');
    write(p.join(rogue, 'lib', 'services.dart'), '// fake Flutter dependency');
    configure([
      {...entry, 'rootUri': Uri.directory(rogue).toString()},
    ]);
    expect(
      () => SafeProject.open(selected),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'sdk_mismatch',
        ),
      ),
    );
    configure([
      {...entry, 'packageUri': Uri.directory(p.join(rogue, 'lib')).toString()},
    ]);
    expect(
      () => SafeProject.open(selected),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'sdk_mismatch',
        ),
      ),
    );
    configure();
    expect(
      () => SafeProject.open(selected),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'package_config_invalid',
        ),
      ),
    );
  });

  test(
    'ordinary nested library build and test directories remain eligible',
    () {
      write(p.join(package, 'lib', 'build', 'model.dart'), 'bool x = true;');
      write(p.join(package, 'lib', 'test', 'helper.dart'), 'bool x = false;');
      final paths = SafeProject.open(payload)
          .discoverFiles()
          .map((file) => file.relativePath);
      expect(paths, [
        'lib/a.dart',
        'lib/build/model.dart',
        'lib/test/helper.dart',
      ]);
    },
  );

  test('absolute, traversal, excluded and non-Dart targets reject', () {
    write(p.join(package, 'test', 'a.dart'), 'bool x = true;');
    final project = SafeProject.open(payload);
    for (final path in [
      '/outside.dart',
      '../outside.dart',
      'lib/../a.dart',
      'test/a.dart',
      'pubspec.yaml',
    ]) {
      expect(() => project.readTarget(path), throwsA(isA<AdapterException>()));
    }
  });

  test(
    'file and directory symlink escapes reject even before include filtering',
    () {
      final outside = p.join(temporary.path, 'outside.dart');
      write(outside, 'bool x = true;');
      Link(p.join(package, 'lib', 'escape.dart')).createSync(outside);
      final project = SafeProject.open(payload);
      expect(
        () => project.readTarget('lib/escape.dart'),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'unsafe_path',
          ),
        ),
      );
      expect(
        () => project.discoverFiles(
          files: ['lib/escape.dart'],
          rules: ScopeRules.fromJson({'includePaths': []}),
        ),
        throwsA(isA<AdapterException>()),
      );
      Link(p.join(package, 'lib', 'escape.dart')).deleteSync();
      final outsideDirectory = p.join(temporary.path, 'outside');
      write(p.join(outsideDirectory, 'a.dart'), 'bool x = true;');
      Link(p.join(package, 'lib', 'escape')).createSync(outsideDirectory);
      expect(
        () => project.discoverFiles(),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'unsafe_path',
          ),
        ),
      );
    },
  );

  test(
    'package-root symlink outside workspace and invalid service roots reject',
    () {
      final outside = p.join(temporary.path, 'outside-package');
      Directory(outside).createSync();
      Link(p.join(workspace, 'linked')).createSync(outside);
      expect(
        () => SafeProject.open({...payload, 'packageRoot': 'linked'}),
        throwsA(isA<AdapterException>()),
      );
      for (final override in [
        {'packageRoot': '../pkg'},
        {'packageRoot': package},
        {'workspaceRoot': 'relative'},
        {'dartSdkPath': 'relative'},
      ]) {
        expect(
          () => SafeProject.open({...payload, ...override}),
          throwsA(isA<AdapterException>()),
        );
      }
    },
  );

  test('internal file aliases and directory aliases are also rejected as mutation targets', () {
    Link(p.join(package, 'lib', 'alias.dart'))
        .createSync(p.join(package, 'lib', 'a.dart'));
    final project = SafeProject.open(payload);
    expect(
      () => project.readTarget('lib/alias.dart'),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'unsafe_path',
        ),
      ),
    );
    expect(
      () => project.discoverFiles(),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'unsafe_path',
        ),
      ),
    );
    Link(p.join(package, 'lib', 'alias.dart')).deleteSync();
    Link(p.join(package, 'alias-dir')).createSync(p.join(package, 'lib'));
    expect(
      () => project.readTarget('alias-dir/a.dart'),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'unsafe_path',
        ),
      ),
    );
    Link(p.join(package, 'lib', 'loop')).createSync(p.join(package, 'lib'));
    expect(
      () => project.discoverFiles(),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'unsafe_path',
        ),
      ),
    );
  });

  test(
    'missing SDK, pubspec, package config and dependencies are failures',
    () {
      expect(
        () => SafeProject.open({
          ...payload,
          'dartSdkPath': p.join(temporary.path, 'absent-sdk'),
        }),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'sdk_missing',
          ),
        ),
      );
      File(p.join(package, 'pubspec.yaml')).deleteSync();
      expect(
        () => SafeProject.open(payload),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'pubspec_missing',
          ),
        ),
      );
      write(p.join(package, 'pubspec.yaml'), 'name: fixture');
      File(p.join(package, '.dart_tool', 'package_config.json')).deleteSync();
      expect(
        () => SafeProject.open(payload),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'package_config_missing',
          ),
        ),
      );
      configure([
        {
          'name': 'missing',
          'rootUri': Uri.directory(p.join(temporary.path, 'missing'))
              .toString(),
          'packageUri': 'lib/',
        },
      ]);
      expect(
        () => SafeProject.open(payload),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.message,
            'message',
            contains('dependency'),
          ),
        ),
      );
    },
  );

  test('nested external dependency source cannot become a mutation target', () {
    final dependency = p.join(package, 'vendor', 'dep');
    write(p.join(dependency, 'lib', 'dep.dart'), 'bool x = true;');
    configure([
      {
        'name': 'dep',
        'rootUri': Uri.directory(dependency).toString(),
        'packageUri': 'lib/',
      },
    ]);
    final project = SafeProject.open(payload);
    expect(
      () => project.readTarget('vendor/dep/lib/dep.dart'),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'unsafe_path',
        ),
      ),
    );
  });

  test(
    'source, aggregate and target-list limits fail rather than truncate',
    () {
      final oversized = File(p.join(package, 'lib', 'oversized.dart'))
          .openSync(mode: FileMode.write);
      oversized.truncateSync(SourceFile.maxSourceBytes + 1);
      oversized.closeSync();
      final project = SafeProject.open(payload);
      expect(
        () => project.discoverFiles(),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'source_too_large',
          ),
        ),
      );
      File(p.join(package, 'lib', 'oversized.dart')).deleteSync();
      for (var index = 0; index < 17; index++) {
        final file = File(p.join(package, 'lib', 'sparse$index.dart'))
            .openSync(mode: FileMode.write);
        file.truncateSync(SourceFile.maxSourceBytes);
        file.closeSync();
      }
      expect(
        () => project.discoverFiles(),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'target_limit_exceeded',
          ),
        ),
      );
      expect(
        () => project.discoverFiles(files: List.filled(2001, 'lib/a.dart')),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'target_limit_exceeded',
          ),
        ),
      );
    },
  );

  test('invalid UTF-8 source is rejected without changing its bytes', () {
    final target = File(p.join(package, 'lib', 'bad.dart'))
      ..writeAsBytesSync([0xff]);
    final project = SafeProject.open(payload);
    expect(
      () => project.readTarget('lib/bad.dart'),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'invalid_utf8',
        ),
      ),
    );
    expect(target.readAsBytesSync(), [0xff]);
  });
}

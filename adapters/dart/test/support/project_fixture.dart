import 'dart:convert';
import 'dart:io';

import 'package:path/path.dart' as p;

class ProjectFixture {
  final Directory directory;
  ProjectFixture._(this.directory);

  static ProjectFixture create(Map<String, String> files) {
    final fixture = ProjectFixture._(
      Directory.systemTemp.createTempSync('failforge dart '),
    );
    fixture.write(
      'pubspec.yaml',
      'name: fixture\nenvironment:\n  sdk: ">=3.13.0 <4.0.0"\n',
    );
    fixture.write(
      '.dart_tool/package_config.json',
      jsonEncode({
        'configVersion': 2,
        'packages': [
          {
            'name': 'fixture',
            'rootUri': '../',
            'packageUri': 'lib/',
            'languageVersion': '3.13',
          },
        ],
      }),
    );
    for (final entry in files.entries) {
      fixture.write(entry.key, entry.value);
    }
    return fixture;
  }

  String get root => directory.path;
  String get sdk => p.dirname(p.dirname(Platform.resolvedExecutable));
  Map<String, Object?> get payload => {
    'workspaceRoot': root,
    'packageRoot': '.',
    'dartSdkPath': sdk,
    'rules': <String, Object?>{},
    'limits': {'maxMutants': 1000},
  };
  void write(String file, String source) {
    final target = File(p.join(root, file));
    target.parent.createSync(recursive: true);
    target.writeAsStringSync(source);
  }

  void close() => directory.deleteSync(recursive: true);
}

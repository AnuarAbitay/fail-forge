import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:failforge_dart_adapter/src/isolation/namespace_plan.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  test('complete inventory binds multi-file Dart 3.4 author sources and leaves all bytes intact', () async {
    const source =
        "import 'dart:isolate' as iso; Uri? locate()=>iso.Isolate.resolvePackageUriSync(Uri.parse('package:fixture/'));";
    final fixture = ProjectFixture.create({
      'lib/anchor.dart': '',
      'test/runtime.dart': source,
      'test/plain.dart': 'void main() {}',
    });
    addTearDown(fixture.close);
    final config = File('${fixture.root}/.dart_tool/package_config.json');
    final parsed = jsonDecode(config.readAsStringSync()) as Map;
    ((parsed['packages'] as List).single as Map)['languageVersion'] = '3.4';
    config.writeAsStringSync(jsonEncode(parsed));
    final before = <String, String>{
      for (final file
          in fixture.directory.listSync(recursive: true).whereType<File>())
        file.path: sha256.convert(file.readAsBytesSync()).toString(),
    };
    final plan = await planNamespace({
      'workspaceRoot': fixture.root,
      'packageRoot': '.',
      'dartSdkPath': fixture.sdk,
      'packageName': 'fixture',
      'alias': 'failforge_subject',
      'files': ['test/runtime.dart', 'test/plain.dart'],
    });
    expect(plan['unsupported'], isEmpty);
    final rows = (plan['files'] as List).cast<Map>();
    expect(rows.map((row) => row['file']), [
      'test/plain.dart',
      'test/runtime.dart',
    ]);
    expect(rows.first['edits'], isEmpty);
    expect(rows.last['edits'], hasLength(1));
    expect(
      rows.last['sourceHash'],
      sha256.convert(utf8.encode(source)).toString(),
    );
    expect({
      for (final path in before.keys)
        path: sha256.convert(File(path).readAsBytesSync()).toString(),
    }, before);
  });
}

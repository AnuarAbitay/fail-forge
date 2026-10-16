import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:failforge_dart_adapter/src/isolation/namespace_plan.dart';
import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  Future<Map<String, Object?>> plan(
    String source, {
    String name = 'test/probe.dart',
  }) async {
    final fixture = ProjectFixture.create({
      'lib/anchor.dart': '',
      name: source,
    });
    addTearDown(fixture.close);
    final before = File('${fixture.root}/$name').readAsBytesSync();
    final config = File('${fixture.root}/.dart_tool/package_config.json')
        .readAsBytesSync();
    final result = await planNamespace({
      'workspaceRoot': fixture.root,
      'packageRoot': '.',
      'dartSdkPath': fixture.sdk,
      'packageName': 'fixture',
      'alias': 'failforge_subject',
      'files': [name],
    });
    expect(File('${fixture.root}/$name').readAsBytesSync(), before);
    expect(
      File('${fixture.root}/.dart_tool/package_config.json').readAsBytesSync(),
      config,
    );
    expect(result['packageConfigHash'], sha256.convert(config).toString());
    expect(result.keys.toSet(), {
      'version',
      'kind',
      'packageName',
      'alias',
      'packageConfigHash',
      'files',
      'unsupported',
    });
    return result;
  }

  List edits(Map<String, Object?> result) =>
      ((result['files'] as List).single as Map)['edits'] as List;
  List unsupported(Map<String, Object?> result) =>
      result['unsupported'] as List;

  test('actual prefixed SDK synchronous and asynchronous resolvers are planned', () async {
    const source =
        "import 'dart:isolate' as iso; import 'dart:core' as core; "
        "core.Uri? a()=>iso.Isolate.resolvePackageUriSync(core.Uri.parse('package:fixture/a.dart')); "
        "core.Object b()=>iso.Isolate.resolvePackageUri(core.Uri.parse('package:fixture/'));";
    final result = await plan(source);
    expect(unsupported(result), isEmpty);
    expect(edits(result), hasLength(2));
    for (final edit in edits(result).cast<Map>()) {
      expect(edit['kind'], 'runtime_package_uri');
      expect(edit['expected'], 'package:fixture/');
      expect(edit['replacement'], 'package:failforge_subject/');
      expect(edit['lengthBytes'], utf8.encode('package:fixture/').length);
    }
  });

  test('BOM, astral text, raw/triple literals and Unicode filenames retain byte offsets', () async {
    const source =
        '\ufeff// 😀 before URI\r\n'
        "import 'dart:isolate';\n"
        "Uri? a()=>Isolate.resolvePackageUriSync(Uri.parse(r'package:fixture/a.dart'));\n"
        'Uri? b()=>Isolate.resolvePackageUriSync(Uri.parse("""package:fixture/"""));\n';
    final result = await plan(source, name: 'test/données 😀.dart');
    expect(unsupported(result), isEmpty);
    var bytes = List<int>.from(utf8.encode(source));
    final original = List<int>.from(bytes);
    final ranges = edits(result).cast<Map>().toList();
    expect(ranges, hasLength(2));
    expect(
      ranges.first['offsetBytes'],
      utf8
          .encode(source.substring(0, source.indexOf('package:fixture/')))
          .length,
    );
    for (final edit in ranges.reversed) {
      final start = edit['offsetBytes'] as int,
          length = edit['lengthBytes'] as int;
      expect(
        utf8.decode(bytes.sublist(start, start + length)),
        edit['expected'],
      );
      bytes.replaceRange(
        start,
        start + length,
        utf8.encode(edit['replacement'] as String),
      );
    }
    expect(
      utf8.decode(bytes, allowMalformed: false),
      contains('package:failforge_subject/a.dart'),
    );
    for (var index = 0; index < ranges.length; index++) {
      final edit = ranges[index];
      final start = edit['offsetBytes'] as int;
      bytes.replaceRange(
        start,
        start + utf8.encode(edit['replacement'] as String).length,
        utf8.encode(edit['expected'] as String),
      );
    }
    expect(bytes, original);
  });

  test('comments, assertions and ordinary own-package strings receive no edits', () async {
    const source =
        "// Isolate.resolvePackageUriSync(Uri.parse('package:fixture/'))\n"
        "String f()=> 'package:fixture/'; void g(){assert(f()=='package:fixture/');}";
    final result = await plan(source);
    expect(edits(result), isEmpty);
    expect(unsupported(result), isEmpty);
  });

  test('custom shadowed Isolate calls and tear-offs receive no edits', () async {
    const source =
        "class Isolate { static Object resolvePackageUriSync(Object v)=>v; } "
        "final call=Isolate.resolvePackageUriSync; Object f()=>call(Uri.parse('package:fixture/'));";
    final result = await plan(source);
    expect(edits(result), isEmpty);
    expect(unsupported(result), isEmpty);
  });

  test('custom Uri.parse cannot authorize an edit for a genuine resolver', () async {
    const source =
        "import 'dart:isolate'; import 'dart:core' as core; "
        "class Uri { static core.Uri parse(core.String v)=>core.Uri.parse(v); } "
        "core.Uri? f()=>Isolate.resolvePackageUriSync(Uri.parse('package:fixture/'));";
    final result = await plan(source);
    expect(edits(result), isEmpty);
    expect((unsupported(result).single as Map)['code'], 'indirect_package_uri');
  });

  test(
    'genuine SDK resolver tear-off fails closed before indirect invocation',
    () async {
      const source =
          "import 'dart:isolate'; final call=Isolate.resolvePackageUriSync; "
          "Uri? f()=>call(Uri.parse('package:fixture/'));";
      final result = await plan(source);
      expect(edits(result), isEmpty);
      expect(
        (unsupported(result).single as Map)['code'],
        'indirect_resolver_reference',
      );
    },
  );

  test('canonical other-package and SDK URI literals stay unchanged', () async {
    const source =
        "import 'dart:isolate'; Uri? a()=>Isolate.resolvePackageUriSync(Uri.parse('package:other/')); "
        "Uri? b()=>Isolate.resolvePackageUriSync(Uri.parse('dart:core'));";
    final result = await plan(source);
    expect(edits(result), isEmpty);
    expect(unsupported(result), isEmpty);
  });

  test('escaped, encoded and noncanonical URI spellings fail closed', () async {
    for (final literal in [
      r"'package:fixt\u0075re/'",
      "'package:fixture/%61.dart'",
      "'Package:fixture/'",
    ]) {
      final result = await plan(
        "import 'dart:isolate'; Uri? f()=>Isolate.resolvePackageUriSync(Uri.parse($literal));",
      );
      expect(edits(result), isEmpty);
      expect(
        (unsupported(result).single as Map)['code'],
        'noncanonical_package_uri',
      );
    }
  });

  test('interpolated, concatenated and constant-alias URI strings fail closed', () async {
    for (final expression in [
      r"'package:fixture/$suffix'",
      "'package:' 'fixture/'",
      'value',
    ]) {
      final result = await plan(
        "import 'dart:isolate'; const value='package:fixture/'; "
        "Uri? f(String suffix)=>Isolate.resolvePackageUriSync(Uri.parse($expression));",
      );
      expect(edits(result), isEmpty);
      expect(
        (unsupported(result).single as Map)['code'],
        'computed_package_uri',
      );
    }
  });

  test(
    'indirect URI values and parse substring arguments fail closed',
    () async {
      for (final expression in ['value', "Uri.parse('package:fixture/',0)"]) {
        final result = await plan(
          "import 'dart:isolate'; final value=Uri.parse('package:fixture/'); "
          "Uri? f()=>Isolate.resolvePackageUriSync($expression);",
        );
        expect(edits(result), isEmpty);
        expect(
          (unsupported(result).single as Map)['code'],
          'indirect_package_uri',
        );
      }
    },
  );

  test(
    'parenthesized direct URI and literal remain the same safe call',
    () async {
      final result = await plan(
        "import 'dart:isolate'; "
        "Uri? f()=>Isolate.resolvePackageUriSync((Uri.parse(('package:fixture/'))));",
      );
      expect(unsupported(result), isEmpty);
      expect(edits(result), hasLength(1));
    },
  );

  test(
    'baseline analysis errors are errors rather than an empty successful plan',
    () async {
      await expectLater(
        plan(
          "import 'dart:isolate'; Uri? f()=>Isolate.resolvePackageUriSync(unknown);",
        ),
        throwsA(
          isA<AdapterException>().having(
            (error) => error.code,
            'code',
            'namespace_analysis_errors',
          ),
        ),
      );
    },
  );

  test(
    'unsafe paths, duplicates, names and unknown payload keys are rejected',
    () async {
      final fixture = ProjectFixture.create({
        'lib/anchor.dart': '',
        'test/probe.dart': 'void main(){}',
      });
      addTearDown(fixture.close);
      final base = <String, Object?>{
        'workspaceRoot': fixture.root,
        'packageRoot': '.',
        'dartSdkPath': fixture.sdk,
        'packageName': 'fixture',
        'alias': 'failforge_subject',
        'files': ['test/probe.dart'],
      };
      for (final changes in <Map<String, Object?>>[
        {
          'files': ['../outside.dart'],
        },
        {
          'files': ['test/../probe.dart'],
        },
        {
          'files': ['bin/probe.dart'],
        },
        {
          'files': ['test/probe.dart', 'test/probe.dart'],
        },
        {'packageName': 'other'},
        {'alias': 'fixture'},
        {'alias': 'bad-name'},
        {'unknown': true},
      ]) {
        await expectLater(
          planNamespace({...base, ...changes}),
          throwsA(isA<AdapterException>()),
        );
      }
    },
  );

  test('symlink source paths are rejected', () async {
    final fixture = ProjectFixture.create({
      'lib/anchor.dart': '',
      'test/original.dart': 'void main(){}',
    });
    addTearDown(fixture.close);
    Link('${fixture.root}/test/link.dart').createSync('original.dart');
    await expectLater(
      planNamespace({
        'workspaceRoot': fixture.root,
        'packageRoot': '.',
        'dartSdkPath': fixture.sdk,
        'packageName': 'fixture',
        'alias': 'failforge_subject',
        'files': ['test/link.dart'],
      }),
      throwsA(
        isA<AdapterException>().having(
          (error) => error.code,
          'code',
          'unsafe_path',
        ),
      ),
    );
  });
}

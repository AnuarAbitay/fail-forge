import 'dart:convert';
import 'dart:io';

import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  test('part files use the parent library SDK exception prefixes for entry failures', () async {
    final fixture = _flutterProject({
      'lib/root.dart': "import 'dart:core' as core;\npart 'piece.dart';\n",
      'lib/piece.dart': "part of 'root.dart';\ncore.int entry() => 1;\n",
    });
    addTearDown(fixture.close);
    final payload = _payload(fixture, 'EXCEPTION_AT_ENTRY', 'entry');
    final mutants = await _discover(payload);
    expect(mutants, hasLength(1));
    expect(mutants.single.edit.replacement, contains('throw core.StateError('));
    expect(mutants.single.file, 'lib/piece.dart');
    expect(
      (await AdapterEngine().handle('validate', {
        ...payload,
        'mutant': mutants.single.toJson(),
      }))['valid'],
      isTrue,
    );
  });

  test('part files use parent Flutter prefixes for both directly awaited channel failures', () async {
    final fixture = _flutterProject({
      'lib/root.dart': "import 'package:flutter/services.dart' as channel;\npart 'piece.dart';\n",
      'lib/piece.dart': "part of 'root.dart';\nFuture<int?> read(channel.MethodChannel target) async => await target.invokeMethod<int>('read');\n",
    });
    addTearDown(fixture.close);
    final payload = _payload(fixture, 'METHOD_CHANNEL_ERROR', 'read');
    final mutants = await _discover(payload);
    expect(mutants, hasLength(2));
    expect(
      mutants.map((mutant) => mutant.edit.replacement),
      containsAll([
        contains('channel.PlatformException('),
        contains('channel.MissingPluginException('),
      ]),
    );
    for (final mutant in mutants) {
      expect(
        (await AdapterEngine().handle('validate', {
          ...payload,
          'mutant': mutant.toJson(),
        }))['valid'],
        isTrue,
      );
    }
  });

  test(
    'entry failures skip an SDK exception shadowed by an extension getter',
    () async {
      final fixture = _flutterProject({
        'lib/root.dart': 'extension Tools on int { int get StateError => 1; int entry() => 2; }\n',
      });
      addTearDown(fixture.close);
      final mutants = await _discover(
        _payload(fixture, 'EXCEPTION_AT_ENTRY', 'entry'),
      );
      expect(mutants, isEmpty);
    },
  );

  test('unloaded deferred exception imports cannot supply channel error constructors', () async {
    final fixture = _flutterProject({
      'lib/root.dart':
          "import 'package:flutter/services.dart' show MethodChannel;\n"
          "import 'package:flutter/services.dart' deferred as errors show PlatformException, MissingPluginException;\n"
          "Future<int?> read(MethodChannel target) async => await target.invokeMethod<int>('read');\n",
    });
    addTearDown(fixture.close);
    final result = await AdapterEngine().handle(
      'discover',
      _payload(fixture, 'METHOD_CHANNEL_ERROR', 'read'),
    );
    expect(
      result['mutants'],
      isEmpty,
      reason: 'An unloaded deferred prefix produces a library-loading error rather than the requested SDK error.',
    );
    expect(
      (result['diagnostics'] as List).whereType<Map>().map(
        (item) => item['code'],
      ),
      contains('sdk_exception_not_available'),
    );
  });

  test(
    'unloaded deferred core imports cannot supply entry error constructors',
    () async {
      final fixture = _flutterProject({
        'lib/root.dart':
            "import 'dart:core' as core hide StateError;\n"
            "import 'dart:core' deferred as errors show StateError;\n"
            'core.int entry() => 1;\n',
      });
      addTearDown(fixture.close);
      expect(
        await _discover(_payload(fixture, 'EXCEPTION_AT_ENTRY', 'entry')),
        isEmpty,
      );
    },
  );

  test('real Dart rejects an unloaded SDK constructor before the requested exception exists', () async {
    final fixture = ProjectFixture.create({
      'tool/probe.dart':
          "import 'dart:core' as core;\n"
          "import 'dart:core' deferred as errors show StateError;\n"
          "void main() { try { throw errors.StateError('FAILFORGE_EXPECTED_STATE_ERROR'); } catch (error) { core.print(error is core.StateError ? 'requested-sdk-error' : 'different-error'); core.print(error); } }\n",
    });
    addTearDown(fixture.close);
    final process = await Process.start(Platform.resolvedExecutable, [
      '--packages=${p.join(fixture.root, '.dart_tool/package_config.json')}',
      p.join(fixture.root, 'tool/probe.dart'),
    ], workingDirectory: fixture.root);
    var exited = false;
    final exitCode = process.exitCode.then((code) {
      exited = true;
      return code;
    });
    final stdout = process.stdout.transform(utf8.decoder).join();
    final stderr = process.stderr.transform(utf8.decoder).join();
    try {
      final result = await Future.wait<Object>([exitCode, stdout, stderr])
          .timeout(const Duration(seconds: 10));
      expect(result[0], 0, reason: result[2] as String);
      expect(
        result[1],
        'different-error\nDeferred library errors was not loaded.\n',
      );
    } finally {
      if (!exited) {
        process.kill(ProcessSignal.sigkill);
        await exitCode.timeout(const Duration(seconds: 5));
      }
    }
  });
}

Future<List<Mutant>> _discover(Map<String, Object?> payload) async {
  final result = await AdapterEngine().handle('discover', payload);
  return (result['mutants'] as List)
      .map((value) => Mutant.fromJson(value as Map<String, Object?>))
      .toList();
}

Map<String, Object?> _payload(
  ProjectFixture fixture,
  String operator,
  String method,
) => {
  ...fixture.payload,
  'flutterSdkPath': p.normalize(p.join(fixture.sdk, '../../..')),
  'rules': {
    'operators': [operator],
    'methods': [method],
  },
};

/// Reuses the already-resolved real Flutter dependencies without pub get or
/// editing the shared application fixture. Every target source is disposable.
ProjectFixture _flutterProject(Map<String, String> files) {
  final fixture = ProjectFixture.create(files);
  final exampleConfig = File(
    p.join(
      Directory.current.path,
      '../../examples/flutter_fixture/.dart_tool/package_config.json',
    ),
  );
  final config =
      jsonDecode(exampleConfig.readAsStringSync()) as Map<String, Object?>;
  config['packages'] = [
    ...(config['packages'] as List).where(
      (value) => (value as Map)['name'] != 'failforge_flutter_fixture',
    ),
    {
      'name': 'fixture',
      'rootUri': Uri.directory(fixture.root).toString(),
      'packageUri': 'lib/',
      'languageVersion': '3.13',
    },
  ];
  fixture.write('.dart_tool/package_config.json', jsonEncode(config));
  return fixture;
}

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:failforge/src/release.dart';
import 'package:failforge/src/runtime_manager.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

import 'support/runtime_fixture.dart';

void main() {
  late Directory temporary;
  late Directory cache;
  late Directory dependencies;
  late RuntimeManager manager;
  late int downloads;
  late int extractions;
  late int preparations;

  setUp(() async {
    temporary = await Directory.systemTemp.createTemp('failforge manager ');
    cache = Directory(p.join(temporary.path, 'runtime-cache'));
    dependencies = Directory(p.join(temporary.path, 'dependencies'));
    downloads = 0;
    extractions = 0;
    preparations = 0;
    manager = RuntimeManager(
      release: fixtureRelease,
      cacheRoot: cache,
      dartExecutable: Platform.resolvedExecutable,
      download: (uri, destination) async {
        downloads++;
        expect(uri, fixtureRelease.uri);
        await destination.writeAsBytes(fixtureArchiveBytes);
      },
      extract:
          (archive, destination, {required version, required target}) async {
            extractions++;
            await extractFixture(
              archive,
              destination,
              version: version,
              target: target,
            );
          },
      prepare: (dart, adapter) async {
        preparations++;
        expect(dart, Platform.resolvedExecutable);
        expect(
          adapter.path,
          p.join(manager.installation.path, 'adapters', 'dart'),
        );
        await prepareFixture(adapter, dependencies);
      },
    );
  });

  tearDown(() async {
    await temporary.delete(recursive: true);
  });

  Future<void> expectNoScratch() async {
    final leftover = await cache
        .list(followLinks: false)
        .where(
          (entry) =>
              entry.path.endsWith('.download') ||
              entry.path.endsWith('.install'),
        )
        .map((entry) => entry.path)
        .toList();
    expect(leftover, isEmpty);
  }

  test('checks archive SHA before extracting or preparing', () async {
    final wrongDownload = RuntimeManager(
      release: fixtureRelease,
      cacheRoot: cache,
      dartExecutable: Platform.resolvedExecutable,
      download: (uri, file) => file.writeAsString('corrupted').then((_) {}),
      extract:
          (archive, destination, {required version, required target}) async {
            extractions++;
          },
      prepare: (dart, adapter) async => preparations++,
    );
    await expectLater(wrongDownload.ensureReady(), throwsFormatException);
    expect(extractions, 0);
    expect(preparations, 0);
    expect(await wrongDownload.installation.exists(), isFalse);
    await expectNoScratch();
  });

  test('checks pinned manifest before publishing the installation', () async {
    final wrongManifest = RuntimeManager(
      release: ReleaseArtifact(
        version: fixtureRelease.version,
        target: fixtureRelease.target,
        sha256: fixtureRelease.sha256,
        manifestSha256: '0' * 64,
      ),
      cacheRoot: cache,
      dartExecutable: Platform.resolvedExecutable,
      download: (uri, file) =>
          file.writeAsBytes(fixtureArchiveBytes).then((_) {}),
      extract: extractFixture,
      prepare: (dart, adapter) async => preparations++,
    );
    await expectLater(wrongManifest.ensureReady(), throwsFormatException);
    expect(preparations, 0);
    expect(await wrongManifest.installation.exists(), isFalse);
    await expectNoScratch();
  });

  test('reuses verified runtime and prepared dependencies', () async {
    final executable = await manager.ensureReady();
    expect(executable, p.join(manager.installation.path, 'bin', 'failforge'));
    expect(await manager.ensureReady(), executable);
    expect(downloads, 1);
    expect(extractions, 1);
    expect(preparations, 1);
    await expectNoScratch();
  });

  test(
    'does not follow a preexisting temporary preparation marker symlink',
    () async {
      await cache.create(recursive: true);
      final outside = File(p.join(temporary.path, 'keep-unchanged'));
      await outside.writeAsString('external marker contents');
      await Link(
        p.join(cache.path, '${manager.installationKey}.prepared.json.tmp'),
      ).create(outside.path);
      await manager.ensureReady();
      expect(await outside.readAsString(), 'external marker contents');
      expect(preparations, 1);
      await expectNoScratch();
    },
    skip: Platform.isWindows
        ? 'Creating symbolic links needs Unix permissions.'
        : false,
  );

  test('retries failed preparation without downloading again', () async {
    final retry = RuntimeManager(
      release: fixtureRelease,
      cacheRoot: cache,
      dartExecutable: Platform.resolvedExecutable,
      download: manager.download,
      extract: manager.extract,
      prepare: (dart, adapter) async {
        preparations++;
        if (preparations == 1) throw StateError('Dependency download failed.');
        await prepareFixture(adapter, dependencies);
      },
    );
    await expectLater(retry.ensureReady(), throwsStateError);
    expect(await retry.installation.exists(), isTrue);
    await retry.ensureReady();
    expect(downloads, 1);
    expect(extractions, 1);
    expect(preparations, 2);
  });

  test('reprepares when a referenced Pub cache directory disappears', () async {
    await manager.ensureReady();
    await dependencies.delete(recursive: true);
    await manager.ensureReady();
    expect(downloads, 1);
    expect(extractions, 1);
    expect(preparations, 2);
    expect(await dependencies.exists(), isTrue);
  });

  test('replaces tampered payload before preparing and returning it', () async {
    final executable = await manager.ensureReady();
    await File(executable).writeAsString('tampered-native');
    await manager.ensureReady();
    expect(await File(executable).readAsString(), 'native-cli');
    expect(downloads, 2);
    expect(extractions, 2);
    expect(preparations, 2);
  });

  test('isolates adapter state for different Dart SDK paths', () async {
    Future<RuntimeManager> sdkManager(String name) async {
      final sdk = Directory(p.join(temporary.path, name));
      final dart = File(p.join(sdk.path, 'bin', 'dart'));
      await dart.parent.create(recursive: true);
      await dart.writeAsString('fixture SDK executable is never run');
      await File(p.join(sdk.path, 'version')).writeAsString('3.13.5\n');
      final sdkDependencies = Directory(p.join(dependencies.path, name));
      return RuntimeManager(
        release: fixtureRelease,
        cacheRoot: cache,
        dartExecutable: dart.path,
        download: manager.download,
        extract: manager.extract,
        prepare: (selected, adapter) async {
          preparations++;
          expect(selected, dart.path);
          await prepareFixture(adapter, sdkDependencies);
        },
      );
    }

    final first = await sdkManager('sdk one');
    final second = await sdkManager('sdk two');
    await first.ensureReady();
    final firstConfig = File(
      p.join(
        first.installation.path,
        'adapters',
        'dart',
        '.dart_tool',
        'package_config.json',
      ),
    );
    final originalConfig = await firstConfig.readAsString();
    await second.ensureReady();

    expect(first.installation.path, isNot(second.installation.path));
    expect(await firstConfig.readAsString(), originalConfig);
    expect(await first.installation.exists(), isTrue);
    expect(await second.installation.exists(), isTrue);
    await first.ensureReady();
    expect(downloads, 2);
    expect(extractions, 2);
    expect(preparations, 2);
    await expectNoScratch();
  });

  test('isolates adapter state for different inherited Pub caches', () async {
    final driver = File('test/support/runtime_manager_driver.dart')
        .absolute
        .path;
    final config = File('.dart_tool/package_config.json').absolute.path;
    final ledger = File(p.join(temporary.path, 'events'));
    final gate = File(p.join(temporary.path, 'release-download'));
    await gate.writeAsString('continue');
    final firstPubCache = Directory(p.join(temporary.path, 'pub-cache-one'));
    final secondPubCache = Directory(p.join(temporary.path, 'pub-cache-two'));

    Future<String> run(Directory pubCache) async {
      final result = await Process.run(
        Platform.resolvedExecutable,
        [
          '--packages=$config',
          driver,
          cache.path,
          ledger.path,
          gate.path,
          pubCache.path,
        ],
        environment: {'PUB_CACHE': pubCache.path},
      );
      expect(result.exitCode, 0, reason: result.stderr.toString());
      expect(result.stderr, isEmpty);
      return const LineSplitter()
          .convert(result.stdout as String)
          .singleWhere((line) => line.startsWith('installation:'))
          .substring('installation:'.length);
    }

    final firstInstallation = await run(firstPubCache);
    final firstConfig = File(
      p.join(
        firstInstallation,
        'adapters',
        'dart',
        '.dart_tool',
        'package_config.json',
      ),
    );
    final originalConfig = await firstConfig.readAsString();
    final secondInstallation = await run(secondPubCache);
    expect(secondInstallation, isNot(firstInstallation));
    expect(await firstConfig.readAsString(), originalConfig);
    expect(await run(firstPubCache), firstInstallation);
    expect(await ledger.readAsLines(), [
      'download',
      'prepare',
      'download',
      'prepare',
    ]);
    await expectNoScratch();
  });

  test('serializes separate processes installing the same release', () async {
    final driver = File('test/support/runtime_manager_driver.dart')
        .absolute
        .path;
    final config = File('.dart_tool/package_config.json').absolute.path;
    final ledger = File(p.join(temporary.path, 'events'));
    final gate = File(p.join(temporary.path, 'release-download'));
    final processes = <Process>[];
    final completed = <int>{};
    final errors = <Future<String>>[];

    addTearDown(() async {
      for (final process in processes) {
        if (!completed.contains(process.pid)) {
          process.kill(ProcessSignal.sigkill);
          await process.exitCode.timeout(const Duration(seconds: 5));
        }
      }
    });

    Future<Process> start() async {
      final process = await Process.start(Platform.resolvedExecutable, [
        '--packages=$config',
        driver,
        cache.path,
        ledger.path,
        gate.path,
      ]);
      processes.add(process);
      unawaited(process.exitCode.then((_) => completed.add(process.pid)));
      errors.add(process.stderr.transform(utf8.decoder).join());
      final ready = Completer<void>();
      process.stdout
          .transform(utf8.decoder)
          .transform(const LineSplitter())
          .listen(
            (line) {
              if (line == 'ready' && !ready.isCompleted) ready.complete();
            },
            onDone: () {
              if (!ready.isCompleted)
                ready.completeError(
                  StateError('Driver exited before readiness.'),
                );
            },
          );
      await ready.future.timeout(const Duration(seconds: 15));
      return process;
    }

    final first = await start();
    Future<void> waitForDownload() async {
      while (!await ledger.exists() ||
          !(await ledger.readAsString()).contains('download')) {
        await Future<void>.delayed(const Duration(milliseconds: 25));
      }
    }

    await waitForDownload().timeout(const Duration(seconds: 15));
    final second = await start();
    await Future<void>.delayed(const Duration(milliseconds: 350));
    await gate.writeAsString('continue');

    expect(await first.exitCode.timeout(const Duration(seconds: 15)), 0);
    expect(await second.exitCode.timeout(const Duration(seconds: 15)), 0);
    for (final error in errors) {
      expect(await error, isEmpty);
    }
    expect(await ledger.readAsLines(), ['download', 'prepare']);
  });
}

import 'dart:async';
import 'dart:io';

import 'package:failforge/src/runtime_manager.dart';
import 'package:path/path.dart' as p;

import 'runtime_fixture.dart';

Future<void> main(List<String> arguments) async {
  final cache = Directory(arguments[0]);
  final ledger = File(arguments[1]);
  final gate = File(arguments[2]);
  final dependencies = Directory(
    arguments.length > 3
        ? arguments[3]
        : p.join(cache.parent.path, 'dependencies'),
  );
  final manager = RuntimeManager(
    release: fixtureRelease,
    cacheRoot: cache,
    dartExecutable: Platform.resolvedExecutable,
    download: (uri, destination) async {
      await ledger.writeAsString(
        'download\n',
        mode: FileMode.append,
        flush: true,
      );
      while (!await gate.exists()) {
        await Future<void>.delayed(const Duration(milliseconds: 25));
      }
      await destination.writeAsBytes(fixtureArchiveBytes);
    },
    extract: extractFixture,
    prepare: (dart, adapter) async {
      await ledger.writeAsString(
        'prepare\n',
        mode: FileMode.append,
        flush: true,
      );
      await prepareFixture(adapter, dependencies);
    },
  );
  stdout.writeln('ready');
  await stdout.flush();
  await manager.ensureReady();
  stdout.writeln('installation:${manager.installation.path}');
}

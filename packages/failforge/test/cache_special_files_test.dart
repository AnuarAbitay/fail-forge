import 'dart:convert';
import 'dart:io';

import 'package:test/test.dart';

void main() {
  for (final kind in ['manifest', 'marker', 'lock']) {
    test('does not block on a cached $kind FIFO', () async {
      final root = await Directory.systemTemp.createTemp(
        'failforge cache FIFO ',
      );
      Process? child;
      var exited = false;
      try {
        child = await Process.start(Platform.resolvedExecutable, [
          '--packages=${File('.dart_tool/package_config.json').absolute.path}',
          File('test/support/cache_special_files_driver.dart').absolute.path,
          root.path,
          kind,
        ]);
        final stdout = child.stdout.transform(utf8.decoder).join();
        final stderr = child.stderr.transform(utf8.decoder).join();
        final code = await child.exitCode.timeout(const Duration(seconds: 10));
        exited = true;
        expect(await stderr, isEmpty);
        expect(code, 0);
        expect(await stdout, '$kind:checked\n');
      } finally {
        if (child != null && !exited) {
          child.kill(ProcessSignal.sigkill);
          await child.exitCode;
        }
        await root.delete(recursive: true);
      }
    }, skip: Platform.isWindows ? 'POSIX FIFO recovery.' : false);
  }
}

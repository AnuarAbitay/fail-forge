import 'dart:ffi';
import 'dart:io';

import 'package:failforge/src/process_launcher.dart';

Future<void> main(List<String> arguments) async {
  if (arguments.first == '--new-session') {
    final setsid = DynamicLibrary.process()
        .lookupFunction<Int32 Function(), int Function()>('setsid');
    if (setsid() == -1) {
      stderr.writeln('Cannot create isolated process group for signal test.');
      exit(70);
    }
    arguments = arguments.skip(1).toList();
  }
  final fixture = Platform.environment['FAILFORGE_TEST_FIXTURE'] == null
      ? File.fromUri(Platform.script.resolve('process_fixture.dart'))
      : File(Platform.environment['FAILFORGE_TEST_FIXTURE']!);
  final dart =
      Platform.environment['FAILFORGE_TEST_DART'] ??
      Platform.resolvedExecutable;
  exit(await runNative(dart, [fixture.path, ...arguments]));
}

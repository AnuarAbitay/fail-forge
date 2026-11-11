import 'dart:io';

import 'package:failforge/src/process_launcher.dart';

Future<void> main(List<String> arguments) async {
  exitCode = await runNative(arguments.first, arguments.skip(1).toList());
}

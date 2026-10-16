import 'dart:convert';
import 'dart:io';

import 'package:failforge_dart_adapter/src/isolation/namespace_plan.dart';
import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';

/// One JSON response; no user-project writes and no dependency resolution.
Future<void> main(List<String> arguments) async {
  try {
    if (arguments.length != 1) {
      throw AdapterException(
        'invalid_payload',
        'Expected one namespace payload JSON file.',
      );
    }
    final handle = File(arguments.single).openSync();
    final List<int> bytes;
    try {
      bytes = handle.readSync(8 * 1024 * 1024 + 1);
    } finally {
      handle.closeSync();
    }
    if (bytes.length > 8 * 1024 * 1024) {
      throw AdapterException(
        'invalid_payload',
        'Namespace payload exceeds 8 MiB.',
      );
    }
    final payload = jsonObject(
      jsonDecode(utf8.decode(bytes)),
      'namespace payload',
    );
    stdout.writeln(jsonEncode(await planNamespace(payload)));
  } on AdapterException catch (error) {
    stderr.writeln(jsonEncode(error.toJson()));
    exitCode = 1;
  } on Object {
    stderr.writeln(
      jsonEncode({
        'code': 'namespace_planning_failed',
        'message': 'Namespace payload or prepared analysis resources could not be read.',
      }),
    );
    exitCode = 1;
  }
}

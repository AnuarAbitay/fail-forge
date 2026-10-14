import 'dart:io';

import 'package:failforge_dart_adapter/src/analysis/adapter_engine.dart';
import 'package:failforge_dart_adapter/src/protocol/protocol_server.dart';

Future<void> main() async {
  final engine = AdapterEngine();
  await ProtocolServer(handle: engine.handle).run(stdin, stdout);
}

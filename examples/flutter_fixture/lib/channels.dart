import 'package:flutter/services.dart' as services;

Future<String?> readValue(services.MethodChannel channel) async {
  return await channel.invokeMethod<String>('read');
}

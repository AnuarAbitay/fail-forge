import 'dart:convert';
import 'dart:io';

import 'package:failforge_platform_sdk_gate/value.dart';
import 'package:test/test.dart';

import 'real_gate_barrier.dart';

void main() {
  test('enabled', () async {
    await assertEnabledAfterGate(
      value: enabled(),
      journal: Platform.environment['FF_PLATFORM_REAL_GATE_JOURNAL'],
      nonce: Platform.environment['FF_PLATFORM_REAL_GATE_BARRIER_NONCE'],
      emitMarker: (marker) => print(jsonEncode(marker)),
      assertion: (value) => expect(value, isTrue),
    );
  });
}

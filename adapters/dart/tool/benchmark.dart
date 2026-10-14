import 'dart:convert';
import 'dart:io';

import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';

Future<void> main(List<String> args) async {
  if (args.length != 4) {
    throw ArgumentError(
      'WORKSPACE_ROOT PACKAGE_ROOT DART_SDK_ROOT FLUTTER_SDK_ROOT',
    );
  }
  final engine = AdapterEngine();
  final payload = <String, Object?>{
    'workspaceRoot': args[0],
    'packageRoot': args[1],
    'dartSdkPath': args[2],
    'flutterSdkPath': args[3],
    'limits': {'maxMutants': 1000},
    'rules': <String, Object?>{},
  };
  const warmup = 2;
  const samples = 10;
  final raw = <Map<String, Object?>>[];
  Map<String, Object?>? selected;
  for (var index = -warmup; index < samples; index++) {
    final timer = Stopwatch()..start();
    final result = await engine.handle('discover', payload);
    final micros = timer.elapsedMicroseconds;
    final mutants = (result['mutants'] as List).cast<Map<String, Object?>>();
    selected ??= mutants.firstWhere(
      (m) => m['operatorId'] == 'BOOLEAN_LITERAL_FLIP',
    );
    if (index >= 0)
      raw.add({
        'stage': 'discover',
        'sample': index,
        'requestMicros': micros,
        'metrics': result['metrics'],
        'candidates': mutants.length,
      });
  }
  for (var index = -warmup; index < samples; index++) {
    final timer = Stopwatch()..start();
    final result = await engine.handle('validate', {
      ...payload,
      'mutant': selected,
    });
    final micros = timer.elapsedMicroseconds;
    if (result['valid'] != true)
      throw StateError('Benchmark candidate must validate');
    if (index >= 0)
      raw.add({
        'stage': 'validate',
        'sample': index,
        'requestMicros': micros,
        'analysisValidationMicros': result['durationMicros'],
      });
  }
  stdout.writeln(
    jsonEncode({
      'dartVersion': Platform.version,
      'operatingSystem': Platform.operatingSystem,
      'processors': Platform.numberOfProcessors,
      'warmupPerStage': warmup,
      'samplesPerStage': samples,
      'raw': raw,
      'scope': 'classic default operators over examples/flutter_fixture lib',
      'includes': 'request analysis-context creation/disposal; excludes VM startup and Flutter test execution',
    }),
  );
}

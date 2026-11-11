import 'dart:async';
import 'dart:convert';
import 'dart:io';

Future<void> main(List<String> arguments) async {
  switch (arguments.first) {
    case 'echo':
      final input = await stdin.fold<List<int>>(
        <int>[],
        (bytes, chunk) => bytes..addAll(chunk),
      );
      stdout.write(
        jsonEncode({
          'arguments': arguments.skip(1).toList(),
          'cwd': Directory.current.path,
          'inputBase64': base64Encode(input),
        }),
      );
      stderr.write('fixture stderr: ошибка\n');
      await Future.wait([stdout.flush(), stderr.flush()]);
      exit(37);
    case 'signal':
      var count = 0;
      final done = Completer<void>();
      final subscriptions = <StreamSubscription<ProcessSignal>>[];
      void received(ProcessSignal signal) {
        count++;
        stdout.writeln('signal:${signal.signalNumber}:$count');
        if (count == 1) {
          Future<void>.delayed(const Duration(milliseconds: 350), () async {
            stdout.writeln('cleanup:complete:$count');
            await stdout.flush();
            exit(128 + signal.signalNumber);
          });
        }
      }

      subscriptions.add(ProcessSignal.sigint.watch().listen(received));
      if (!Platform.isWindows) {
        subscriptions.add(ProcessSignal.sigterm.watch().listen(received));
      }
      stdout.writeln('ready');
      await stdout.flush();
      await done.future;
      await Future.wait(
        subscriptions.map((subscription) => subscription.cancel()),
      );
    default:
      stderr.writeln('Unknown fixture mode.');
      exit(64);
  }
}

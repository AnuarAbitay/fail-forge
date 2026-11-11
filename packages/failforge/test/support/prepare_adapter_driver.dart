import 'dart:async';
import 'dart:io';

import 'package:failforge/src/runtime_manager.dart';

Future<void> main(List<String> arguments) async {
  if (arguments.first == 'pub-child') {
    final marker = File(arguments[1]);
    var count = 0;
    void received(ProcessSignal signal) {
      count++;
      stdout.writeln('signal:${signal.signalNumber}:$count');
      if (count == 1) {
        unawaited(
          Future<void>.delayed(const Duration(milliseconds: 350), () async {
            await marker.writeAsString(
              '${signal.signalNumber}:$count',
              flush: true,
            );
            stdout.writeln('cleanup:complete:$count');
            await stdout.flush();
            // Successful child cleanup must still be reported as cancellation.
            exit(0);
          }),
        );
      }
    }

    ProcessSignal.sigint.watch().listen(received);
    ProcessSignal.sigterm.watch().listen(received);
    stdout.writeln('ready:$pid');
    await stdout.flush();
    await Completer<void>().future;
    return;
  }

  try {
    await prepareAdapter(arguments[0], Directory(arguments[1]));
    stdout.writeln('unexpected-success');
    exitCode = 70;
  } on SetupCancelled {
    stdout.writeln('cancelled:SetupCancelled');
    exitCode = 130;
  }
}

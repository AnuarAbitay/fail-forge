import 'dart:ffi';
import 'dart:io';

import 'package:path/path.dart' as p;

/// Dart's Windows VM broker does not preserve Ctrl+Break while waiting for its
/// VM child. Require the standard native CLI installation before any setup or
/// campaign starts, rather than leaving a campaign running after the broker
/// has returned control to its caller.
void ensureSupportedWindowsEntrypoint({
  bool? windows,
  String? resolvedExecutable,
}) {
  if (!(windows ?? Platform.isWindows)) return;
  final name = p.windows
      .basename(resolvedExecutable ?? Platform.resolvedExecutable)
      .toLowerCase();
  if (name == 'dart.exe' || name == 'dartvm.exe') {
    throw UnsupportedError(
      'Windows requires the native FailForge launcher to preserve Ctrl+Break '
      'and wait for campaign cleanup. Install it with `dart install failforge` '
      'and add its installation bin directory to PATH. Remove a legacy Pub '
      'batch entry first with `dart pub global deactivate failforge`. '
      'For a local checkout use '
      '`dart install "failforge@{path: C:/path/to/packages/failforge}"`.',
    );
  }
}

typedef _ConsoleHandler = Int32 Function(Uint32);
typedef _SetHandlerNative = Int32 Function(
  Pointer<NativeFunction<_ConsoleHandler>>,
  Int32,
);
typedef _SetHandler = int Function(
  Pointer<NativeFunction<_ConsoleHandler>>,
  int,
);

// This callback runs on a Windows-created thread. It must be trivially
// shareable and must not access isolate-local state. The native CLI receives
// these same events and owns graceful/forced cancellation; forwarding another
// signal here would accidentally escalate the first Ctrl+C.
int _observeConsoleEvent(int event) => event == 0 || event == 1 ? 1 : 0;

final _kernel = DynamicLibrary.open('kernel32.dll');
final _setHandler = _kernel.lookupFunction<_SetHandlerNative, _SetHandler>(
  'SetConsoleCtrlHandler',
);
final _getLastError = _kernel.lookupFunction<Uint32 Function(), int Function()>(
  'GetLastError',
);
NativeCallable<_ConsoleHandler>? _retainedCallback;
var _leases = 0;

/// Keeps an AOT launcher's console alive until all owned child waits finish.
///
/// The callback is retained for the process lifetime: unregistering a handler
/// does not synchronize with an already dispatched Windows callback, so closing
/// its function pointer on the Dart thread would create a use-after-close race.
/// It does not keep the isolate alive and owns no user data or OS handle.
void Function() preserveWindowsConsoleSignals() {
  final callback = _retainedCallback ??=
      NativeCallable<_ConsoleHandler>.isolateGroupBound(
        _observeConsoleEvent,
        exceptionalReturn: 0,
      )..keepIsolateAlive = false;
  if (_leases == 0 && _setHandler(callback.nativeFunction, 1) == 0) {
    throw OSError(
      'Cannot install the Windows console handler',
      _getLastError(),
    );
  }
  _leases++;
  var released = false;
  return () {
    if (released) return;
    released = true;
    _leases--;
    if (_leases == 0 && _setHandler(callback.nativeFunction, 0) == 0) {
      throw OSError(
        'Cannot remove the Windows console handler',
        _getLastError(),
      );
    }
  };
}

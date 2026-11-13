import 'dart:ffi' as ffi;
import 'dart:io';

typedef _GetProcessHeapNative = ffi.Pointer<ffi.Void> Function();
typedef _GetProcessHeap = ffi.Pointer<ffi.Void> Function();
typedef _HeapAllocNative = ffi.Pointer<ffi.Void> Function(
  ffi.Pointer<ffi.Void>,
  ffi.Uint32,
  ffi.UintPtr,
);
typedef _HeapAlloc = ffi.Pointer<ffi.Void> Function(
  ffi.Pointer<ffi.Void>,
  int,
  int,
);
typedef _HeapFreeNative = ffi.Int32 Function(
  ffi.Pointer<ffi.Void>,
  ffi.Uint32,
  ffi.Pointer<ffi.Void>,
);
typedef _HeapFree = int Function(
  ffi.Pointer<ffi.Void>,
  int,
  ffi.Pointer<ffi.Void>,
);
typedef _CreateFileNative = ffi.Pointer<ffi.Void> Function(
  ffi.Pointer<ffi.Uint16>,
  ffi.Uint32,
  ffi.Uint32,
  ffi.Pointer<ffi.Void>,
  ffi.Uint32,
  ffi.Uint32,
  ffi.Pointer<ffi.Void>,
);
typedef _CreateFile = ffi.Pointer<ffi.Void> Function(
  ffi.Pointer<ffi.Uint16>,
  int,
  int,
  ffi.Pointer<ffi.Void>,
  int,
  int,
  ffi.Pointer<ffi.Void>,
);
typedef _WriteFileNative = ffi.Int32 Function(
  ffi.Pointer<ffi.Void>,
  ffi.Pointer<ffi.Void>,
  ffi.Uint32,
  ffi.Pointer<ffi.Uint32>,
  ffi.Pointer<ffi.Void>,
);
typedef _WriteFile = int Function(
  ffi.Pointer<ffi.Void>,
  ffi.Pointer<ffi.Void>,
  int,
  ffi.Pointer<ffi.Uint32>,
  ffi.Pointer<ffi.Void>,
);
typedef _CloseHandleNative = ffi.Int32 Function(ffi.Pointer<ffi.Void>);
typedef _CloseHandle = int Function(ffi.Pointer<ffi.Void>);
typedef _GetLastErrorNative = ffi.Uint32 Function();
typedef _GetLastError = int Function();

/// Test-only publisher analogue: retains GENERIC_WRITE and DELETE access.
class WindowsDeleteWriter {
  WindowsDeleteWriter(this.path) {
    if (!Platform.isWindows)
      throw UnsupportedError('Windows-only writer probe');
    final units = path.codeUnits;
    final memory = _api.allocate((units.length + 1) * 2);
    try {
      final wide = memory.cast<ffi.Uint16>();
      wide.asTypedList(units.length + 1)
        ..setAll(0, units)
        ..[units.length] = 0;
      final handle = _api.open(
        wide,
        0x40010000,
        7,
        ffi.nullptr,
        3,
        0x80,
        ffi.nullptr,
      );
      if (handle == ffi.Pointer<ffi.Void>.fromAddress(-1)) {
        throw FileSystemException(
          'Create DELETE writer',
          path,
          OSError('Windows error', _api.lastError()),
        );
      }
      _handle = handle;
    } finally {
      _api.release(memory);
    }
  }

  final String path;
  final _api = _WriterApi.instance;
  ffi.Pointer<ffi.Void>? _handle;
  bool get isOpen => _handle != null;

  void writeBytes(List<int> bytes) {
    final handle = _handle;
    if (handle == null) throw StateError('Writer is closed');
    final memory = _api.allocate(bytes.isEmpty ? 1 : bytes.length);
    ffi.Pointer<ffi.Void>? countMemory;
    try {
      countMemory = _api.allocate(4);
      memory.cast<ffi.Uint8>().asTypedList(bytes.length).setAll(0, bytes);
      final count = countMemory.cast<ffi.Uint32>();
      if (_api.write(handle, memory, bytes.length, count, ffi.nullptr) == 0) {
        throw FileSystemException(
          'Write retained journal',
          path,
          OSError('Windows error', _api.lastError()),
        );
      }
      if (count.value != bytes.length)
        throw StateError('Incomplete writer probe payload');
    } finally {
      if (countMemory != null) _api.release(countMemory);
      _api.release(memory);
    }
  }

  void close() {
    final handle = _handle;
    if (handle == null) return;
    _handle = null;
    if (_api.close(handle) == 0) {
      throw FileSystemException(
        'Close DELETE writer',
        path,
        OSError('Windows error', _api.lastError()),
      );
    }
  }
}

class _WriterApi {
  static final instance = _WriterApi();
  _WriterApi() {
    final kernel = ffi.DynamicLibrary.open('kernel32.dll');
    heap = kernel.lookupFunction<_GetProcessHeapNative, _GetProcessHeap>(
      'GetProcessHeap',
    )();
    alloc = kernel.lookupFunction<_HeapAllocNative, _HeapAlloc>('HeapAlloc');
    free = kernel.lookupFunction<_HeapFreeNative, _HeapFree>('HeapFree');
    open = kernel.lookupFunction<_CreateFileNative, _CreateFile>('CreateFileW');
    write = kernel.lookupFunction<_WriteFileNative, _WriteFile>('WriteFile');
    close = kernel.lookupFunction<_CloseHandleNative, _CloseHandle>(
      'CloseHandle',
    );
    lastError = kernel.lookupFunction<_GetLastErrorNative, _GetLastError>(
      'GetLastError',
    );
    if (heap == ffi.nullptr) throw StateError('GetProcessHeap failed');
  }
  late final ffi.Pointer<ffi.Void> heap;
  late final _HeapAlloc alloc;
  late final _HeapFree free;
  late final _CreateFile open;
  late final _WriteFile write;
  late final _CloseHandle close;
  late final _GetLastError lastError;

  ffi.Pointer<ffi.Void> allocate(int size) {
    final memory = alloc(heap, 0, size);
    if (memory == ffi.nullptr)
      throw StateError('HeapAlloc failed in DELETE writer probe');
    return memory;
  }

  void release(ffi.Pointer<ffi.Void> memory) {
    if (free(heap, 0, memory) == 0)
      throw StateError('HeapFree failed in DELETE writer probe');
  }
}

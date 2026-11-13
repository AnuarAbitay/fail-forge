import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:io';
import 'dart:typed_data';

const maxGateJournalBytes = 1024 * 1024;

/// Reads the live journal without denying the publisher's retained DELETE
/// access on Windows. Dart's ordinary File open can use incompatible sharing.
String readGateJournal(String path) {
  if (!Platform.isWindows) return File(path).readAsStringSync();
  return _WindowsReader.instance.read(path);
}

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
typedef _ReadFileNative = ffi.Int32 Function(
  ffi.Pointer<ffi.Void>,
  ffi.Pointer<ffi.Void>,
  ffi.Uint32,
  ffi.Pointer<ffi.Uint32>,
  ffi.Pointer<ffi.Void>,
);
typedef _ReadFile = int Function(
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

class _WindowsReader {
  // Static initialization is lazy: Linux never attempts to load kernel32.
  static final instance = _WindowsReader();

  _WindowsReader() {
    final kernel = ffi.DynamicLibrary.open('kernel32.dll');
    heap = kernel.lookupFunction<_GetProcessHeapNative, _GetProcessHeap>(
      'GetProcessHeap',
    )();
    alloc = kernel.lookupFunction<_HeapAllocNative, _HeapAlloc>('HeapAlloc');
    free = kernel.lookupFunction<_HeapFreeNative, _HeapFree>('HeapFree');
    open = kernel.lookupFunction<_CreateFileNative, _CreateFile>('CreateFileW');
    readFile = kernel.lookupFunction<_ReadFileNative, _ReadFile>('ReadFile');
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
  late final _ReadFile readFile;
  late final _CloseHandle close;
  late final _GetLastError lastError;

  ffi.Pointer<ffi.Void> allocate(int bytes, String path) {
    final pointer = alloc(heap, 0, bytes);
    if (pointer == ffi.nullptr) {
      throw FileSystemException(
        'Allocating shared journal buffer failed',
        path,
        const OSError('HeapAlloc failed', 8),
      );
    }
    return pointer;
  }

  FileSystemException failure(String operation, String path, int code) {
    final error = OSError('Windows shared journal $operation failed', code);
    if (code == 2 || code == 3) {
      return PathNotFoundException(path, error, operation);
    }
    return FileSystemException(operation, path, error);
  }

  String read(String path) {
    final units = path.codeUnits;
    if (units.isEmpty || units.contains(0) || units.length > 32766) {
      throw ArgumentError.value(path, 'path', 'Invalid journal path');
    }
    final allocations = <ffi.Pointer<ffi.Void>>[];
    ffi.Pointer<ffi.Void>? handle;
    var completed = false;
    try {
      final pathMemory = allocate((units.length + 1) * 2, path);
      allocations.add(pathMemory);
      final widePath = pathMemory.cast<ffi.Uint16>();
      widePath.asTypedList(units.length + 1)
        ..setAll(0, units)
        ..[units.length] = 0;
      final opened = open(
        widePath,
        0x80000000, // GENERIC_READ only.
        7, // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
        ffi.nullptr,
        3, // OPEN_EXISTING.
        0x80, // FILE_ATTRIBUTE_NORMAL, synchronous handle.
        ffi.nullptr,
      );
      if (opened == ffi.Pointer<ffi.Void>.fromAddress(-1)) {
        throw failure('Open live journal', path, lastError());
      }
      handle = opened;
      final buffer = allocate(65536, path);
      allocations.add(buffer);
      final lengthMemory = allocate(4, path);
      allocations.add(lengthMemory);
      final readLength = lengthMemory.cast<ffi.Uint32>();
      final bytes = BytesBuilder(copy: false);
      var total = 0;
      while (true) {
        final remaining = maxGateJournalBytes - total + 1;
        final requested = remaining < 65536 ? remaining : 65536;
        if (readFile(opened, buffer, requested, readLength, ffi.nullptr) == 0) {
          throw failure('Read live journal', path, lastError());
        }
        final count = readLength.value;
        if (count > requested) throw StateError('ReadFile exceeded its buffer');
        if (count == 0) break;
        total += count;
        if (total > maxGateJournalBytes) {
          throw FileSystemException('Journal exceeds 1 MiB limit', path);
        }
        bytes.add(
          Uint8List.fromList(buffer.cast<ffi.Uint8>().asTypedList(count)),
        );
      }
      final text = utf8.decode(bytes.takeBytes());
      completed = true;
      return text;
    } finally {
      // Preserve an in-flight IO/decoding failure. Cleanup failures on an
      // otherwise successful read must not be reported as a successful proof.
      int? closeError;
      if (handle != null && close(handle) == 0) closeError = lastError();
      var freeFailed = false;
      for (final memory in allocations.reversed) {
        if (free(heap, 0, memory) == 0) freeFailed = true;
      }
      if (completed && closeError != null) {
        throw failure('Close live journal', path, closeError);
      }
      if (completed && freeFailed) {
        throw StateError('HeapFree failed after reading live journal');
      }
    }
  }
}

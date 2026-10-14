import 'dart:convert';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';

import '../patches/patch.dart';
import '../protocol/adapter_error.dart';

/// Exact snapshot bytes and analyzer text, with a single O(n) offset index.
class SourceFile {
  SourceFile(this.relativePath, this.absolutePath, List<int> bytes)
    : rawBytes = _boundedBytes(bytes) {
    // Utf8Decoder consumes a leading BOM. A sentinel makes the original BOM a
    // normal code point, preserving analyzer offsets and byte-for-byte hashes.
    final prefixed = Uint8List(rawBytes.length + 1)..[0] = 0x20;
    prefixed.setRange(1, prefixed.length, rawBytes);
    try {
      text = utf8.decode(prefixed, allowMalformed: false).substring(1);
    } on FormatException {
      throw AdapterException('invalid_utf8', 'source is not valid UTF-8');
    }
    sourceHash = sha256.convert(rawBytes).toString();
    _offsets = Int32List(text.length + 1)..fillRange(0, text.length + 1, -1);
    _boundaries = Uint8List(rawBytes.length + 1);
    var bytesOffset = 0;
    for (var unitsOffset = 0; unitsOffset < text.length; unitsOffset++) {
      _offsets[unitsOffset] = bytesOffset;
      _boundaries[bytesOffset] = 1;
      final unit = text.codeUnitAt(unitsOffset);
      if (unit <= 0x7f) {
        bytesOffset++;
      } else if (unit <= 0x7ff) {
        bytesOffset += 2;
      } else if (unit >= 0xd800 && unit <= 0xdbff) {
        // Strict UTF-8 decoding guarantees a paired surrogate here. The inner
        // UTF-16 boundary stays -1 and cannot be used for a mutation range.
        unitsOffset++;
        bytesOffset += 4;
      } else {
        bytesOffset += 3;
      }
    }
    _offsets[text.length] = bytesOffset;
    _boundaries[bytesOffset] = 1;
    if (bytesOffset != rawBytes.length) {
      throw StateError('UTF-8 offset index does not match snapshot bytes');
    }
  }

  static const maxSourceBytes = 4 * 1024 * 1024;
  static Uint8List _boundedBytes(List<int> bytes) {
    if (bytes.length > maxSourceBytes) {
      throw AdapterException(
        'source_too_large',
        'source exceeds its byte limit',
      );
    }
    if (bytes.any((byte) => byte < 0 || byte > 255)) {
      throw AdapterException(
        'invalid_source_bytes',
        'source values must be bytes',
      );
    }
    return Uint8List.fromList(bytes).asUnmodifiableView();
  }

  final String relativePath;
  final String absolutePath;
  final Uint8List rawBytes;
  Uint8List get bytes => rawBytes;
  late final String text;
  // Analyzer's resolved content removes one leading BOM. Keep that view
  // distinct from exact snapshot text and translate its spans explicitly.
  late final String analysisText = text.startsWith('\ufeff')
      ? text.substring(1)
      : text;
  late final String sourceHash;
  late final Int32List _offsets;
  late final Uint8List _boundaries;

  int byteOffset(int utf16Offset) {
    if (utf16Offset < 0 ||
        utf16Offset >= _offsets.length ||
        _offsets[utf16Offset] < 0) {
      throw AdapterException(
        'invalid_range',
        'range splits a surrogate or exceeds the source',
      );
    }
    return _offsets[utf16Offset];
  }

  bool isByteBoundary(int offset) =>
      offset >= 0 && offset < _boundaries.length && _boundaries[offset] == 1;

  ByteEdit editFromUtf16(int offset, int length, String replacement) {
    if (length < 0) {
      throw AdapterException(
        'invalid_range',
        'range length cannot be negative',
      );
    }
    final start = byteOffset(offset);
    final end = byteOffset(offset + length);
    final expected = text.substring(offset, offset + length);
    return ByteEdit(
      offsetBytes: start,
      lengthBytes: end - start,
      expected: expected,
      replacement: replacement,
    );
  }

  ByteEdit editFromAnalyzer(int offset, int length, String replacement) {
    if (offset < 0) {
      throw AdapterException(
        'invalid_range',
        'analyzer range offset cannot be negative',
      );
    }
    return editFromUtf16(
      offset + (text.startsWith('\ufeff') ? 1 : 0),
      length,
      replacement,
    );
  }
}

import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'adapter_error.dart';

const protocolVersion = 1;
const maxProtocolLineBytes = 8 * 1024 * 1024;

typedef RequestHandler = Future<Map<String, Object?>> Function(
  String command,
  Map<String, Object?> payload,
);

/// Sequential JSON Lines framing with bounded input and response bodies.
class ProtocolServer {
  ProtocolServer({
    required this.handle,
    this.lineLimitBytes = maxProtocolLineBytes,
  }) {
    if (lineLimitBytes < 256 || lineLimitBytes > maxProtocolLineBytes) {
      throw ArgumentError.value(lineLimitBytes, 'lineLimitBytes');
    }
  }

  final RequestHandler handle;
  final int lineLimitBytes;

  Future<void> run(Stream<List<int>> input, IOSink output) async {
    var builder = BytesBuilder(copy: false);
    var size = 0;
    var oversized = false;
    try {
      await for (final chunk in input) {
        var start = 0;
        for (var index = 0; index < chunk.length; index++) {
          if (chunk[index] != 0x0a) continue;
          final length = index - start;
          if (!oversized && size + length <= lineLimitBytes) {
            builder.add(chunk.sublist(start, index));
            size += length;
          } else {
            oversized = true;
          }
          if (oversized) {
            await _write(
              output,
              _error(
                '',
                AdapterException(
                  'request_too_large',
                  'request exceeds its byte limit',
                ),
              ),
            );
          } else {
            await _process(builder.takeBytes(), output);
          }
          builder = BytesBuilder(copy: false);
          size = 0;
          oversized = false;
          start = index + 1;
        }
        final length = chunk.length - start;
        if (!oversized && size + length <= lineLimitBytes) {
          builder.add(chunk.sublist(start));
          size += length;
        } else {
          oversized = true;
          builder = BytesBuilder(copy: false);
          size = 0;
        }
      }
      // A final record without a newline is accepted; empty EOF emits nothing.
      if (oversized) {
        await _write(
          output,
          _error(
            '',
            AdapterException(
              'request_too_large',
              'request exceeds its byte limit',
            ),
          ),
        );
      } else if (size != 0) {
        await _process(builder.takeBytes(), output);
      }
    } on IOException {
      // Do not echo stream errors, filenames, or the incomplete input record.
      await _write(
        output,
        _error(
          '',
          AdapterException('input_error', 'protocol input could not be read'),
        ),
      );
    }
    await output.flush();
  }

  Future<void> _process(Uint8List bytes, IOSink output) async {
    var requestId = '';
    try {
      Object? decoded;
      try {
        decoded = jsonDecode(utf8.decode(bytes, allowMalformed: false));
      } on FormatException {
        throw AdapterException(
          'malformed_json',
          'request is not strict UTF-8 JSON',
        );
      }
      final request = jsonObject(decoded, 'request');
      final suppliedId = request['requestId'];
      if (suppliedId is String &&
          suppliedId.isNotEmpty &&
          suppliedId.length <= 256 &&
          !suppliedId.contains('\u0000')) {
        // Only a bounded, valid ID is reflected in failures. An invalid ID must
        // not defeat the response bound or serialize an unpaired surrogate.
        checkUnicode(suppliedId);
        if (utf8.encode(suppliedId).length <= 256) requestId = suppliedId;
      }
      onlyKeys(request, {
        'protocolVersion',
        'requestId',
        'command',
        'payload',
      }, 'request');
      requestId = jsonString(request['requestId'], 'requestId');
      if (utf8.encode(requestId).length > 256 || requestId.contains('\u0000')) {
        requestId = '';
        throw AdapterException(
          'invalid_request',
          'requestId exceeds its length limit',
        );
      }
      if (request['protocolVersion'] is! int ||
          request['protocolVersion'] != protocolVersion) {
        throw AdapterException(
          'incompatible_protocol',
          'protocolVersion 1 is required',
        );
      }
      final command = jsonString(request['command'], 'command');
      if (!{'handshake', 'discover', 'validate'}.contains(command)) {
        throw AdapterException('unknown_command', 'command is not supported');
      }
      final payload = jsonObject(request['payload'], 'payload');
      if (command == 'handshake') onlyKeys(payload, {}, 'handshake payload');
      final result = await handle(command, payload);
      await _write(output, {
        'protocolVersion': protocolVersion,
        'requestId': requestId,
        'kind': command,
        'payload': result,
      });
    } on AdapterException catch (error) {
      await _write(output, _error(requestId, error));
    } catch (_) {
      await _write(
        output,
        _error(
          requestId,
          AdapterException(
            'internal_error',
            'adapter could not process the request',
          ),
        ),
      );
    }
  }

  Map<String, Object?> _error(String requestId, AdapterException error) => {
    'protocolVersion': protocolVersion,
    'requestId': requestId,
    'kind': 'error',
    'error': error.toJson(),
  };

  Future<void> _write(IOSink output, Map<String, Object?> response) async {
    var bytes = utf8.encode(jsonEncode(response));
    if (bytes.length > lineLimitBytes) {
      bytes = utf8.encode(
        jsonEncode(
          _error(
            response['requestId'] as String,
            AdapterException(
              'response_too_large',
              'response exceeds its byte limit; no complete result was returned',
            ),
          ),
        ),
      );
      if (bytes.length > lineLimitBytes) {
        bytes = utf8.encode(
          jsonEncode(
            _error(
              '',
              AdapterException(
                'response_too_large',
                'response exceeds its byte limit',
              ),
            ),
          ),
        );
      }
    }
    output.add(bytes);
    output.add(const [0x0a]);
    await output.flush();
  }
}

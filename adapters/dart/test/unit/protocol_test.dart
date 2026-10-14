import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';
import 'package:failforge_dart_adapter/src/protocol/protocol_server.dart';
import 'package:test/test.dart';

Map<String, Object?> request({
  String id = 'r1',
  Object? version = 1,
  String command = 'handshake',
  Object? payload = const <String, Object?>{},
}) => {
  'protocolVersion': version,
  'requestId': id,
  'command': command,
  'payload': payload,
};

Future<List<Map<String, Object?>>> exchange(
  List<List<int>> chunks, {
  RequestHandler? handler,
  int limit = maxProtocolLineBytes,
}) async {
  final outputChunks = StreamController<List<int>>();
  final bytes = BytesBuilder();
  final done = Completer<void>();
  outputChunks.stream.listen(bytes.add, onDone: done.complete);
  final output = IOSink(outputChunks.sink);
  await ProtocolServer(
    handle: handler ?? (_, _) async => {'ok': true},
    lineLimitBytes: limit,
  ).run(Stream.fromIterable(chunks), output);
  await output.close();
  await done.future;
  final text = utf8.decode(bytes.takeBytes(), allowMalformed: false);
  if (text.isEmpty) return [];
  expect(text.endsWith('\n'), isTrue);
  return text
      .substring(0, text.length - 1)
      .split('\n')
      .map((line) => jsonDecode(line) as Map<String, Object?>)
      .toList();
}

void main() {
  test(
    'NUL request ID rejects without reflecting the invalid header',
    () async {
      final responses = await exchange([
        utf8.encode('${jsonEncode(request(id: 'r\u0000x'))}\n'),
      ]);
      expect(responses.single['kind'], 'error');
      expect(responses.single['requestId'], '');
    },
  );
  test('request ID uses the shared 256 UTF-8 byte bound', () async {
    final boundary = List.filled(128, 'я').join();
    final accepted = await exchange([
      utf8.encode('${jsonEncode(request(id: boundary))}\n'),
    ]);
    expect(accepted.single['kind'], 'handshake');
    expect(accepted.single['requestId'], boundary);
    final oversized = '${boundary}я';
    final rejected = await exchange([
      utf8.encode('${jsonEncode(request(id: oversized))}\n'),
    ]);
    expect(rejected.single['kind'], 'error');
    expect(rejected.single['requestId'], '');
  });
  test('arbitrary chunk boundaries, CRLF, sequential commands and final EOF record work', () async {
    final calls = <String>[];
    final text =
        '${jsonEncode(request())}\r\n${jsonEncode(request(id: 'r2', command: 'discover'))}\n${jsonEncode(request(id: 'r3', command: 'validate'))}';
    final raw = utf8.encode(text);
    final responses = await exchange(
      [raw.sublist(0, 3), raw.sublist(3, 11), raw.sublist(11)],
      handler: (command, _) async {
        calls.add(command);
        return {'sequence': calls.length};
      },
    );
    expect(calls, ['handshake', 'discover', 'validate']);
    expect(responses.map((response) => response['requestId']), [
      'r1',
      'r2',
      'r3',
    ]);
    expect(responses.map((response) => response['kind']), calls);
    expect(
      responses.map((response) => (response['payload'] as Map)['sequence']),
      [1, 2, 3],
    );
    expect(
      responses.every((response) => response['protocolVersion'] == 1),
      isTrue,
    );
  });

  test('empty EOF produces no messages and no engine call', () async {
    var calls = 0;
    expect(
      await exchange(
        [],
        handler: (_, _) async {
          calls++;
          return {};
        },
      ),
      isEmpty,
    );
    expect(calls, 0);
  });

  test('malformed JSON and invalid UTF-8 are structured errors without input disclosure', () async {
    for (final input in [
      utf8.encode('{PRIVATE_SECRET\n'),
      [0xff, 10],
      utf8.encode('[]\n'),
    ]) {
      final responses = await exchange([input]);
      expect(responses.single['kind'], 'error');
      expect(responses.single['requestId'], '');
      expect(jsonEncode(responses), isNot(contains('PRIVATE_SECRET')));
    }
  });

  test(
    'incompatible versions and unknown commands never reach the engine',
    () async {
      var calls = 0;
      for (final value in [
        request(version: 2),
        request(version: 1.0),
        request(version: true),
        request(command: 'execute'),
      ]) {
        final responses = await exchange(
          [utf8.encode('${jsonEncode(value)}\n')],
          handler: (_, _) async {
            calls++;
            return {};
          },
        );
        expect(responses.single['kind'], 'error');
        expect(responses.single['requestId'], 'r1');
      }
      expect(calls, 0);
    },
  );

  test(
    'unknown fields, wrong types and handshake payload execution fields reject',
    () async {
      for (final value in [
        request()..['unexpected'] = true,
        request(payload: []),
        request(payload: {'command': 'run'}),
        request()..['requestId'] = null,
        request()..['requestId'] = '',
        request()..remove('command'),
      ]) {
        final responses = await exchange([
          utf8.encode('${jsonEncode(value)}\n'),
        ]);
        expect(responses.single['kind'], 'error');
      }
    },
  );

  test(
    'invalid or oversized IDs cannot defeat the structured response bound',
    () async {
      for (final id in [
        String.fromCharCode(0xd800),
        List.filled(257, 'x').join(),
      ]) {
        final responses = await exchange([
          utf8.encode('${jsonEncode(request(id: id))}\n'),
        ]);
        expect(responses.single['kind'], 'error');
        expect(responses.single['requestId'], '');
      }
    },
  );

  test('input limit is byte based and drains exactly one oversized record', () async {
    final valid = jsonEncode(request());
    final padded =
        '$valid${List.filled(256 - utf8.encode(valid).length, ' ').join()}';
    final oversized =
        '$padded '; // 257 bytes, even though trailing whitespace is JSON-valid.
    var calls = 0;
    final responses = await exchange(
      [
        utf8.encode(oversized.substring(0, 200)),
        utf8.encode('${oversized.substring(200)}\n$valid\n'),
      ],
      limit: 256,
      handler: (_, _) async {
        calls++;
        return {'ok': true};
      },
    );
    expect(responses.length, 2);
    expect((responses[0]['error'] as Map)['code'], 'request_too_large');
    expect(responses[1]['kind'], 'handshake');
    expect(calls, 1);
    final boundary = await exchange([utf8.encode('$padded\n')], limit: 256);
    expect(boundary.single['kind'], 'handshake');
  });

  test(
    'oversized final record without newline is not silently ignored',
    () async {
      final responses = await exchange([List.filled(257, 0x20)], limit: 256);
      expect((responses.single['error'] as Map)['code'], 'request_too_large');
    },
  );

  test('oversized output becomes an error rather than a truncated successful result', () async {
    final responses = await exchange(
      [utf8.encode('${jsonEncode(request())}\n')],
      limit: 256,
      handler: (_, _) async => {
        'mutants': [List.filled(1024, 'x').join()],
      },
    );
    expect(responses.single['kind'], 'error');
    expect(responses.single.containsKey('payload'), isFalse);
    expect((responses.single['error'] as Map)['code'], 'response_too_large');
    expect(
      utf8.encode(jsonEncode(responses.single)).length,
      lessThanOrEqualTo(256),
    );
  });

  test('engine failures are typed and unexpected exceptions do not leak source text', () async {
    final typed = await exchange(
      [utf8.encode('${jsonEncode(request())}\n')],
      handler: (_, _) async =>
          throw AdapterException('sdk_missing', 'required SDK missing'),
    );
    expect((typed.single['error'] as Map)['code'], 'sdk_missing');
    final unknown = await exchange([
      utf8.encode('${jsonEncode(request())}\n'),
    ], handler: (_, _) async => throw StateError('PRIVATE_SOURCE_SECRET'));
    expect((unknown.single['error'] as Map)['code'], 'internal_error');
    expect(jsonEncode(unknown), isNot(contains('PRIVATE_SOURCE_SECRET')));
  });
}

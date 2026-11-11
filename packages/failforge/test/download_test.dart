import 'dart:async';
import 'dart:io';

import 'package:failforge/src/runtime_manager.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

import 'support/runtime_fixture.dart';

final _uri = Uri.parse('https://example.invalid/native-release.tar.gz');

class _Client implements HttpClient {
  _Client(this.body);

  final Stream<List<int>> Function() body;
  bool closed = false;
  bool forceClosed = false;

  @override
  Future<HttpClientRequest> getUrl(Uri uri) async => _Request(body);

  @override
  void close({bool force = false}) {
    closed = true;
    forceClosed = force;
  }

  @override
  dynamic noSuchMethod(Invocation invocation) => null;
}

class _Request implements HttpClientRequest {
  _Request(this.body);

  final Stream<List<int>> Function() body;

  @override
  Future<HttpClientResponse> close() async => _Response(body());

  @override
  dynamic noSuchMethod(Invocation invocation) => null;
}

class _Response extends Stream<List<int>> implements HttpClientResponse {
  _Response(this.body);

  final Stream<List<int>> body;

  @override
  int get statusCode => HttpStatus.ok;

  @override
  int get contentLength => -1;

  @override
  StreamSubscription<List<int>> listen(
    void Function(List<int>)? onData, {
    Function? onError,
    void Function()? onDone,
    bool? cancelOnError,
  }) => body.listen(
    onData,
    onError: onError,
    onDone: onDone,
    cancelOnError: cancelOnError,
  );

  @override
  dynamic noSuchMethod(Invocation invocation) => null;
}

class _OutputFile implements File {
  _OutputFile(this.handle);

  final _OutputHandle handle;

  @override
  Future<RandomAccessFile> open({FileMode mode = FileMode.read}) async {
    expect(mode, FileMode.write);
    return handle;
  }

  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

class _OutputHandle implements RandomAccessFile {
  _OutputHandle({this.failureAt});

  final String? failureAt;
  final failure = const FileSystemException('Injected output failure');
  final bytes = <int>[];
  int closeCalls = 0;

  @override
  Future<RandomAccessFile> writeFrom(
    List<int> buffer, [
    int start = 0,
    int? end,
  ]) async {
    if (failureAt == 'write') throw failure;
    bytes.addAll(buffer.sublist(start, end ?? buffer.length));
    return this;
  }

  @override
  Future<RandomAccessFile> flush() async {
    if (failureAt == 'flush') throw failure;
    return this;
  }

  @override
  Future<void> close() async {
    closeCalls++;
    if (failureAt == 'close') throw failure;
  }

  @override
  dynamic noSuchMethod(Invocation invocation) => super.noSuchMethod(invocation);
}

Future<void> _withClient(_Client client, Future<void> Function() action) =>
    HttpOverrides.runZoned(action, createHttpClient: (_) => client);

void main() {
  late Directory temporary;

  setUp(() async {
    temporary = await Directory.systemTemp.createTemp('failforge download ');
  });

  tearDown(() async => temporary.delete(recursive: true));

  test('downloads all bytes and closes the HTTP client', () async {
    final client = _Client(
      () => Stream.fromIterable([
        [0, 1],
        [255, 13, 10],
      ]),
    );
    final output = File(p.join(temporary.path, 'release.tar.gz'));
    await _withClient(client, () => downloadRelease(_uri, output));
    expect(await output.readAsBytes(), [0, 1, 255, 13, 10]);
    expect(client.closed, isTrue);
    expect(client.forceClosed, isTrue);
  });

  test('asynchronous output-open errors reach the caller', () async {
    final client = _Client(() async* {
      // Reproduces the IOSink open error arriving while HTTP is still idle.
      await Future<void>.delayed(const Duration(milliseconds: 50));
      yield [1, 2, 3];
    });
    var finallyReached = false;
    try {
      await expectLater(
        _withClient(client, () => downloadRelease(_uri, File(temporary.path))),
        throwsA(isA<FileSystemException>()),
      );
    } finally {
      finallyReached = true;
    }
    // Any independent, unhandled IOSink.done error fails the test's zone.
    await Future<void>.delayed(const Duration(milliseconds: 100));
    expect(finallyReached, isTrue);
    expect(client.closed, isTrue);
    expect(client.forceClosed, isTrue);
  });

  for (final operation in ['write', 'flush', 'close']) {
    test('$operation errors propagate and still close client/handle', () async {
      final handle = _OutputHandle(failureAt: operation);
      final client = _Client(() => Stream.value([1, 2, 3]));
      await expectLater(
        _withClient(client, () => downloadRelease(_uri, _OutputFile(handle))),
        throwsA(same(handle.failure)),
      );
      expect(handle.closeCalls, 1);
      expect(client.closed, isTrue);
      expect(client.forceClosed, isTrue);
    });
  }

  test('write failure cancels the HTTP body', () async {
    var cancelled = false;
    final handle = _OutputHandle(failureAt: 'write');
    final client = _Client(() async* {
      try {
        yield [1];
        yield [2];
      } finally {
        cancelled = true;
      }
    });
    await expectLater(
      _withClient(client, () => downloadRelease(_uri, _OutputFile(handle))),
      throwsA(same(handle.failure)),
    );
    expect(cancelled, isTrue);
    expect(handle.closeCalls, 1);
  });

  test('partial download is removed and setup can retry', () async {
    final cache = Directory(p.join(temporary.path, 'cache'));
    final clients = <_Client>[];
    final failure = const SocketException('Interrupted release download');
    var attempts = 0;
    var prepared = 0;
    final manager = RuntimeManager(
      release: fixtureRelease,
      cacheRoot: cache,
      dartExecutable: Platform.resolvedExecutable,
      extract: extractFixture,
      prepare: (dart, adapter) async {
        prepared++;
        await prepareFixture(
          adapter,
          Directory(p.join(temporary.path, 'deps')),
        );
      },
    );
    await HttpOverrides.runZoned(
      () async {
        await expectLater(manager.ensureReady(), throwsA(same(failure)));
        expect(await manager.installation.exists(), isFalse);
        expect(prepared, 0);
        expect(
          await cache
              .list()
              .where(
                (entry) =>
                    entry.path.endsWith('.download') ||
                    entry.path.endsWith('.install'),
              )
              .toList(),
          isEmpty,
        );
        final executable = await manager.ensureReady();
        expect(await File(executable).exists(), isTrue);
        expect(prepared, 1);
      },
      createHttpClient: (_) {
        attempts++;
        final client = _Client(() async* {
          if (attempts == 1) {
            yield fixtureArchiveBytes.sublist(0, 5);
            throw failure;
          }
          yield fixtureArchiveBytes;
        });
        clients.add(client);
        return client;
      },
    );
    expect(attempts, 2);
    expect(
      clients.every((client) => client.closed && client.forceClosed),
      isTrue,
    );
    expect(
      await cache
          .list()
          .where(
            (entry) =>
                entry.path.endsWith('.download') ||
                entry.path.endsWith('.install'),
          )
          .toList(),
      isEmpty,
    );
  });
}

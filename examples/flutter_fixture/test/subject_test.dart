import 'package:failforge_flutter_fixture/subject.dart';
import 'package:failforge_flutter_fixture/channels.dart';
import 'package:flutter/services.dart' as services;
import 'package:flutter/widgets.dart';
import 'package:flutter_test/flutter_test.dart';

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();
  test('direct MethodChannel returns the nullable typed result', () async {
    const channel = services.MethodChannel('failforge.fixture');
    final messenger =
        TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;
    messenger.setMockMethodCallHandler(channel, (call) async => 'ok');
    addTearDown(() => messenger.setMockMethodCallHandler(channel, null));
    Future<String?>? result;
    expect(() => result = readValue(channel), returnsNormally);
    expect(result, completion('ok'));
  });

  test('async entry returns a Future before completion', () {
    Future<bool>? result;
    expect(() => result = asyncEnabled(), returnsNormally);
    expect(result, completion(isTrue));
  });

  test('numeric boundary is inclusive', () {
    final subject = RangePolicy();
    expect(subject.accepts(9), isFalse);
    expect(subject.accepts(10), isTrue);
    expect(subject.enabled(), isTrue);
    expect(subject.both(true, false), isFalse);
    expect(fromPart(), isTrue);
    expect(nullableLength('abc'), 3);
    expect(nullableLength(null), 0);
  });

  testWidgets('threshold appears in a widget', (tester) async {
    await tester.pumpWidget(
      Directionality(
        textDirection: TextDirection.ltr,
        child: Text(RangePolicy().accepts(10) ? 'accepted' : 'rejected'),
      ),
    );
    expect(find.text('accepted'), findsOneWidget);
  });
}

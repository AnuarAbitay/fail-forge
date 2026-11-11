import 'package:failforge_example/boundary.dart';
import 'package:test/test.dart';

void main() {
  test('withdrawal boundary', () {
    expect(canWithdraw(10, 10), isTrue);
    expect(canWithdraw(9, 10), isFalse);
    expect(canWithdraw(11, 10), isTrue);
  });
}

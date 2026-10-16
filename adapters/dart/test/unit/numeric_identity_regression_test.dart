import 'package:failforge_dart_adapter/src/operators/classic/calls/argument_permutation.dart';
import 'package:failforge_dart_adapter/src/operators/common.dart';
import 'package:failforge_dart_adapter/src/operators/language_specific/dart/switches/switches.dart';
import 'package:test/test.dart';

import '../support/control_flow_fixture.dart';

void main() {
  Future<ControlFlowFixture> fixture(String source) async {
    final result = await ControlFlowFixture.open(source);
    addTearDown(result.close);
    return result;
  }

  Future<void> valid(
    ControlFlowFixture fixture,
    String source,
    Iterable<CandidateDraft> candidates,
  ) async {
    for (final candidate in candidates) {
      expectNoErrors(await fixture.resolve(apply(source, candidate)));
      expectNoErrors(await fixture.resolve(source));
    }
  }

  for (final operator in const [
    SwitchBranchResultReplacement(),
    SwitchExpressionReplacement(),
  ]) {
    final statement = operator is SwitchBranchResultReplacement;
    test(
      '${operator.id} retains distinct large contextual double literals',
      () async {
        final source = statement
            ? 'double f(bool x) { switch(x) { case true: return 18446744073709551616; case false: return 36893488147419103232; } } '
                  'void main() { print(f(true)); print(f(false)); }'
            : 'double f(bool x) => switch(x) { true => 18446744073709551616, false => 36893488147419103232 }; '
                  'void main() { print(f(true)); print(f(false)); }';
        final f = await fixture(source);
        final baseline = await f.execute(source);
        expect(baseline.map(double.parse), [
          double.parse('18446744073709551616'),
          double.parse('36893488147419103232'),
        ]);
        final scan = await f.scan(operator, source);
        expect(scan.candidates, hasLength(2));
        await valid(f, source, scan.candidates);
        for (final candidate in scan.candidates) {
          expect(await f.execute(apply(source, candidate)), isNot(baseline));
        }
      },
    );

    test('${operator.id} suppresses equal contextual double spellings', () async {
      final source = statement
          ? 'double f(bool x) { switch(x) { case true: return 0x8000000000000000; case false: return 9223372036854775808; } }'
          : 'double f(bool x) => switch(x) { true => 0x8000000000000000, false => 9223372036854775808 };';
      final f = await fixture(source);
      expect((await f.scan(operator, source)).candidates, isEmpty);
    });
  }

  test('argument permutation distinguishes signed hex from contextual double values', () async {
    const source =
        'bool ordered(double a, double b) { print(a); print(b); return a > b; } '
        'void main() { print(ordered(0xFFFF000000000000, -281474976710656)); }';
    final f = await fixture(source);
    final baseline = await f.execute(source);
    expect(baseline.take(2).map(double.parse), [
      double.parse('18446462598732840960'),
      -281474976710656.0,
    ]);
    expect(baseline.last, 'true');
    final scan = await f.scan(const ArgumentPermutation(), source);
    expect(scan.candidates, hasLength(1));
    await valid(f, source, scan.candidates);
    final changed = await f.execute(apply(source, scan.candidates.single));
    expect(changed.take(2), baseline.take(2).toList().reversed);
    expect(changed.last, 'false');
  });

  test(
    'argument permutation retains contextual double negative zero',
    () async {
      const source =
          'void pair(double a, double b) { print(1 / a); print(1 / b); } '
          'void main() { pair(0, -0); }';
      final f = await fixture(source);
      expect(await f.execute(source), ['Infinity', '-Infinity']);
      final scan = await f.scan(const ArgumentPermutation(), source);
      expect(scan.candidates, hasLength(1));
      await valid(f, source, scan.candidates);
      expect(await f.execute(apply(source, scan.candidates.single)), [
        '-Infinity',
        'Infinity',
      ]);
    },
  );

  for (final arguments in [
    '0x8000000000000000, 9223372036854775808',
    '18446744073709551616, 0x10000000000000000',
    '(-0), -0.0',
  ]) {
    test('argument permutation suppresses equal double values: $arguments', () async {
      final source =
          'void pair(double a, double b) {} void main() { pair($arguments); }';
      final f = await fixture(source);
      expect(
        (await f.scan(const ArgumentPermutation(), source)).candidates,
        isEmpty,
      );
    });
  }
}

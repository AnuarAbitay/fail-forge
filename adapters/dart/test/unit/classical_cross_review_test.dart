import 'package:failforge_dart_adapter/src/operators/classic/collections/collection_literal_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/condition_forcing.dart';
import 'package:failforge_dart_adapter/src/operators/classic/control_flow/switch_case_mutation.dart';
import 'package:failforge_dart_adapter/src/operators/classic/returns/return_value_replacement.dart';
import 'package:test/test.dart';

import '../support/call_operator_fixture.dart';

void main() {
  test(
    'typed collection defaults never render a shadowing generic type parameter',
    () async {
      const source = 'Object make<T>(T value) => <T>(T ignored) => [value];';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final collections = const CollectionLiteralMutation().discover(
        unit,
        source,
      );
      final returns = const ReturnValueReplacement().discover(unit, source);
      expect(
        collections.candidates,
        isEmpty,
        reason: 'The captured value has outer T, but spelling T at this literal resolves inner T.',
      );
      expect(
        returns.candidates,
        isEmpty,
        reason: 'The inferred closure return List<outer T> cannot be rendered as List<inner T>.',
      );
      expect(
        collections.skips.map((skip) => skip.code),
        contains('collection_type_unavailable'),
      );
    },
  );

  test(
    'forcing parenthesized literal conditions excludes unchanged truth values',
    () async {
      const source = 'void f() { if ((true)) {} while ((false)) {} }';
      final fixture = CallOperatorFixture.open(source);
      addTearDown(fixture.close);
      final unit = await fixture.resolve();
      CallOperatorFixture.noErrors(unit);
      final scan = const ConditionForcing().discover(unit, source);
      expect(scan.candidates.map((candidate) => candidate.replacement), [
        'false',
        'true',
      ]);
      await fixture.validateAll(scan.candidates);
    },
  );

  test('case replacement excludes semantic no-ops with hexadecimal integer spelling', () async {
    const source =
        'void f(int value) { switch (value) { case 0x1: print(value); break; default: break; } }';
    final fixture = CallOperatorFixture.open(source);
    addTearDown(fixture.close);
    final unit = await fixture.resolve();
    CallOperatorFixture.noErrors(unit);
    final scan = const SwitchCaseMutation().discover(unit, source);
    expect(
      scan.candidates
          .where((candidate) => fixture.before(candidate) == '0x1')
          .map((candidate) => candidate.replacement),
      ['0', '-1'],
    );
  });
}

import 'dart:io';

import 'package:failforge_dart_adapter/failforge_dart_adapter.dart';
import 'package:test/test.dart';

import '../support/project_fixture.dart';

void main() {
  test('different AST edit ranges producing identical file bytes merge before maxMutants', () async {
    const text =
        '\uFEFF// 😀 Кириллица\r\nvoid action(){} void f(bool c){if(c){action();}}\r\n';
    final fixture = ProjectFixture.create({'lib/main.dart': text});
    addTearDown(fixture.close);
    final engine = AdapterEngine();
    final payload = fixture.payload;
    Future<Map<String, Object?>> discover(List<String> ids, {int max = 1000}) =>
        engine.handle('discover', {
          ...payload,
          'rules': {'operators': ids},
          'limits': {'maxMutants': max},
        });
    final branch = await discover(['BRANCH_BODY_REMOVAL']);
    final statement = await discover(['STATEMENT_DELETION']);
    final a = Mutant.fromJson(
      (branch['mutants'] as List).single as Map<String, Object?>,
    );
    final b = Mutant.fromJson(
      (statement['mutants'] as List).single as Map<String, Object?>,
    );
    expect(a.edit.offsetBytes, isNot(b.edit.offsetBytes));
    expect(a.edit.lengthBytes, isNot(b.edit.lengthBytes));
    final snapshot = SafeProject.open(payload).readTarget('lib/main.dart');
    expect(a.edit.apply(snapshot), b.edit.apply(snapshot));
    final both = await discover([
      'BRANCH_BODY_REMOVAL',
      'STATEMENT_DELETION',
    ], max: 1);
    expect(both['rawCandidates'], 2);
    expect(both['totalCandidates'], 1);
    expect(both['deduplicatedCandidates'], 1);
    expect(both['truncated'], false);
    final raw = (both['mutants'] as List).single as Map<String, Object?>;
    expect(
      (await engine.handle('validate', {
        ...payload,
        'rules': {
          'operators': ['BRANCH_BODY_REMOVAL', 'STATEMENT_DELETION'],
        },
        'mutant': raw,
      }))['valid'],
      true,
    );
    expect(
      File('${fixture.root}/lib/main.dart').readAsBytesSync(),
      snapshot.rawBytes,
    );
  });
}

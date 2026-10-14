import 'dart:convert';
import 'dart:io';

import 'package:crypto/crypto.dart';
import 'package:failforge_dart_adapter/src/model/mutation_family.dart';
import 'package:failforge_dart_adapter/src/patches/patch.dart';
import 'package:failforge_dart_adapter/src/project/source_file.dart';
import 'package:failforge_dart_adapter/src/protocol/adapter_error.dart';
import 'package:test/test.dart';

Matcher failure(String code) =>
    isA<AdapterException>().having((error) => error.code, 'code', code);

void main() {
  test('operator ID uses the shared 128 UTF-8 byte bound', () {
    final source = SourceFile('lib/a.dart', '/a', utf8.encode('true'));
    final boundary = List.filled(64, 'я').join();
    Mutant make(String operator) => Mutant.create(
      file: source.relativePath,
      sourceHash: source.sourceHash,
      operatorId: operator,
      family: MutationFamily.classic,
      edit: source.editFromUtf16(0, 4, 'false'),
    );
    expect(make(boundary), isA<Mutant>());
    expect(() => make('${boundary}я'), throwsA(failure('invalid_mutant')));
  });
  for (final category in ['className', 'methodName', 'component']) {
    test('$category uses the shared 512 UTF-8 byte bound', () {
      final source = SourceFile('lib/a.dart', '/a', utf8.encode('true'));
      final boundary = List.filled(256, 'я').join();
      Mutant make(String name) => Mutant.create(
        file: source.relativePath,
        sourceHash: source.sourceHash,
        operatorId: 'BOOLEAN_LITERAL_FLIP',
        family: MutationFamily.classic,
        edit: source.editFromUtf16(0, 4, 'false'),
        className: category == 'className' ? name : null,
        methodName: category == 'methodName' ? name : null,
        components: category == 'component' ? [name] : [],
      );
      expect(make(boundary), isA<Mutant>());
      expect(() => make('${boundary}я'), throwsA(failure('invalid_mutant')));
    });
  }
  test('UTF-16 offsets map Cyrillic, emoji, LF, CRLF and every leading BOM exactly', () {
    for (final prefix in ['', '\ufeff', '\ufeff\ufeff']) {
      for (final newline in ['\n', '\r\n']) {
        final text =
            '$prefix// Кириллица 😀$newline bool value = true;$newline';
        final bytes = utf8.encode(text);
        final source = SourceFile('lib/a.dart', '/snapshot/lib/a.dart', bytes);
        expect(source.text, text);
        expect(source.sourceHash, sha256.convert(bytes).toString());
        final offset = text.indexOf('true');
        final edit = source.editFromUtf16(offset, 4, 'false');
        expect(edit.offsetBytes, utf8.encode(text.substring(0, offset)).length);
        expect(edit.lengthBytes, 4);
        expect(edit.expected, 'true');
        expect(
          edit.apply(source),
          utf8.encode(text.replaceFirst('true', 'false')),
        );
      }
    }
  });

  test('raw bytes are immutable and hashes depend on original BOM and newline bytes', () {
    final bytes = utf8.encode('true\r\n');
    final source = SourceFile('lib/a.dart', '/one/lib/a.dart', bytes);
    bytes[0] = 0;
    expect(source.text, 'true\r\n');
    expect(() => source.rawBytes[0] = 0, throwsUnsupportedError);
    expect(
      source.sourceHash,
      isNot(
        SourceFile(
          'lib/a.dart',
          '/two/lib/a.dart',
          utf8.encode('true\n'),
        ).sourceHash,
      ),
    );
  });

  test(
    'analyzer spans exclude the BOM and still map Unicode to original bytes',
    () {
      for (final prefix in ['', '\ufeff', '\ufeff\ufeff']) {
        final text = '$prefix// Привет 😀\r\nbool x = true;';
        final source = SourceFile('lib/a.dart', '/a', utf8.encode(text));
        final analyzerOffset = source.analysisText.indexOf('true');
        final patch = source.editFromAnalyzer(analyzerOffset, 4, 'false');
        expect(
          patch.offsetBytes,
          utf8.encode(text.substring(0, text.indexOf('true'))).length,
        );
        expect(patch.expected, 'true');
        expect(
          patch.apply(source),
          utf8.encode(text.replaceFirst('true', 'false')),
        );
        expect(
          () => source.editFromAnalyzer(-1, 0, 'x'),
          throwsA(failure('invalid_range')),
        );
      }
    },
  );

  for (final name in ['patch.json', 'patch-unicode-expression.json']) {
    test(
      'the Rust-shared $name fixture has the exact canonical ID and patch bytes',
      () {
        final fixture = jsonDecode(
          File('../../protocol/fixtures/$name').readAsStringSync(),
        ) as Map<String, Object?>;
        final mutant = Mutant.fromJson(
          fixture['mutant'] as Map<String, Object?>,
        );
        final source = SourceFile(
          mutant.file,
          '/snapshot/${mutant.file}',
          utf8.encode(fixture['sourceUtf8'] as String),
        );
        mutant.validateAgainst(source);
        expect(
          mutant.edit.apply(source),
          utf8.encode(fixture['patchedUtf8'] as String),
        );
        expect(
          source
              .editFromAnalyzer(
                source.analysisText.indexOf(mutant.edit.expected),
                mutant.edit.expected.length,
                mutant.edit.replacement,
              )
              .toJson(),
          mutant.edit.toJson(),
        );
        expect(mutant.id, (fixture['mutant'] as Map)['id']);
      },
    );
  }

  test('invalid UTF-8 never becomes replacement characters', () {
    for (final bytes in [
      [0xff],
      [0xc0, 0xaf],
      [0xed, 0xa0, 0x80],
      [0xe2, 0x82],
    ]) {
      expect(
        () => SourceFile('lib/a.dart', '/a', bytes),
        throwsA(failure('invalid_utf8')),
      );
    }
  });

  test('UTF-16 and byte ranges cannot split an emoji or exceed a source', () {
    final source = SourceFile('lib/a.dart', '/a', utf8.encode('😀true'));
    expect(source.byteOffset(0), 0);
    expect(source.byteOffset(2), 4);
    expect(() => source.byteOffset(1), throwsA(failure('invalid_range')));
    expect(() => source.byteOffset(-1), throwsA(failure('invalid_range')));
    expect(
      () => source.editFromUtf16(2, 20, 'false'),
      throwsA(failure('invalid_range')),
    );
    expect(
      () => source.editFromUtf16(2, -1, 'false'),
      throwsA(failure('invalid_range')),
    );
    final insertion = ByteEdit(
      offsetBytes: 1,
      lengthBytes: 0,
      expected: '',
      replacement: 'x',
    );
    expect(() => insertion.apply(source), throwsA(failure('invalid_range')));
    final beyond = ByteEdit(
      offsetBytes: 99,
      lengthBytes: 0,
      expected: '',
      replacement: 'x',
    );
    expect(() => beyond.apply(source), throwsA(failure('invalid_range')));
  });

  test('expected mismatch, unchanged edits, invalid lengths and lone surrogates reject', () {
    final source = SourceFile('lib/a.dart', '/a', utf8.encode('true'));
    final wrong = ByteEdit(
      offsetBytes: 0,
      lengthBytes: 4,
      expected: 'TRUE',
      replacement: 'false',
    );
    expect(() => wrong.apply(source), throwsA(failure('expected_mismatch')));
    expect(
      () => ByteEdit(
        offsetBytes: 0,
        lengthBytes: 3,
        expected: 'true',
        replacement: 'false',
      ),
      throwsA(failure('invalid_patch')),
    );
    expect(
      () => ByteEdit(
        offsetBytes: 0,
        lengthBytes: 4,
        expected: 'true',
        replacement: 'true',
      ),
      throwsA(failure('invalid_patch')),
    );
    expect(
      () => ByteEdit(
        offsetBytes: 0,
        lengthBytes: 0,
        expected: '',
        replacement: String.fromCharCode(0xd800),
      ),
      throwsA(failure('invalid_unicode')),
    );
    expect(
      () => ByteEdit.fromJson({
        'offsetBytes': 0.5,
        'lengthBytes': 0,
        'expected': '',
        'replacement': 'x',
      }),
      throwsA(failure('invalid_payload')),
    );
  });

  test(
    'insertion and removal preserve untouched bytes without fuzzy matching',
    () {
      final source = SourceFile(
        'lib/a.dart',
        '/a',
        utf8.encode('\ufeff😀\r\ntrue; true;'),
      );
      final insertion = source.editFromUtf16(source.text.length, 0, '\r\n');
      expect(insertion.apply(source), [...source.rawBytes, 13, 10]);
      final removal = source.editFromUtf16(source.text.indexOf('true'), 4, '');
      expect(removal.apply(source), utf8.encode('\ufeff😀\r\n; true;'));
    },
  );

  test('canonical targets reject absolute, traversal, aliases and platform separators', () {
    for (final file in [
      '',
      '/lib/a.dart',
      '../a.dart',
      'lib/../a.dart',
      'lib/./a.dart',
      'lib//a.dart',
      'lib\\a.dart',
      'C:/a.dart',
      'lib/a\u0000.dart',
    ]) {
      expect(() => canonicalTarget(file), throwsA(failure('unsafe_path')));
    }
    expect(
      canonicalTarget('lib/path with spaces.dart'),
      'lib/path with spaces.dart',
    );
    expect(canonicalTarget('.', allowDot: true), '.');
  });

  test('mutant ID is canonical and independent of snapshot path, components and validation', () {
    final sources = ['/temporary/one/a.dart', '/another/root/a.dart']
        .map((path) => SourceFile('lib/a.dart', path, utf8.encode('true')))
        .toList();
    Mutant make(SourceFile source, List<String> names, String validation) =>
        Mutant.create(
          file: source.relativePath,
          sourceHash: source.sourceHash,
          operatorId: 'BOOLEAN_LITERAL_FLIP',
          family: MutationFamily.classic,
          edit: source.editFromUtf16(0, 4, 'false'),
          components: names,
          validation: validation,
        );
    final one = make(sources[0], ['z', 'a', 'a'], 'unvalidated');
    final two = make(sources[1], [], 'valid');
    expect(one.id, two.id);
    expect(one.components, ['a', 'z']);
    expect(
      one.id,
      sha256
          .convert(
            utf8.encode(
              jsonEncode([
                1,
                'lib/a.dart',
                sources[0].sourceHash,
                'BOOLEAN_LITERAL_FLIP',
                1,
                'classic',
                0,
                4,
                'true',
                'false',
              ]),
            ),
          )
          .toString(),
    );
    expect(Mutant.fromJson(one.toJson()).toJson(), one.toJson());
    final corrupt = one.toJson()..['id'] = List.filled(64, '0').join();
    expect(() => Mutant.fromJson(corrupt), throwsA(failure('invalid_id')));
    expect(
      () => one.validateAgainst(
        SourceFile('lib/a.dart', '/a', utf8.encode('TRUE')),
      ),
      throwsA(failure('source_hash_mismatch')),
    );
    expect(
      () => one.validateAgainst(
        SourceFile('lib/b.dart', '/b', utf8.encode('true')),
      ),
      throwsA(failure('source_hash_mismatch')),
    );
  });

  test('source byte limit is enforced before decoding and indexing', () {
    expect(
      () => SourceFile(
        'lib/a.dart',
        '/a',
        List.filled(SourceFile.maxSourceBytes + 1, 0x20),
      ),
      throwsA(failure('source_too_large')),
    );
  });
}

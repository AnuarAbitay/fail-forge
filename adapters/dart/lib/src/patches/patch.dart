import 'dart:convert';
import 'dart:typed_data';

import 'package:crypto/crypto.dart';

import '../model/mutation_family.dart';
import '../project/source_file.dart';
import '../protocol/adapter_error.dart';

String canonicalTarget(String value, {bool allowDot = false}) {
  checkUnicode(value);
  if (allowDot && value == '.') return value;
  final segments = value.split('/');
  if (value.isEmpty ||
      value.contains('\\') ||
      value.contains('\u0000') ||
      value.startsWith('/') ||
      RegExp(r'^[A-Za-z]:').hasMatch(value) ||
      segments.any(
        (segment) => segment.isEmpty || segment == '.' || segment == '..',
      )) {
    throw AdapterException(
      'unsafe_path',
      'target must be a canonical package-relative path',
    );
  }
  return value;
}

class ByteEdit {
  ByteEdit({
    required this.offsetBytes,
    required this.lengthBytes,
    required this.expected,
    required this.replacement,
  }) {
    jsonInteger(offsetBytes, 'offsetBytes');
    jsonInteger(lengthBytes, 'lengthBytes');
    checkUnicode(expected);
    checkUnicode(replacement);
    if (utf8.encode(expected).length != lengthBytes) {
      throw AdapterException(
        'invalid_patch',
        'expected byte length does not match lengthBytes',
      );
    }
    if (expected == replacement) {
      throw AdapterException(
        'invalid_patch',
        'a mutation must change the original range',
      );
    }
  }

  factory ByteEdit.fromJson(Map<String, Object?> value) {
    onlyKeys(value, {
      'offsetBytes',
      'lengthBytes',
      'expected',
      'replacement',
    }, 'edit');
    return ByteEdit(
      offsetBytes: jsonInteger(value['offsetBytes'], 'offsetBytes'),
      lengthBytes: jsonInteger(value['lengthBytes'], 'lengthBytes'),
      expected: jsonString(value['expected'], 'expected', allowEmpty: true),
      replacement: jsonString(
        value['replacement'],
        'replacement',
        allowEmpty: true,
      ),
    );
  }

  final int offsetBytes;
  final int lengthBytes;
  final String expected;
  final String replacement;

  Map<String, Object?> toJson() => {
    'offsetBytes': offsetBytes,
    'lengthBytes': lengthBytes,
    'expected': expected,
    'replacement': replacement,
  };

  void validateAgainst(SourceFile source) {
    final end = offsetBytes + lengthBytes;
    if (!source.isByteBoundary(offsetBytes) || !source.isByteBoundary(end)) {
      throw AdapterException(
        'invalid_range',
        'patch exceeds the source or splits a UTF-8 code point',
      );
    }
    final expectedBytes = utf8.encode(expected);
    for (var index = 0; index < expectedBytes.length; index++) {
      if (source.rawBytes[offsetBytes + index] != expectedBytes[index]) {
        throw AdapterException(
          'expected_mismatch',
          'patch expected bytes do not match the snapshot',
        );
      }
    }
  }

  Uint8List apply(SourceFile source) {
    validateAgainst(source);
    final replacementBytes = utf8.encode(replacement);
    final result = Uint8List(
      source.rawBytes.length - lengthBytes + replacementBytes.length,
    );
    result.setRange(0, offsetBytes, source.rawBytes);
    result.setRange(
      offsetBytes,
      offsetBytes + replacementBytes.length,
      replacementBytes,
    );
    result.setRange(
      offsetBytes + replacementBytes.length,
      result.length,
      source.rawBytes,
      offsetBytes + lengthBytes,
    );
    return result;
  }
}

class Mutant {
  Mutant._({
    required this.id,
    required this.file,
    required this.sourceHash,
    required this.operatorId,
    required this.operatorVersion,
    required this.family,
    required this.edit,
    required this.components,
    required this.validation,
    this.className,
    this.methodName,
  });

  factory Mutant.create({
    required String file,
    required String sourceHash,
    required String operatorId,
    required MutationFamily family,
    required ByteEdit edit,
    int operatorVersion = 1,
    List<String> components = const [],
    String? className,
    String? methodName,
    String validation = 'unvalidated',
  }) {
    canonicalTarget(file);
    if (!file.endsWith('.dart')) {
      throw AdapterException(
        'invalid_mutant',
        'mutation targets must be Dart files',
      );
    }
    if (!RegExp(r'^[0-9a-f]{64}$').hasMatch(sourceHash)) {
      throw AdapterException(
        'invalid_mutant',
        'sourceHash must be lowercase SHA-256',
      );
    }
    jsonString(operatorId, 'operatorId');
    if (utf8.encode(operatorId).length > 128 ||
        operatorVersion != 1 ||
        !{'unvalidated', 'valid', 'invalid'}.contains(validation)) {
      throw AdapterException('invalid_mutant', 'unsupported mutation metadata');
    }
    for (final name in [
      ...components,
      if (className != null) className,
      if (methodName != null) methodName,
    ]) {
      jsonString(name, 'symbol or component');
      if (utf8.encode(name).length > 512) {
        throw AdapterException(
          'invalid_mutant',
          'symbol or component exceeds its length limit',
        );
      }
    }
    final names = components.toSet().toList()..sort();
    final canonical = jsonEncode([
      1,
      file,
      sourceHash,
      operatorId,
      operatorVersion,
      family.wireName,
      edit.offsetBytes,
      edit.lengthBytes,
      edit.expected,
      edit.replacement,
    ]);
    return Mutant._(
      id: sha256.convert(utf8.encode(canonical)).toString(),
      file: file,
      sourceHash: sourceHash,
      operatorId: operatorId,
      operatorVersion: operatorVersion,
      family: family,
      edit: edit,
      components: List.unmodifiable(names),
      className: className,
      methodName: methodName,
      validation: validation,
    );
  }

  factory Mutant.fromJson(Map<String, Object?> value) {
    onlyKeys(value, {
      'id',
      'operatorId',
      'operatorVersion',
      'family',
      'file',
      'sourceHash',
      'components',
      'edit',
      'symbol',
      'validation',
    }, 'mutant');
    final symbol = value.containsKey('symbol')
        ? jsonObject(value['symbol'], 'symbol')
        : <String, Object?>{};
    onlyKeys(symbol, {'className', 'methodName'}, 'symbol');
    final result = Mutant.create(
      file: jsonString(value['file'], 'file'),
      sourceHash: jsonString(value['sourceHash'], 'sourceHash'),
      operatorId: jsonString(value['operatorId'], 'operatorId'),
      operatorVersion: jsonInteger(
        value['operatorVersion'],
        'operatorVersion',
        minimum: 1,
      ),
      family: MutationFamily.fromWire(value['family']),
      edit: ByteEdit.fromJson(jsonObject(value['edit'], 'edit')),
      components: jsonStrings(value['components'], 'components'),
      className: symbol.containsKey('className')
          ? jsonString(symbol['className'], 'className')
          : null,
      methodName: symbol.containsKey('methodName')
          ? jsonString(symbol['methodName'], 'methodName')
          : null,
      validation: jsonString(value['validation'], 'validation'),
    );
    if (jsonString(value['id'], 'id') != result.id) {
      throw AdapterException(
        'invalid_id',
        'mutant id does not match its canonical content',
      );
    }
    return result;
  }

  final String id;
  final String file;
  final String sourceHash;
  final String operatorId;
  final int operatorVersion;
  final MutationFamily family;
  final ByteEdit edit;
  final List<String> components;
  final String? className;
  final String? methodName;
  final String validation;

  void validateAgainst(SourceFile source) {
    if (file != source.relativePath || sourceHash != source.sourceHash) {
      throw AdapterException(
        'source_hash_mismatch',
        'mutant does not belong to this snapshot source',
      );
    }
    edit.validateAgainst(source);
  }

  Map<String, Object?> toJson() => {
    'id': id,
    'operatorId': operatorId,
    'operatorVersion': operatorVersion,
    'family': family.wireName,
    'file': file,
    'sourceHash': sourceHash,
    'components': components,
    'edit': edit.toJson(),
    'symbol': {
      if (className != null) 'className': className,
      if (methodName != null) 'methodName': methodName,
    },
    'validation': validation,
  };
}

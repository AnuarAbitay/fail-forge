/// A protocol-safe failure. Messages describe a category, never raw source.
class AdapterException implements Exception {
  AdapterException(this.code, this.message, {this.details});

  final String code;
  final String message;
  final Map<String, Object?>? details;

  Map<String, Object?> toJson() => {
    'code': code,
    'message': message,
    if (details != null) 'details': details,
  };

  @override
  String toString() => '$code: $message';
}

Map<String, Object?> jsonObject(Object? value, String name) {
  if (value is! Map<String, Object?>) {
    throw AdapterException('invalid_payload', '$name must be an object');
  }
  return value;
}

void onlyKeys(Map<String, Object?> value, Set<String> keys, String name) {
  if (value.keys.any((key) => !keys.contains(key))) {
    throw AdapterException('invalid_payload', '$name has an unknown field');
  }
}

String jsonString(Object? value, String name, {bool allowEmpty = false}) {
  if (value is! String || (!allowEmpty && value.isEmpty)) {
    throw AdapterException(
      'invalid_payload',
      '$name must be a nonempty string',
    );
  }
  checkUnicode(value);
  return value;
}

List<String> jsonStrings(Object? value, String name) {
  if (value is! List<Object?>) {
    throw AdapterException('invalid_payload', '$name must be a string array');
  }
  if (value.length > 10000) {
    throw AdapterException('invalid_payload', '$name exceeds its item limit');
  }
  return value.map((item) => jsonString(item, name)).toList(growable: false);
}

int jsonInteger(Object? value, String name, {int minimum = 0}) {
  if (value is! int || value < minimum || value > 0x1fffffffffffff) {
    throw AdapterException(
      'invalid_payload',
      '$name must be a bounded integer',
    );
  }
  return value;
}

void checkUnicode(String text) {
  for (var index = 0; index < text.length; index++) {
    final unit = text.codeUnitAt(index);
    if (unit >= 0xd800 && unit <= 0xdbff) {
      if (++index >= text.length) {
        throw AdapterException('invalid_unicode', 'unpaired UTF-16 surrogate');
      }
      final next = text.codeUnitAt(index);
      if (next < 0xdc00 || next > 0xdfff) {
        throw AdapterException('invalid_unicode', 'unpaired UTF-16 surrogate');
      }
    } else if (unit >= 0xdc00 && unit <= 0xdfff) {
      throw AdapterException('invalid_unicode', 'unpaired UTF-16 surrogate');
    }
  }
}

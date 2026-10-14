// Кириллица и emoji 😀 precede byte edits.
library;

part 'subject_part.dart';

class RangePolicy {
  bool accepts(int value) {
    return value >= 10;
  }

  bool enabled() => true;

  bool both(bool left, bool right) => left && right;
}

bool untestedFlag() => false;

Future<bool> asyncEnabled() async => true;

int nullableLength(String? value) {
  if (value != null) {
    return value.length;
  }
  return 0;
}

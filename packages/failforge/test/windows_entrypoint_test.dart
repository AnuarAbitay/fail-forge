import 'package:failforge/src/windows_console.dart';
import 'package:test/test.dart';

void main() {
  test('allows VM execution on Unix', () {
    ensureSupportedWindowsEntrypoint(
      windows: false,
      resolvedExecutable: '/sdk/bin/dart',
    );
  });

  for (final executable in [
    r'C:\sdk\bin\dart.exe',
    r'C:\sdk\bin\DART.EXE',
    'C:/sdk/bin/dartvm.exe',
  ]) {
    test('rejects the Windows VM broker before starting work: $executable', () {
      expect(
        () => ensureSupportedWindowsEntrypoint(
          windows: true,
          resolvedExecutable: executable,
        ),
        throwsA(
          isA<UnsupportedError>()
              .having(
                (error) => error.toString(),
                'native install',
                contains('dart install failforge'),
              )
              .having(
                (error) => error.toString(),
                'remove stale batch entry',
                contains('dart pub global deactivate failforge'),
              ),
        ),
      );
    });
  }

  for (final executable in [
    r'C:\tools\failforge.exe',
    'C:/sdk/bin/dartaotruntime.exe',
  ]) {
    test('allows a native executable without a VM broker: $executable', () {
      ensureSupportedWindowsEntrypoint(
        windows: true,
        resolvedExecutable: executable,
      );
    });
  }
}

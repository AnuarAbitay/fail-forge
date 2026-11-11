import 'dart:io';

import 'package:failforge/src/environment.dart';
import 'package:path/path.dart' as p;
import 'package:test/test.dart';

void main() {
  late Directory temporary;
  final dartName = Platform.isWindows ? 'dart.exe' : 'dart';
  final pathSeparator = Platform.isWindows ? ';' : ':';

  setUp(() {
    temporary = Directory.systemTemp.createTempSync(
      'failforge SDK with spaces ',
    );
  });
  tearDown(() {
    temporary.deleteSync(recursive: true);
  });

  File sdk(String relativeRoot) {
    final root = Directory(p.join(temporary.path, relativeRoot));
    final executable = File(p.join(root.path, 'bin', dartName));
    executable.parent.createSync(recursive: true);
    executable.writeAsStringSync('SDK Dart fixture');
    File(p.join(root.path, 'version')).writeAsStringSync('3.13.5\n');
    return executable;
  }

  group('Dart SDK discovery', () {
    test('uses the running VM SDK before environment fallbacks', () {
      final running = sdk('running SDK');
      final other = sdk('other SDK');
      expect(
        resolveDartExecutable(
          environment: {'DART_SDK': other.parent.parent.path},
          resolvedExecutable: running.path,
        ),
        running.resolveSymbolicLinksSync(),
      );
    });

    test('does not try to run an AOT launcher as Dart', () {
      final wrapper = File(p.join(temporary.path, 'failforge'))
        ..writeAsStringSync('AOT launcher fixture');
      final explicit = sdk('explicit SDK');
      expect(
        resolveDartExecutable(
          environment: {'DART_SDK': explicit.parent.parent.path},
          resolvedExecutable: wrapper.path,
        ),
        explicit.resolveSymbolicLinksSync(),
      );
    });

    test('ignores a dart-named launcher without an SDK marker', () {
      final wrapper = File(p.join(temporary.path, 'wrapper', 'bin', dartName));
      wrapper.parent.createSync(recursive: true);
      wrapper.writeAsStringSync('Dart shim fixture');
      final fallback = sdk('fallback SDK');
      expect(
        resolveDartExecutable(
          environment: {'PATH': fallback.parent.path},
          resolvedExecutable: wrapper.path,
        ),
        fallback.resolveSymbolicLinksSync(),
      );
    });

    test('continues past broken SDK and PATH entries', () {
      final fallback = sdk('PATH SDK');
      expect(
        resolveDartExecutable(
          environment: {
            'DART_SDK': p.join(temporary.path, 'missing SDK'),
            'PATH': [
              p.join(temporary.path, 'missing bin'),
              fallback.parent.path,
            ].join(pathSeparator),
          },
          resolvedExecutable: p.join(temporary.path, 'missing wrapper'),
        ),
        fallback.resolveSymbolicLinksSync(),
      );
    });

    test('does not accept a directory in place of the Dart executable', () {
      final invalidRoot = Directory(p.join(temporary.path, 'invalid SDK'));
      Directory(p.join(invalidRoot.path, 'bin', dartName))
          .createSync(recursive: true);
      File(p.join(invalidRoot.path, 'version')).writeAsStringSync('3.13.5');
      final fallback = sdk('valid SDK');
      expect(
        resolveDartExecutable(
          environment: {'PATH': fallback.parent.path},
          resolvedExecutable: p.join(invalidRoot.path, 'bin', dartName),
        ),
        fallback.resolveSymbolicLinksSync(),
      );
    });

    test('finds Flutter bundled Dart from a Flutter-only PATH', () {
      final bundled = sdk(p.join('flutter', 'bin', 'cache', 'dart-sdk'));
      final flutterBin = bundled.parent.parent.parent.parent;
      File(
        p.join(flutterBin.path, Platform.isWindows ? 'flutter.bat' : 'flutter'),
      ).writeAsStringSync('Flutter launcher fixture');
      expect(
        resolveDartExecutable(
          environment: {'PATH': flutterBin.path},
          resolvedExecutable: p.join(temporary.path, 'AOT launcher'),
        ),
        bundled.resolveSymbolicLinksSync(),
      );
    });

    test('supports FLUTTER_ROOT for a compiled launcher', () {
      final bundled = sdk(p.join('flutter', 'bin', 'cache', 'dart-sdk'));
      expect(
        resolveDartExecutable(
          environment: {'FLUTTER_ROOT': p.join(temporary.path, 'flutter')},
          resolvedExecutable: p.join(temporary.path, 'AOT launcher'),
        ),
        bundled.resolveSymbolicLinksSync(),
      );
    });

    test(
      'resolves Dart and Flutter symlinks to their SDK layouts',
      () {
        final actual = sdk('real SDK');
        final linkedDart = Link(p.join(temporary.path, 'dart shim'))
          ..createSync(actual.path);
        expect(
          resolveDartExecutable(
            environment: {},
            resolvedExecutable: linkedDart.path,
          ),
          actual.resolveSymbolicLinksSync(),
        );

        final bundled = sdk(p.join('flutter', 'bin', 'cache', 'dart-sdk'));
        final flutterBin = bundled.parent.parent.parent.parent;
        final flutter = File(p.join(flutterBin.path, 'flutter'))
          ..writeAsStringSync('Flutter launcher fixture');
        final shimBin = Directory(p.join(temporary.path, 'shims'))
          ..createSync();
        Link(p.join(shimBin.path, 'flutter')).createSync(flutter.path);
        expect(
          resolveDartExecutable(
            environment: {'PATH': shimBin.path},
            resolvedExecutable: p.join(temporary.path, 'AOT launcher'),
          ),
          bundled.resolveSymbolicLinksSync(),
        );
      },
      skip: Platform.isWindows ? 'Symlinks require Windows privileges.' : false,
    );

    test('reports how to supply a missing SDK', () {
      expect(
        () => resolveDartExecutable(
          environment: {},
          resolvedExecutable: p.join(temporary.path, 'AOT launcher'),
        ),
        throwsA(
          isA<StateError>().having(
            (error) => error.message,
            'message',
            contains('DART_SDK'),
          ),
        ),
      );
    });
  });

  group('cache selection', () {
    test('custom cache overrides missing user environment', () {
      final root = p.join(temporary.path, 'cache with spaces');
      expect(
        resolveCacheDirectory(environment: {'FAILFORGE_CACHE': root}).path,
        root,
      );
      expect(Directory(root).existsSync(), isFalse);
    });

    test('relative custom cache is made absolute', () {
      expect(
        resolveCacheDirectory(
          environment: {'FAILFORGE_CACHE': 'relative cache'},
        ).path,
        p.absolute('relative cache'),
      );
    });

    test('uses macOS and Linux user cache conventions', () {
      expect(
        resolveCacheDirectory(
          environment: {'HOME': '/users/example'},
          operatingSystem: 'macos',
        ).path,
        '/users/example/Library/Caches/failforge',
      );
      expect(
        resolveCacheDirectory(
          environment: {'HOME': '/users/example'},
          operatingSystem: 'linux',
        ).path,
        '/users/example/.cache/failforge',
      );
      expect(
        resolveCacheDirectory(
          environment: {'XDG_CACHE_HOME': '/custom/cache'},
          operatingSystem: 'linux',
        ).path,
        '/custom/cache/failforge',
      );
    });

    test('uses Windows local app data with case-insensitive env keys', () {
      expect(
        resolveCacheDirectory(
          environment: {'LocalAppData': r'C:\Users\Example\AppData\Local'},
          operatingSystem: 'windows',
        ).path,
        r'C:\Users\Example\AppData\Local\failforge\cache',
      );
    });

    test('reports a missing home value and unsupported OS', () {
      for (final os in ['macos', 'linux', 'windows']) {
        expect(
          () => resolveCacheDirectory(environment: {}, operatingSystem: os),
          throwsA(isA<StateError>()),
        );
      }
      expect(
        () =>
            resolveCacheDirectory(environment: {}, operatingSystem: 'android'),
        throwsUnsupportedError,
      );
    });
  });

  group('native process environment', () {
    test('preserves Flutter search priority and unrelated variables', () {
      final executable = sdk('SDK');
      final original = {'PATH': '/flutter/bin', 'CUSTOM': 'value'};
      final result = nativeEnvironment(executable.path, environment: original);
      expect(
        result['PATH'],
        '/flutter/bin$pathSeparator${executable.parent.path}',
      );
      expect(result['CUSTOM'], 'value');
      expect(original, {'PATH': '/flutter/bin', 'CUSTOM': 'value'});
    });

    test('keeps an existing Dart bin entry in place', () {
      final executable = sdk('SDK');
      final existing = [
        '/before',
        executable.parent.path,
        '/after',
      ].join(pathSeparator);
      expect(
        nativeEnvironment(
          executable.path,
          environment: {'PATH': existing},
        )['PATH'],
        existing,
      );
    });

    test('creates PATH when none is supplied', () {
      final executable = sdk('SDK');
      expect(
        nativeEnvironment(executable.path, environment: {})['PATH'],
        executable.parent.path,
      );
    });
  });
}

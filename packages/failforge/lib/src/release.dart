import 'dart:ffi';

/// One immutable release asset. Checksums are part of the launcher release,
/// rather than downloaded alongside the executable they are meant to verify.
class ReleaseArtifact {
  const ReleaseArtifact({
    required this.version,
    required this.target,
    required this.sha256,
    required this.manifestSha256,
  });

  final String version;
  final String target;
  final String sha256;
  final String manifestSha256;

  String get archiveName => 'failforge-$version-$target.tar.gz';
  String get cacheKey => '$version-$target';
  Uri get uri => Uri.https(
    'github.com',
    '/AnuarAbitay/fail-forge/releases/download/v$version/$archiveName',
  );
}

// Verified against GitHub's public v0.1.0 release asset digests and retained
// SHA256SUMS on 2026-11-10. Updating these also requires installation smoke tests.
const releaseArtifacts = {
  'aarch64-apple-darwin': ReleaseArtifact(
    version: '0.1.0',
    target: 'aarch64-apple-darwin',
    sha256: 'd6dad09004b4bf46082c39194d8002ff12a849c7398eacad59c2f5acf8a8b515',
    manifestSha256:
        '34680c7352a84015d7889f31a390092e61db2f7bf1dd7ffe8178b998c2117c8f',
  ),
  'x86_64-unknown-linux-gnu': ReleaseArtifact(
    version: '0.1.0',
    target: 'x86_64-unknown-linux-gnu',
    sha256: 'b75136cf25d31167fe59900c19ae66d1d91f8be8b42d67f8818584d2fde1f310',
    manifestSha256:
        '649e30c2dbd490807938281925d35a3ef200b7780f0c8d73d6b42df1c10f91cd',
  ),
  'x86_64-pc-windows-msvc': ReleaseArtifact(
    version: '0.1.0',
    target: 'x86_64-pc-windows-msvc',
    sha256: '62d96bee3a3cc3dd87a76279e8028a66e6c52b0ba4e402c91b57658c8d86cfc1',
    manifestSha256:
        'df21e51fa06da2f455e1fe6304bdf8cb7002c35ec68f0fbc581dd046172ba7c1',
  ),
};

ReleaseArtifact currentRelease({Abi? abi}) {
  final current = abi ?? Abi.current();
  final target = {
    Abi.macosArm64: 'aarch64-apple-darwin',
    Abi.linuxX64: 'x86_64-unknown-linux-gnu',
    Abi.windowsX64: 'x86_64-pc-windows-msvc',
  }[current];
  if (target == null) {
    throw UnsupportedError(
      'FailForge has no verified native release for $current. '
      'Supported: macOS arm64, Linux x64 (glibc), Windows x64. '
      'Build the native CLI from source for other platforms.',
    );
  }
  return releaseArtifacts[target]!;
}

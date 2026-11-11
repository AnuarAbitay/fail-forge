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

// Verified against the native v0.2.0 release assets on 2026-11-11.
// Source: ea503af463cf861459d719f5ffbe6182421e24f7. Updating pins requires installation smoke tests.
const releaseArtifacts = {
  'aarch64-apple-darwin': ReleaseArtifact(
    version: '0.2.0',
    target: 'aarch64-apple-darwin',
    sha256: '2b831e7341c3d55cf2719672bef2e9a77033f56e205a511dac18f247d862e443',
    manifestSha256:
        '08341640a4894df1be97773ddef7ec8c3f7bbe8b78a0af37e9098dc016969d25',
  ),
  'x86_64-unknown-linux-gnu': ReleaseArtifact(
    version: '0.2.0',
    target: 'x86_64-unknown-linux-gnu',
    sha256: '24d176d1059b4e3fc6881be33f134d521c673ff16cde98b10e244b05f162e4bc',
    manifestSha256:
        '90aa3ca50973730ad34ba31babe9d82b1eeef5af7afa272d324d44a67bcc306b',
  ),
  'x86_64-pc-windows-msvc': ReleaseArtifact(
    version: '0.2.0',
    target: 'x86_64-pc-windows-msvc',
    sha256: '6de7b7be8e5744f82ddfb2942b91369b0ef188a7de2eb0a83648f8bca3e4f4fe',
    manifestSha256:
        'b48f7c9464a94c375bb0cebd1f106aec814293db3677e4bf91a32adbb9f1a103',
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

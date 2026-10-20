# Build a local beta archive

`failforge-tools package` builds a native FailForge archive for the machine on
which it runs. The first beta version is `0.1.0-beta.1`. Creating an archive does
not publish a GitHub release or establish that other platforms have passed.

To use an archive, follow the [installation guide](installation.md#install-from-an-archive).
This page is for preparing and checking the archive.

## Requirements and scope

- Build on Linux or macOS on an `aarch64` or `x86_64` host with Rust/Cargo, a C
  linker, Git and `tar`. The package command targets the native Rust host; it is not a
  cross-compilation interface.
- Install the matching `rust-src` and `rust-docs` components for that toolchain.
  Packaging retains their Rust runtime license notices and fails if the required
  notice files are missing; it does not download these components itself.
- Start from a complete FailForge source checkout with its Cargo lockfile,
  adapter source, documentation and retained third-party license material.
  Adapter runtime sources are selected from tracked `.dart` files under its
  `bin/` and `lib/` directories; untracked runtime additions are not shipped.
- Use a new output directory. Existing destinations are rejected rather than
  overwritten.
- Cargo may download missing build dependencies. Packaging does not install
  Dart/Flutter SDKs or resolve Pub dependencies.

The resulting archive contains executables for one operating system and
architecture. It is not a universal binary. A successful local macOS arm64 build
does not verify Linux, macOS Intel or another system-library baseline.

The current local packaging setup uses Rust **1.99.0**. The project's source
checks on Rust 1.85.1 do not establish packaging compatibility with that older
toolchain. Runtime notice paths can differ between Rust versions; if a required
notice is absent, packaging fails even when the toolchain can compile the code.

## Build

From the source checkout:

```sh
rustup component add rust-src rust-docs
cargo run -p failforge-tools --locked -- package \
  --output /absolute/new-beta-output \
  --expected-version 0.1.0-beta.1
```

If the tool is already built, select the source checkout explicitly:

```sh
target/debug/failforge-tools package \
  --installation /absolute/path/to/fail-forge-source \
  --output /absolute/another-new-beta-output \
  --expected-version 0.1.0-beta.1
```

For this developer command, `--installation` identifies the source checkout to
package. It is not the destination or an already installed runtime directory.
`--output` identifies a new directory that the command creates.

The command builds only the two production executables, `failforge` and
`failforge-adapter-campaign`, using locked Rust dependencies and the project's
optimized default build. No `--release` argument is needed. Developer tools,
test fixtures and examples are not runtime executables in the archive.

The version comes from the newly built `failforge --version` output.
`--expected-version` checks that value; it cannot rename a binary as a different
version. The target triple comes from the build toolchain's native host.
The command checks the retained dependency notice checksums and their lockfile
input bindings. A stale inventory must be refreshed before packaging; that
consistency check does not replace a dependency license review.

## Output and contents

A successful macOS arm64 package of this version produces:

```text
new-beta-output/
  failforge-0.1.0-beta.1-aarch64-apple-darwin.tar.gz
  SHA256SUMS
  package-manifest.json
```

The target component changes for other native hosts. Keep the checksum file and
manifest with the corresponding archive.

The archive has one top-level directory:

```text
failforge-0.1.0-beta.1-aarch64-apple-darwin/
  bin/
    failforge
    failforge-adapter-campaign
  adapters/dart/
    bin/
    lib/
    pubspec.yaml
    pubspec.lock
    LICENSE
    NOTICE
  docs/
  README.md
  CHANGELOG.md
  SECURITY.md
  LICENSE
  NOTICE
  THIRD_PARTY_NOTICES.md
  third-party/
```

`package-manifest.json` records the version, target, source revision and dirty
state, toolchain/build information, and the paths, sizes and hashes of packaged
files. It also identifies runtime notices copied from the selected Rust sysroot,
retained under `third-party/rust-runtime/`. Inspect the manifest when identifying
the exact artifact under test. A package
from uncommitted sources is a local test artifact; build a publication candidate
from the intended committed revision.

The archive excludes the source checkout's `target/`, `.git/`, `.dart_tool/`,
SDKs, package caches, temporary mutation copies, local reports, `.env` files and
test/development executables. It does not include the tested user's project or
preconfigured credentials. Files deliberately placed in distributable source or
documentation still require review; packaging is not a secret-scanning service.

## Verify and exercise the archive

Run the generated checksum check from its output directory:

```sh
cd /absolute/new-beta-output
shasum -a 256 -c SHA256SUMS
tar -tzf failforge-0.1.0-beta.1-aarch64-apple-darwin.tar.gz
```

On systems providing `sha256sum`, `sha256sum -c SHA256SUMS` is an alternative.
The hashes detect changed files when compared with a trusted checksum file;
they are not a digital signature or a claim of authenticity.

### Installed-archive smoke check

After extracting and moving a disposable copy of the archive, run this command
with the source checkout's developer tool:

```sh
target/debug/failforge-tools package-smoke \
  --installation "/absolute/path/to/extracted beta installation" \
  --dart-sdk /absolute/path/to/dart-sdk \
  --output /absolute/new-package-smoke-results
```

`--installation`, `--dart-sdk` and `--output` are required. The output must be a
new directory outside the installation and SDK. The command prepares
the adapter and its own small Dart test fixture with `pub get --enforce-lockfile`,
using a fresh Pub cache in its output directory. It invokes the installed CLI
through a symlink from another working directory, checks help/version, generates
a profile and runs one classical boolean mutant through both direct and YAML
commands. It checks matching mutant identities, the selected installation,
source preservation, reports and working-copy cleanup. It also checks the CLI
version against the package manifest and verifies the listed payload files'
hashes, sizes and executable permissions before and after the run.

Keep the output for command receipts, reports and the dependency cache. This is
a disposable installation check: prepare the adapter again at its final user
location if you later move it or remove that cache. The developer smoke tool is
not part of the runtime archive.

For a prepared offline environment, supply `--offline` and an existing populated
`PUB_CACHE`. This avoids dependency downloads and does not test a fresh-cache
installation. The smoke check is Dart-only; it does not replace the Flutter,
cancellation, recovery or full mutation regression suites.

### Publication checks

Before publication:

1. Extract the archive into a new directory outside the checkout. Follow the
   [archive installation steps](installation.md#install-from-an-archive), prepare
   the adapter at its final path and supply a compatible SDK separately.
2. Check `failforge --version` and help from the extracted `bin/` directory.
   Keep both executables together.
3. On a prepared test package, exercise profile generation and mutation runs
   through both YAML and direct CLI options. Use new report directories. Check
   verdicts, reports, source preservation and cleanup.
4. Exercise the relocated installation from another working directory, including
   paths with spaces. Verify operation without relying on the source checkout's
   adapter or default SDK path.
5. Record the source revision, archive checksum, host/architecture, SDK versions,
   commands and results. Repeat on every platform advertised for that release.
6. Review the actual binary/runtime and archive contents against the
   [dependency license boundaries](dependency-licenses.md#distribution-boundaries).
   Rust runtime notice files are retained, but copying them does not establish
   the complete runtime/library contents of a binary or audit its system-library
   dependencies.
7. Check the source checkout step in the actual CI run. The current workflow
   downloads source anonymously and therefore requires a public repository.
   A private repository needs a separately approved authenticated checkout
   arrangement consistent with the repository's account policy. A queued run or
   the presence of the YAML file is not evidence of successful checkout or tests.

This procedure produces local evidence. A successful packaging command alone is
not a successful regression run or a completed license review of the binary.

## Current boundaries

- The command creates `.tar.gz` files only. It does not create a ZIP, installer,
  package-manager entry or system-wide configuration.
- Selected payload inputs must be regular files; symbolic links and special
  files are rejected rather than followed into the archive.
- It does not publish, upload, create a tag, sign or notarize an artifact.
- An SDK and adapter dependencies are not bundled. Installation may need network
  access for `pub get --enforce-lockfile`; the archive is not an offline SDK bundle.
- Rust/Cargo are build-time requirements for the packager. Users running an
  archive built for their platform do not need Rust.
- The CI workflow is configured to package, verify, extract, relocate and
  smoke-test archives in its Linux/macOS Dart jobs. Those settings do not establish
  that either platform has passed; inspect actual runs, including source download.
  CI does not publish release assets.

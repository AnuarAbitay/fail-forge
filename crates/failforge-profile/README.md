# Native profile generation

`failforge-profile` prepares explicit file-scoped mutation profiles, determines
operator applicability through the Dart adapter, and publishes complete YAML.
The implementation uses Rust for configuration, orchestration and filesystem
operations. Static language analysis remains in the Dart adapter.

`preparation::prepare_profile` validates the source, destination and selections,
renders every selected catalogue switch, and round-trips the document through
`failforge-config`. `--template` uses this path without SDK discovery.

`analysis::analyze_profile` requires a prepared standalone package and adapter.
It analyzes an isolated snapshot through the native campaign helper, validates
the transcript and patch evidence, and requires complete cleanup before returning.
An incomplete, cancelled or truncated analysis never becomes an all-false profile.
Author tests are never executed. Call the returned evidence's `verify_unchanged`
immediately before publishing.

`preparation::apply_applicability` intersects requested operators with the complete
analysis results. A file with no applicable operators is reported as an error and
does not create output directories.

`output::write_profile` checks the directory identities captured during preparation
and publishes verified complete bytes. Existing regular files require explicit
force. Symlinks, special files, changed directories and nested packages are rejected.
Force preserves file permissions and keeps a recoverable backup if rollback fails.
These checks detect observed concurrent changes; they do not lock out other programs
after successful publication.

Run the native regression tests with:

```sh
cargo test -p failforge-profile --all-targets --locked
cargo test -p failforge-cli --test profile_generate --locked
```

The `profile_bench` example accepts JSON Lines requests for preparation and
returns semantic profile data plus elapsed nanoseconds. Build it in release mode
for measurements. Its timing includes path checks, output binding, framework
detection, rendering and configuration round-trip. It excludes process startup,
static Dart analysis and publication. Measure those separately.

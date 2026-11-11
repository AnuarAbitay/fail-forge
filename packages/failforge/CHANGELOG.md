# Changelog

## 0.1.0-beta.1

- Add a Dart launcher for native FailForge `0.1.0`.
- Prepare a verified runtime through `failforge setup` or the first ordinary
  command, then reuse the cached installation.
- Preserve the native command's arguments, working directory, standard streams
  and exit code.
- Add local Pub activation and publication preparation instructions.
- Use the standard `dart install` native entry point on Windows so Ctrl+C and
  Ctrl+Break wait for native cleanup and preserve exit codes 130 and 143.
  Reject legacy Windows VM/batch activation before starting work and explain
  how to migrate.
- Propagate asynchronous archive file errors through normal error handling,
  close network/file resources, and permit retry after a partial download.
- Reject special files in cached manifests, preparation markers and install
  locks before opening a potentially blocking stream.

This is a local prerelease prototype; no pub.dev publication is recorded.

# Changelog

## 0.2.1

- Add a runnable boundary-mutation example with tests and an offline HTML report
  walkthrough, available in the package's Example tab.
- Apply Dart core style rules and enforce them in package CI.
- Use the verified native FailForge `0.2.0` runtime.

## 0.2.0

- Provide the `failforge` command for Dart and Flutter with native FailForge
  `0.2.0`, including offline HTML reports for saved mutation runs.
- Install a platform-specific runtime with pinned archive and package manifest
  SHA-256 hashes; prepare its Dart adapter and reuse the verified runtime cache.
- Preserve native arguments, working directory, standard streams and exit code.
- Use the standard `dart install` native entry point on Windows so Ctrl+C and
  Ctrl+Break wait for native cleanup and preserve exit codes 130 and 143.
  Explain migration from legacy Windows VM entry points before starting work.
- Handle asynchronous archive file errors, close network/file resources and
  support retry after a partial download.
- Reject special files in cached manifests, preparation markers and install
  locks before opening potentially blocking streams.

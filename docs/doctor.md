# Check an installation with `doctor`

Run this command from the target package root before generating a profile or
starting a campaign:

```sh
failforge doctor
```

`doctor` was added in `0.1.0-beta.2` and remains available in `0.1.0-beta.4`.
The published `0.1.0-beta.1` archive predates this command.

It reads local files and prepared dependency metadata. It does not run SDK
programs, the adapter or your tests, download dependencies, create working
copies or change project files. A successful result means the static checks
passed; it does not establish a passing baseline or mutation score.

## Select the environment

The defaults match mutation commands: the invocation directory is the project,
the executable location determines the installation, the adapter is
`<installation>/adapters/dart`, and SDK discovery searches `PATH` before the
installation's `.toolchain` fallback.

Use explicit paths to check another environment:

```sh
failforge doctor \
  --project /absolute/path/to/package \
  --dart-sdk /absolute/path/to/dart-sdk \
  --installation /absolute/path/to/fail-forge
```

Use `--flutter-sdk /absolute/path/to/flutter` instead of `--dart-sdk` for an
explicit Flutter installation. These SDK options are mutually exclusive.
`--adapter-root PATH` selects a separately prepared adapter. Relative command
paths are resolved from the invocation directory.

To check the environment and source selection of an existing YAML profile:

```sh
failforge doctor --config profiles/lib/src/calculator.yml
```

As with `mutate`, `--config` cannot be mixed with `--project`, SDK options or
`--adapter-root`. Put these settings in the profile; `--installation` remains
available on the command line. Relative YAML paths are resolved from the
profile's directory, and its nearest parent `pubspec.yaml` identifies the target
package. Unknown settings, profiles without enabled mutation operators and
invalid literal source paths are diagnosed using the campaign's configuration
rules.

## Read the checks

The report uses `[OK]`, `[WARN]`, `[FAIL]` and `[SKIP]`. Independent checks
continue after a failure; checks that cannot be evaluated explain why they were
skipped.
Failures include a suggested next step, such as preparing adapter dependencies
or running the target package's `pub get`.

The checks cover:

- Installation directory and the native analysis helper beside the executable.
- Effective configuration and literal source selections in a profile.
- Dart executable, SDK layout and the bounded local SDK `version` file. The
  adapter accepts a stable Dart version `>=3.13.0 <4.0.0`. Dart minor versions
  above 3.13 produce `[WARN]`: the SDK is in range, but compatibility with the
  pinned analyzer `14.5.0` and newer language features needs separate validation.
- Adapter entry point and prepared dependency roots, including its declared
  dependencies and its own package identity.
- Target package metadata, dependency graph, supported test framework and source
  files using the same inspection as mutation campaigns.
- Test runner files and conventional test-file presence. Custom test discovery
  through `dart_test.yaml` may need a separate test run.
- For Flutter projects, the matching Flutter installation and prepared local
  test/compiler artifacts. Dart-only projects do not require Flutter artifacts.
- Existing report destinations and relevant filesystem access permissions,
  without creating output directories.

`doctor` prints commands for manual remediation. Run them separately, then repeat
the check. A read-only dependency check cannot prove that prepared package
metadata is current with every constraint in `pubspec.yaml`; if dependencies
were changed, prepare them again.

Local verification used Flutter 3.47.7 with Dart 3.13.5. A version in the
accepted range, including one without a warning, does not establish that your
SDK, package and language features have been exercised successfully.

## Exit codes

| Code | Meaning |
| --- | --- |
| `0` | No static check failed. Review warnings and skipped checks. Warnings alone do not change the exit code. |
| `1` | A prerequisite is missing, inaccessible or unsupported. |
| `2` | Invalid command arguments, configuration or profile source selection. |
| `125` | The command could not complete or write its report. |

The [installation guide](installation.md) explains manual SDK and dependency
preparation. The [mutation command reference](mutation-cli.md) covers profile
and campaign settings.

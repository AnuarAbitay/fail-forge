# FailForge

FailForge's first stage is a Rust library and CLI for running external commands
on Linux and macOS with bounded output capture, timeout handling, cancellation,
and explicit Unix process cleanup. It runs tools such as `dart test`, `flutter
test`, Cargo, or a project-specific executable. The Dart/Flutter development adapter adds AST mutation discovery, in-memory
static validation and a Rust patch/protocol contract. `failforge mutate` connects
these components to the Python campaign controller, with explicit YAML or CLI
configuration, fresh test copies and family-specific reports. See the
[mutation CLI guide](docs/mutation-cli.md) for installation, commands and current
standalone-package boundaries, and the [Dart adapter guide](docs/dart-adapter.md)
for the protocol and operators.
The [Dart semantic mutation guide](docs/dart-semantic-mutations.md) describes
the 19 operators under `language_specific -> dart`, their limits and configuration.
One YAML file selects a profile with explicit family/operator switches. Omitted
operators are off; `language_specific` requires an explicit language. Profiles
have no `version` or `project` keys: the nearest parent `pubspec.yaml` identifies
the target package. See the [single-file profile](examples/flutter_fixture/profiles/subject.yml).

Generate a profile for one existing Dart file under its package's `lib/`:

```sh
failforge profile generate --file lib/src/calculator.dart --family classic \
  --output profiles/calculator.yml
failforge mutate --config profiles/calculator.yml
```

Generation uses the prepared project's SDK and Dart adapter to enable only
operators with a statically valid mutation in the selected file. Generation runs
in Rust and needs prepared dependencies; author tests are never run. Use `--template`
for a catalogue template without an SDK, dependencies or source analysis.
Repeat `--family` to combine families, add `--language dart` when selecting
`language_specific`, and use `--dry-run` to preview the YAML. The
required `--output` names a YAML file inside the same package; existing files
require `--force`. See [profile generation](docs/mutation-cli.md#generate-a-profile)
for selection and overwrite rules.

The backend supports Linux and macOS. Windows is intentionally unsupported.
FailForge is **not a sandbox**: the child retains the filesystem, network, and
other access granted by its operating-system credentials. Stdin defaults to `null`. Library callers can transfer an input descriptor with
`Runner::start_with_stdin` for protocols; the CLI has no interactive input or PTY.

## Build and verify

Install Rust with Cargo and the platform's normal C linker. The MSRV verification
toolchain is **Rust 1.85.1**, separate from CI's current stable toolchain. The
workspace uses edition 2024 and a committed `Cargo.lock`.

```sh
cargo build --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked -- --test-threads=1
cargo test --workspace --doc --locked
cargo doc --workspace --no-deps --locked
```

Run the same build checks with the minimum verification toolchain:

```sh
cargo +1.85.1 check --workspace --all-targets --locked
cargo +1.85.1 test --workspace --all-targets --locked -- --test-threads=1
```

The process tests use a Rust fixture and do not require Dart or Flutter. Their
control channels are independent of captured output, and waits have external
deadlines. The serial test-harness setting prevents unrelated tests from competing
for deliberately bounded global capture-worker resources; tests still exercise
concurrent stdout/stderr and supervised processes internally.

CI defines separate Linux/macOS jobs for stable and MSRV checks. It downloads
public repository sources anonymously by commit SHA, without a Git checkout,
saved Git credentials, or a GitHub token. A private repository requires a separate,
policy-compliant source acquisition arrangement; there is no credential fallback.
Configured CI is not evidence that a particular platform has already passed.
Record the executed commands, outcomes, toolchain versions, and platform
limitations when verifying a build.

## Run a command

```sh
./target/debug/failforge run --timeout 30s -- /usr/bin/printf '%s\n' 'hello world'
./target/debug/failforge run --timeout 2m --cwd ./my-project -- dart test
./target/debug/failforge run --stream --capture-dir ./artifacts/run-001 -- cargo test
```

The required `--` ends FailForge's options. Everything after it belongs to the
child command. Arguments remain separate native OS strings; FailForge does not
concatenate them into shell text. A shell runs only when explicitly requested,
or when the OS executes an executable script's valid shebang:

```sh
./target/debug/failforge run -- /bin/sh -c 'printf "%s\n" "$HOME"'
```

Bare program names use the effective **child** `PATH`, after environment
inheritance, removals, and assignments. Relative and empty `PATH` entries refer
to the child working directory. Prepared candidates are attempted with `execve`;
permission-denied, missing-file, and non-directory errors permit trying the next
candidate. Other execution errors stop the search. There is no implicit OS search
path or `ENOEXEC` shell fallback.

With an empty child environment, provide a `PATH` or a program containing `/`:

```sh
./target/debug/failforge run --clear-env --env PATH=/usr/bin:/bin -- tool
./target/debug/failforge run --clear-env -- ./tool
```

An explicitly empty `PATH` searches the child directory. Relative `--capture-dir`
paths refer to the directory where FailForge was invoked, independently of
`--cwd`. Program names, arguments, paths, and environment entries preserve
non-UTF-8 Unix bytes. NUL is rejected. See the [CLI contract](docs/cli.md) for all
options, environment rules, display behavior, and duration syntax.

## Library

`failforge-runner` uses an existing Tokio runtime with I/O and time enabled; it
does not create another runtime. A basic application can construct a command,
start it, wait for its retained outcome, and explicitly shut down the runner:

```rust
use failforge_runner::{CommandSpec, Runner, TimeoutPolicy};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runner = Runner::new();
    let spec = CommandSpec {
        program: "/usr/bin/printf".into(),
        args: vec!["%s\n".into(), "hello".into()],
        cwd: std::env::current_dir()?,
        env: Default::default(),
        timeout_policy: TimeoutPolicy {
            timeout: Some(Duration::from_secs(5)),
            ..Default::default()
        },
        capture_policy: Default::default(),
    };

    let mut handle = runner.start(spec)?;
    let outcome = handle.wait().await;
    runner.shutdown().await?;
    let result = outcome?;
    println!("cause={:?}, status={:?}", result.cause, result.status);
    Ok(())
}
```

The stop cause and actual OS status are separate. A process handling a timeout
by exiting zero still has `cause=TimedOut` and `status=exit:0`. Post-spawn failures
provide a partial result with available status, output, timing, and cleanup
evidence. Cancelling a `wait()` future leaves the supervisor running, and later
waits return the same shared outcome. Dropping the handle requests cancellation;
it does not abandon wait ownership. See [the architecture](docs/architecture.md)
for ownership, result snapshots, and shutdown guarantees.

## Output and exit codes

Default capture keeps a 1 MiB tail per stream, reads 32 KiB chunks, and permits a
1 MiB combined display queue when streaming. Optional raw capture files have a
256 MiB quota per stream. Capture is byte-oriented: output need not contain
newlines or valid UTF-8. A slow display consumer may lose display chunks while
raw capture continues. Capture errors and quota exhaustion produce partial
results rather than successful truncated files.

Normal diagnostics omit raw child arguments, environment values, and output
tails. **Streamed child output, raw capture files, and library result tails can
contain secrets.** FailForge does not redact child bytes. Capture files are
created exclusively with mode `0600`; protect the destination directory and
handle artifacts according to their contents.

For library display errors, call `chunk.report_error(error)` on the failed chunk.
Errors without a chunk use `receiver.report_error(OutputStream::Stderr, error)`
with an explicit source. See [the architecture](docs/architecture.md) for capture
ownership and failure handling.

| Outcome | CLI exit code |
| --- | --- |
| Ordinary child completion | Actual child exit code |
| Independent child signal `N` | `128 + N` |
| Timeout with complete cleanup | `124` |
| Runner failure or incomplete cleanup | `125` |
| Ctrl+C cancellation with complete cleanup | `130` |
| SIGTERM cancellation with complete cleanup | `143` |
| Invalid CLI arguments or configuration | `2` |
| Help or version | `0` |

Cleanup failure has priority for the CLI exit code; the original stop cause and
available real OS status remain in the result. A child's own exit code may
coincide with one of these numbers, so inspect the cause/status summary when
the distinction matters.

## Optional Flutter smoke test

On a machine with Flutter already installed, choose an existing Flutter project
whose dependencies and baseline tests are prepared. From this repository:

```sh
./target/debug/failforge run \
  --timeout 2m \
  --cwd /absolute/path/to/flutter-project \
  --stream \
  --capture-dir ./artifacts/flutter-smoke \
  -- flutter test
```

Use a fresh capture directory for each run. Check the stderr cause/status
summary and raw capture files, then repeat with Ctrl+C during a running test.
This is a manual environment smoke test, not part of the portable Rust suite.
Record its environment and result separately; success cannot be inferred from
the runner's fixture tests.

The [benchmark driver](scripts/benchmark.py) collects measurements. Record the
environment, build, commands, and workload alongside the results. Timings and
memory results describe the measured machine and workload, not universal
performance promises.

# CLI contract

The `run` subcommand runs external commands on Linux, macOS and Windows. The
child can access files, the network, and other resources allowed by its OS credentials;
`run` does not provide a system sandbox or interactive terminal. Mutation campaigns
and their isolated working copies are handled by [`mutate`](mutation-cli.md).
In `0.1.0-beta.4`, native Windows execution uses Job Objects; see the
[Windows guide](windows.md) for process, argument and cancellation differences.
The Unix-specific details below describe Linux/macOS behavior.

Build the workspace, then run the `failforge` binary:

```sh
cargo build --workspace --locked
./target/debug/failforge run --timeout 30s -- dart test
```

The default build uses release compiler settings and retains Cargo's
`target/debug` output path. Running through Cargo uses the same optimized
settings without an additional flag:

```sh
cargo run -p failforge-cli --bin failforge -- run --timeout 30s -- dart test
```

The main test suite uses a Rust helper and requires neither Dart nor Flutter.
A Dart or Flutter command is an optional smoke test on an existing prepared
project, rather than a prerequisite for the runner's tests.

## Arguments

```sh
failforge run \
  --timeout 30s \
  --cwd ./example \
  --stream \
  --capture-dir ./artifacts/run-001 \
  --env CI=true \
  --env-remove DEBUG_TOKEN \
  -- flutter test
```

A program is required after `--`. Every following argument belongs to that
program, including options named like FailForge's options and further `--`
arguments. FailForge passes a program and an array of native OS strings to the
OS; it does not join them into a shell command. Quotes in these examples belong
to the shell invoking FailForge.

```sh
failforge run -- /usr/bin/printf '%s\n' 'an argument with spaces' '' '*;$()'
failforge run -- /bin/sh -c 'printf "%s\n" "$HOME"'
```

The second command explicitly requests a shell. Executable launcher scripts with
valid shebangs are supported by the OS. Executable text without a valid shebang
is not silently retried through a shell.

| Option | Meaning | Default |
| --- | --- | --- |
| `--timeout DURATION` | Execution budget after successful spawn | No execution limit |
| `--cwd PATH` | Child working directory | Invocation directory |
| `--stream` | Display raw child stdout/stderr while capturing | Capture without continuous display |
| `--capture-dir PATH` | Directory for raw capture files | No capture files; bounded tails remain in the library result |
| `--env KEY=VALUE` | Set a child variable; repeatable | Inherit environment |
| `--env-remove KEY` | Remove a child variable; repeatable | No removals |
| `--clear-env` | Start the child with an empty environment | False |
| `--grace-period DURATION` | Budget after a graceful signal before SIGKILL | `2s` |

Stdin is disconnected with `null`, so reading it returns EOF. There is no
interactive stdin or PTY.

### Durations and time budgets

Durations are unsigned decimal integers immediately followed by `ms`, `s`, or
`m`: `500ms`, `30s`, `2m`. Fractions, signs, spaces, unknown units, and numeric
conversion overflow are errors. Leading zeroes are accepted.

An execution timeout must be positive. Grace can be zero, which requests
immediate escalation after the graceful signal. Values representable as Rust
`Duration` but not representable as a system-clock deadline are rejected by the
runner before spawn.

The execution timeout starts after successful spawn and ends at observed child
completion or the selection of a stop cause. Preparation and spawn are measured
separately. Grace, the default five-second forced wait, and the default
one-second output drain have separate budgets. Therefore `--timeout 30s` does
not promise that the CLI returns within 30 seconds. Filesystem operations and
other blocking OS calls do not have a hard real-time bound. Technical budgets
are configurable through the runner's library API.

### Paths and native values

`cwd` is made absolute and canonicalized by the runner. Relative executable
paths such as `./tool` are resolved against that effective child directory.

A bare executable name is searched using the effective **child** `PATH`, after
inheritance, removals, and assignments. Entries are considered from left to
right; relative and empty entries refer to the effective child directory.
Candidate paths are prepared in the parent and attempted with `execve` in the
fork child. Search continues after permission-denied, missing-file, and
non-directory errors (`EACCES`, `ENOENT`, `ENOTDIR`). If all candidates fail,
permission-denied takes priority over missing-file when one candidate was
denied. Other execution errors are returned immediately. There is no executable
mode-bit preselection, implicit OS fallback directory, or `ENOEXEC` shell fallback.

With `--clear-env`, supply an explicit `PATH` or a program containing `/`:

```sh
failforge run --clear-env --env PATH=/usr/bin:/bin -- tool
failforge run --clear-env -- ./tool
```

An explicitly empty `PATH` is permitted and searches the child directory.
Removing inherited `PATH` makes bare-name lookup fail. To replace `PATH`, use
`--env PATH=...` without removing it; setting and removing the same key is rejected.

`--capture-dir` is always relative to the **FailForge invocation directory**,
even if `--cwd` changes the child directory. Absolute paths are preserved.
Program names, child arguments, paths, and environment entries use native OS
strings, so non-UTF-8 Unix bytes are retained. NUL is invalid for OS parameters.

### Environment

`--env` splits at the first `=`. Empty values are accepted; empty keys are
rejected. Keys cannot contain NUL or `=`, and values cannot contain NUL.
Repeated assignments to one key use the last value. Values containing additional
`=` are preserved:

```sh
failforge run --env TOKEN=old --env TOKEN='a=b=c' --env EMPTY= -- tool
```

Setting and removing the same key is a configuration error. Environment and
working directory changes apply to the child command only: FailForge does not
modify its own global environment or working directory.

## Capture, display, and diagnostics

Capture reads stdout and stderr concurrently as bytes. Default limits are:

| Resource | Default bound |
| --- | --- |
| Read block | 32 KiB |
| Retained tail | 1 MiB per stream |
| Display queue, including the chunk being displayed | 1 MiB combined |
| Capture file quota | 256 MiB per stream |

A capture directory contains `stdout.bin` and `stderr.bin`. Files are created
exclusively; an existing file is an error and is not silently overwritten. The
saved files contain exact child bytes, including NUL and invalid UTF-8, without
FailForge messages. A file quota or capture write error stops the run and returns
a partial result rather than silently accepting truncated capture.

Without `--stream`, child output is not continuously printed. Without a capture
directory, the library result retains bounded tails rather than an unbounded
copy of the entire output. The CLI reports byte counts and truncation flags;
use file capture when the full raw output is required.

With `--stream`, output is written on a dedicated OS thread through owned,
close-on-exec duplicates of stdout/stderr descriptors. The writer uses unbuffered
`File` handles and does not change shared file-status flags. A slow or blocked
terminal cannot block the supervisor's timeout or cancellation loop. When its
byte queue fills, display fragments may be dropped, while raw capture continues.
An inherited nonblocking descriptor that returns `WouldBlock` waits for writable
readiness on the display thread; temporary backpressure does not fail the command.
The independent output-drain budget also covers that readiness wait.
The result reports dropped and unconfirmed display bytes. These are separate
from tail truncation and from capture errors. Display write failures are explicit
runner failures with partial results. There is no promised total order between
stdout and stderr; each stream preserves its own order.

An OS terminal write can remain blocked indefinitely. FailForge does not join
that thread during shutdown. The output drain budget closes queued display,
records undelivered/unconfirmed bytes, and permits the CLI to return after process
cleanup. An in-flight display write may therefore remain unconfirmed at the
frozen result snapshot. The CLI process returning terminates its detached
threads. Likewise, blocked capture filesystem operations can cause an incomplete
capture result; OS calls themselves cannot be safely forcibly interrupted.

The descriptor writer deliberately avoids `std::io::stdout`'s global
`LineWriter`: Rust's shutdown cleanup can acquire its unlocked mutex and flush
remaining buffered bytes. If a worker were paused between a buffered write and
flush, that flush could block during main-thread shutdown. Direct descriptor
writes leave no global stdout buffer to flush. The standard-library behavior is
visible in the primary [Rust 1.85.0 stdio implementation](https://github.com/rust-lang/rust/blob/1.85.0/library/std/src/io/stdio.rs)
(`stdout()` and `cleanup()`), also present in the current standard-library
source; it is an implementation detail, not a stable
blocking guarantee on which this CLI relies.

Diagnostics and summaries go to stderr. The diagnostic writer has a bounded
64-message queue with at most 4 KiB per message. Tracing is best effort. Final
messages have a 100 ms delivery acknowledgement budget each, so a blocked stderr
cannot make the process cleanup or runtime shutdown hang indefinitely. A blocked
or unavailable stderr may therefore prevent delivery of the summary. The summary
is intended for people; it is not a machine protocol.

FailForge's normal diagnostics omit raw arguments, environment values, and raw
output tails. Child capture files and streamed output **can contain secrets**
printed by the child; FailForge does not redact raw child data.
Input parsing errors print a static error category and help hint, without quoting
supplied invalid values or unknown arguments. Explicit help/version output and
standard CLI exit codes remain available.

## Cancellation, cleanup, and exit codes

The CLI installs SIGINT and SIGTERM listeners before spawning. The first Ctrl+C
requests graceful SIGINT for the managed process group. A second Ctrl+C skips the
remaining grace period and requests forced termination; it keeps supervisor
ownership and does not rewrite the original stop cause. SIGTERM to FailForge
requests cancellation with graceful SIGTERM. Execution timeouts and ordinary
programmatic cancellation use SIGTERM followed by bounded SIGKILL escalation.

The CLI waits for the runner result and explicitly calls `Runner::shutdown`
before returning from `main`; it does not call `process::exit` during cleanup.
The result's stop cause and actual OS status are separate. For example, a child
handling a timeout by exiting zero has `cause=TimedOut` and `status=exit:0`, while
the CLI returns 124.

| Condition | CLI code |
| --- | --- |
| Normal completion | Child exit code, including nonzero codes |
| Child independently terminated by signal `N` | `128 + N` |
| Execution timeout, complete cleanup | 124 |
| Runner error or incomplete cleanup | 125 |
| Ctrl+C cancellation, complete cleanup | 130 |
| SIGTERM cancellation of the CLI, complete cleanup | 143 |
| Invalid CLI arguments/configuration | Standard clap error code 2 |
| Help/version | 0 |

Cleanup failure takes priority for the CLI code, while the original stop cause
and available OS status remain in the partial result and summary. A child can
itself return 124, 125, 130, or 143; numbers alone do not distinguish those exits
from FailForge stop decisions. Inspect the result or stderr summary's `cause`
and `status` fields.

Each run uses a separate process group. Cleanup distinguishes the direct child,
observed group members, sent signals, errors, and output completion. A process
group is not an inescapable container: descendants that create another group or
session fall outside its guarantee. SIGKILL of FailForge, abrupt destruction of
its runtime, or supervisor abort cannot promise normal Unix cleanup. See
[the architecture guide in the source repository](https://github.com/AnuarAbitay/fail-forge/blob/main/docs/architecture.md)
for the lifecycle and guarantee boundary. That link follows `main`; development
documentation is not bundled in the runtime archive.

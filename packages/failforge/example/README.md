# Boundary mutation example

This directory is a standalone Dart package to test with the `failforge`
command. Its `lib/boundary.dart` contains one comparison:

```dart
bool canWithdraw(int balance, int amount) => balance >= amount;
```

The test checks an exact balance, insufficient funds and a surplus. Changing
`>=` to `>` makes the exact-balance assertion fail, so the test should kill that
mutant.

## Install FailForge

On macOS or Linux:

```sh
dart pub global activate failforge
```

On Windows, use the native installer from PowerShell. If you previously used
Pub global activation, first remove its old entry with
`dart pub global deactivate failforge`.

```powershell
dart install failforge
```

Make the installed command available on `PATH`. The Windows installer prints
its bin directory. Keep a compatible Dart SDK on `PATH`; the launcher and
native CLI discover it automatically.

## Prepare the target package

Copy this example directory into a new working directory. Alternatively, from
the FailForge repository root:

```sh
cd packages/failforge/example
```

Run the following commands from the example package directory:

```sh
dart pub get
dart test
failforge setup
failforge doctor
```

The original suite should pass one test. Initial FailForge setup downloads the
verified native runtime and prepares its adapter.

## Run one mutation

The following command works in a POSIX shell or PowerShell:

```sh
failforge mutate --project . --family classic --operator RELATIONAL_BOUNDARY --include lib/boundary.dart --workers 1 --command-timeout 60s --test-timeout 5s --output .failforge/boundary-run
```

Expected result: **one `KILLED` mutant**, mutation score **100%**, and unchanged
original source and tests. FailForge retains the results in
`.failforge/boundary-run`.

## Open the HTML report

```sh
failforge report .failforge/boundary-run --html .failforge/boundary-report.html
```

Open `.failforge/boundary-report.html` in a browser to inspect the mutation,
original source and retained test output offline. Use a new output directory
and HTML filename when repeating the example.

For reusable generated YAML profiles and further options, see the
[mutation guide](https://github.com/AnuarAbitay/fail-forge/blob/v0.2.0/docs/mutation-cli.md).

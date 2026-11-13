# HTML reports

`failforge report` turns a saved mutation campaign into one HTML file that can
be opened locally in a browser. It was added in **`0.2.0`**; the published
`0.1.0` archives predate this command.

## Export a run

With the `0.2.0` CLI, export a saved run:

```sh
failforge report .failforge/runs/RUN_ID --html .failforge/report.html
```

Replace `RUN_ID` with an existing run directory. A `summary.json` path is also
accepted:

```sh
failforge report .failforge/runs/RUN_ID/summary.json \
  --html .failforge/report.html
```

Both paths are relative to the directory where the command is invoked unless
absolute. `--html FILE` is required. The output file must not already exist, and
its parent directory must exist. Choose a new filename when exporting again.

For a multi-family profile, pass the profile's root run directory or its root
`summary.json`, then choose a family using the HTML tabs. The
`families/<family>/summary.json` publication aliases are not standalone report
inputs.

The export reads the saved report. It needs no Dart or Flutter SDK, prepared
adapter, target source checkout or network connection, and it does not run
tests. The generated file contains its styles, scripts, report data and bounded
output captures. Open it directly in a browser; no local server is required.
JavaScript must be enabled for filtering and detail navigation.

### Source lines and nearby code

New mutation results retain a small original-source excerpt with a one-based
line number and column. Columns count UTF-16 code units, as Dart does. The HTML
shows numbered original lines, highlights the affected lines and keeps the
exact original/replacement edit below them. The excerpt remains available when
the original project has moved or is unavailable.

For older saved runs without retained context, explicitly provide the original
package when exporting:

```sh
failforge report .failforge/runs/RUN_ID --html .failforge/report.html \
  --source-root /path/to/original-package
```

The exporter checks each file's full SHA-256 against the original hash saved in
the run and verifies the edit's UTF-8 byte range. A changed, missing, unsafe or
unverifiable file is marked unavailable. It never substitutes current code for
the original code without that match. Without `--source-root`, export reads only
retained report material and does not inspect the original project.

### Copy a source location

When a source location is available, **Copy location** copies the relative
`file:line:column`, for example `lib/wallet.dart:12:10`. Line and column are
one-based; columns count UTF-16 code units. The copied path is relative to the
source package.

This works directly in a standalone HTML report and needs no local integration.
If the browser cannot access the clipboard, the report shows the location as
selected text for manual copying. A mutant without verified source context
remains browsable through its exact edit and byte offset.

## Browse the results

The report presents the saved run as a working area:

- **Family tabs** keep `classic`, `failure_mode` and `language_specific` results
  separate. There is no combined mutation score.
- **Quality gates**, when configured for the saved run, show each family's
  threshold and verdict. Early-stopped runs show the maximum possible score and
  skipped-work counts while keeping the final mutation score unavailable. See
  [quality gates](quality-gates.md).
- **Summary counts** describe the selected family's entire saved scope. Choosing
  a file or outcome does not recalculate that family's score.
- **Source files and outcome filters** narrow the mutant list. Mutants that
  survived are a useful starting point for review; a survivor may expose a
  missing assertion or a behaviorally equivalent change.
- **Change details** show numbered original source context when retained or
  explicitly supplied and hash-verified, followed by the exact original and
  replacement edit fragments. UTF-8 byte offset and length remain visible even
  when source context is unavailable.
- **Test details** show available test counters, reporter state and retained
  output. Missing evidence remains unavailable; it is not replaced with an
  inferred test result.
- **Metadata** retains the recorded outcome, reason and other available
  execution details for investigation.

Only `KILLED` and `SURVIVED` contribute to a family's score:

```text
KILLED / (KILLED + SURVIVED)
```

All other outcomes are excluded. `TIMEOUT` means the outer command was stopped;
it is not a confirmed kill. Neither `SURVIVED` nor `NO_TESTS` establishes that a
source location was uncovered. The report does not infer which test killed a
mutant when that information is absent from retained evidence.

A completed campaign can contain individual `INFRA_ERROR` results. If all
mutants were processed and the original controls and final checks passed, the
family score is still calculated from `KILLED + SURVIVED`. The header says
**Run completed with errors**, and the selected family's warning identifies the
infrastructure errors excluded from its score and the number of scored mutants.
File and outcome filters do not change that scope. The mutation command still
returns `125`, so an available percentage does not imply error-free execution.

## Incomplete and empty reports

The authoritative input is the saved `summary.json`, including a terminal
summary from a failed or cancelled campaign. Such campaigns retain their
available results but have no final score. An incomplete multi-family profile
suppresses the scores of all its families, including a family that completed
before the interruption.

An error in a baseline, final original control, input-integrity check, cleanup
or publication is a campaign failure rather than an excluded mutant result.
The exporter uses the saved status and does not retroactively mark an older
failed summary as complete.

A completed run with no killed or surviving mutants has no numeric score. A
run with no candidates shows an empty result list. Missing or unstarted family
results remain distinguishable from completed results.

An initial summary can remain after a crash even when the journal and
individual mutant directories contain more evidence. The exporter does not
reconstruct a campaign from those files or use `progress.json` as proof of
completion. It does not resume or rerun an interrupted campaign. Preserve the
original run directory for a full investigation.

## Retained files and export limits

The command accepts the direct summary format and the multi-family profile
summary format. When present, a nearby `manifest.json` supplies configuration
context. Mutant capture files are read from the run's own mutant directories;
the exporter does not follow absolute paths embedded in recorded commands.

Missing optional configuration or captures do not prevent browsing the saved
mutation results. The report marks unavailable output and truncated captures.
The export limits are:

| Input or output | Limit |
| --- | ---: |
| Main summary JSON | 64 MiB |
| Mutation records across all families | 10,000 |
| Optional manifest JSON | 1 MiB |
| Preview of each capture | 64 KiB |
| Embedded capture text across the report | 8 MiB |
| Original source file used for context | 4 MiB |
| One source excerpt | Up to 7 lines and 16 KiB |
| Embedded source context across the report | 8 MiB |
| Generated HTML file | 128 MiB |

Capture previews read a bounded byte prefix and display it as UTF-8 text,
replacing invalid sequences. Text replacement also stays within the embedded
capture budget. Keep the original captures when full or byte-exact output is
needed. Malformed or unsupported main summaries and exports exceeding required
limits fail without producing a successful report.

The file can contain source fragments, filesystem paths, configuration and test
output copied from the run. Review its contents before uploading or sharing it.
Exporting a report does not establish that the saved files are authentic or
that a mutation testing quality threshold has passed.

## Platform verification

On 2026-11-11, the source before the `0.2.0` version metadata update was built
and tested on Windows 11
Pro for Workstations x64 and Ubuntu 24.04 x64 with Rust 1.99.1. Both used the
same source snapshot, verified against a 563-file SHA-256 manifest. This is
source verification; the published native `0.1.0` archives do not contain
`failforge report`. These scoped results do not establish CI or archive checks
for the final tagged `0.2.0` revision.

Focused Rust report/context tests and the report CLI integration suite passed
on both systems. Each platform exported eight shared scenario reports and
checked ten CLI cases, including export without an SDK, input preservation,
refusal to overwrite an existing report and rejection of malformed input.
Windows also exported a real saved mutation run; Linux completed a fresh
mutation run with the current CLI and verified its retained UTF-16 location.

Offline `file:` reports were exercised in Microsoft Edge 157 on Windows and
Chromium 158 on Linux. Ten desktop/mobile browser cases passed on each system:
149 interface assertions on Windows and 159 on Linux. They cover outcome/file
filters, family tabs and independent scores, source/edit/test/log details,
Unicode source paths and columns, clipboard success and manual fallback,
incomplete/empty/infrastructure-error reports, changed-source handling and
HTML metacharacter escaping. No JavaScript errors or HTTP requests were
observed. Desktop and mobile screenshots were retained for visual inspection.

The source snapshots and report inputs remained unchanged, and owned browser
processes were cleaned up. Evidence and screenshots are retained under
`artifacts/pub-fixes-html-20261111/windows-html-evidence/` and
`artifacts/pub-html-linux-20261111/` (both ignored by Git), with SHA-256-verified
result bundles and JSON verification summaries.

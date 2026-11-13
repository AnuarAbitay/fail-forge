# Mutation quality gates

These options are available in unreleased native source builds.

A quality gate requires a minimum percentage of detected mutations in a selected
family. Gates are opt-in. Without a threshold, survivors alone continue to allow
a successful command.

## Direct campaigns

```sh
failforge mutate --project ./my_package --family classic --min-score 80
```

This runs the full campaign, performs the original controls and final checks,
then compares its score with the threshold. A score equal to the threshold
passes. A completed campaign below the threshold keeps its final score and
complete report, and returns `1`.

Thresholds range from `0` to `100`, with at most two decimal places. Comparisons
use exact outcome counts and the decimal threshold, before display rounding.
For example, a value displayed as `80.00%` can still fall below a threshold of
`80` if its exact ratio is smaller.

## Profiles

Add a gate to an existing explicit profile:

```yaml
quality_gate:
  stop_when_unreachable: true
  min_scores:
    classic: 80
    failure_mode: 70
    language_specific: 75
```

Each named family must have an active campaign with enabled operators. Unknown
families and thresholds for inactive families are configuration errors. Only
named families have a gate; there is no combined or averaged profile score.
Every configured threshold must pass. Newly generated profiles keep gates off.

The ordinary profile/CLI separation applies: put gate settings in YAML when
using `--config`; do not mix them with semantic CLI options.

## Optional early stopping

```sh
failforge mutate --project ./my_package --family classic \
  --min-score 80 --stop-when-unreachable
```

`--stop-when-unreachable` requires a threshold. It stops admission of new mutants
only when the threshold is mathematically impossible to reach. A low current
percentage by itself does not trigger a stop.

For one family, let `K` be confirmed killed mutants, `S` confirmed survivors, and
`R` all candidates without a completed result, including active workers. The
most optimistic final score is:

```text
maximum possible score = 100 × (K + R) / (K + S + R)
```

The gate stops work only when this upper bound is strictly below its threshold.
For 100 candidates and an 80% threshold, the first 5 killed and 5 survivors leave
90 candidates: the maximum remains 95%, so execution continues. With 60 killed,
25 survivors and 15 remaining, the maximum is 75%, so new work stops. A maximum
of exactly 80% continues.

Already excluded results are outside the denominator. Future exclusions cannot
raise the upper bound above the case where every remaining candidate is killed.
The bound concerns the current selected files and operators; it makes no claim
about mutations outside that scope.

Started workers finish and retain their results. The original-after control,
input-integrity checks and cleanup still run, so early stopping is not
instantaneous. A gate stop is separate from Ctrl+C. Candidates not run because
of the gate are counted separately and are not assigned `SURVIVED` or
`CANCELLED` outcomes.

The admission stop applies to every worker in that family's shared queue.
It does not cancel other, independently started FailForge campaigns. Existing
tasks finish normally; a gate stop does not forcibly terminate their test
processes.

In a profile with early stopping enabled, a failing required family prevents
later families from starting. Without early stopping, all families execute and
the full set of thresholds is evaluated at the end.

## Scores, evidence and reports

The scoring formula remains `100 × KILLED / (KILLED + SURVIVED)`. `INVALID`,
`TIMEOUT` and inconclusive outcomes remain excluded and visible in the report.
An empty scored denominator is `N/A`, even with a threshold of zero; its gate is
not evaluated and the command returns `125`.

A campaign stopped before its planned work finishes has status `quality_stopped`
and `complete: false`. Its final scores remain unavailable. A separate
`qualityGate` object records the failed gate, `threshold_unreachable` reason,
upper-bound proof, threshold and counts. Profiles also retain separate gate
results for their selected families; unstarted gates are not marked passed.

The terminal and offline HTML report display the saved gate result. JSON keeps
the threshold and decision with the run, so exporting later does not apply a
different threshold. Older reports without gate fields remain readable.

An infrastructure error, failed original control, changed input, failed cleanup
or failed publication must not become an ordinary quality failure. In particular,
a numeric score above the threshold does not hide `INFRA_ERROR`,
`UNCLASSIFIED` or `NO_TESTS`.

| Exit code | Meaning |
| --- | --- |
| `0` | Execution succeeded and every configured threshold passed. |
| `1` | A threshold was missed, or an early gate stop established failure. |
| `2` | Invalid configuration. |
| `125` | Execution failure or insufficient evidence to evaluate a required gate. |
| `130` / `143` | Ctrl+C / SIGTERM cancellation. |

Execution failures and cancellation retain their existing priority over a
quality failure. Gates do not add automatic resume or early success: an apparent
pass still needs the complete campaign and its final checks.

# Native profile report fixtures

These are complete JSON reports emitted by the production Rust CLI during the
2026-10-20 real Dart SDK regression. They retain the serializer's actual field
structure, all family summaries, records, controls, scores and cleanup evidence.
The contract tests read these files directly; they do not reconstruct a profile
report from a hand-written schema.

| Fixture | Retained source | Source SHA-256 |
| --- | --- | --- |
| `native-profile-all-families.json` | `artifacts/bugfix-regression/2026-10-20/sdk-all/all-families/summary.json` | `e98ce501ebe75fa82b0905c6570c5abed154aa97ede5e24eb16793d5be682616` |
| `native-profile-zero.json` | `artifacts/bugfix-regression/2026-10-20/sdk-all/profile-zero/summary.json` | `7a9f8ec33fe996ea8d7dc308f152d65c25b0d132fb4b5def40e68d2846c48efb` |

Sanitization replaces only string path prefixes: the local installation becomes
`/fixture/install`, and private temporary roots become `/fixture/tmp`. JSON
formatting is normalized. Fields, arrays, values other than those path strings,
and nullability are preserved. Fixtures do not execute any referenced command.

The full profile has five killed mutants across all three families. The empty
profile has all three families enabled, zero mutants, zero denominators and null
percentages. Both have **null aggregate score fields**: scores belong only to
individual family summaries. Tests derive explicitly labeled invalid or partial
variants from these native reports to check score suppression and rejection.

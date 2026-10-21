# Security policy

## Supported versions

This policy covers FailForge `0.1.0-beta.1` and the current `main` revision.
Include the affected version and exact commit ID in a security report. There is
no stable support or backport schedule during the beta.

## Report a vulnerability privately

Use the repository's
[Security Advisories page](https://github.com/AnuarAbitay/fail-forge/security/advisories)
and choose **Report a vulnerability**. Private Vulnerability Reporting is enabled
for this repository.
GitHub's [private reporting guide](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/report-privately)
explains how to submit the report.

If private reporting is unavailable, open a
[GitHub issue](https://github.com/AnuarAbitay/fail-forge/issues) titled
**Private security contact requested**. Include only a request for a private
contact channel. Keep vulnerability details, affected private projects,
logs and proof-of-concept code out of the public issue. Wait for a private
channel before sending those details.

Do not disclose a suspected vulnerability through a public bug report or pull
request. Coordinate disclosure with the maintainer while the report is assessed
and a fix is prepared.

## What to include in the private report

- FailForge version and commit ID, operating system and architecture.
- Dart/Flutter SDK versions and the relevant command or sanitized YAML profile.
- A minimal reproduction and the expected versus observed behavior.
- The security impact and the conditions needed to reproduce it.
- Relevant sanitized diagnostics, with paths and credentials removed.

Use a disposable test project. Do not include tokens, `.env` secrets, credentials,
private source code or unreviewed capture archives. If an attachment is necessary,
describe what it contains before sharing it privately.

There is no guaranteed response or fix deadline during the beta. Keep
follow-up information in the same private conversation.

## Security boundaries

FailForge intentionally executes project tests and, through `failforge run`,
user-selected commands. They have the filesystem, network and other access
granted to the executing account. Working copies protect the original project
from FailForge's mutation patches; they do not sandbox malicious tests.

Use only trusted projects, or provide your own operating-system isolation.
Do not give untrusted test jobs production credentials. Reports, raw captures
and streamed test output may contain sensitive information; FailForge does not
redact arbitrary test output.

Unexpected modification of files outside an owned working copy, unsafe cleanup,
exposure of secrets by FailForge itself or a bypass of its documented input and
path checks are appropriate topics for a private security report. Ordinary test
failures, surviving mutants and expected command timeouts belong in public bug
reports after sensitive information has been removed.

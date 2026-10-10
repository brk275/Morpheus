# Contributing to Morpheus

Morpheus welcomes contributions to its native Rust Matrix client. The source is
public pre-alpha software under AGPL-3.0-or-later; installer releases follow the
platform and security acceptance checks. Screenshots and the offline preview
use fictional accounts and conversations.

Start with the [feature matrix](docs/FEATURE_MATRIX.md) and existing issues.
Open an issue for a substantial design change, or submit a focused pull request
that explains the behavior, tests, and remaining limitations. Treat people
respectfully and discuss the work. Contributions use the repository license;
keep third-party copyright and license notices intact.

Read [AGENTS.md](AGENTS.md) before building or changing workflows. Choose checks
by impact, reuse valid evidence, and avoid duplicate or paid builds. Ordinary
documentation edits do not need native packaging. Shared native changes need
the supported platform matrix before they can establish compatibility.

## Privacy and testing

You may contribute under a pseudonym. Configure a GitHub noreply commit email
and inspect commit metadata before pushing. Your GitHub handle, contributions,
issues, and public CI logs remain visible; pseudonyms do not guarantee anonymity.

Use the disposable local test instructions in [tests/LOCAL_MATRIX.md](tests/LOCAL_MATRIX.md)
and [tests/LOCAL_MATRIX_RTC.md](tests/LOCAL_MATRIX_RTC.md). Keep generated
credentials outside every source checkout. Do not paste tokens, recovery keys,
real conversations, personal paths, or account databases into issues or logs.

Report vulnerabilities through the private channel in [SECURITY.md](SECURITY.md).
Public pull-request workflows require maintainer approval for outside
contributors and receive no signing credentials. CI results do not authorize
installer publication or bypass the release checklist.

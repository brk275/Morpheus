# Security policy

Morpheus is pre-alpha software. There is no supported production release yet.
Public source and passing CI are not evidence of completed security, encryption
recovery, device, or platform acceptance. Use disposable accounts when testing.

## Report a vulnerability privately

Use [GitHub private vulnerability reporting](https://github.com/brk275/Morpheus/security/advisories/new).
Include the affected revision, platform, expected behavior, and a minimal
reproduction with synthetic data. Do not open a public issue with an unpatched
exploit or attach account tokens, passwords, recovery keys, private messages,
signing keys, or full account stores.

If the private report form is unavailable, open a public issue asking for a
private reporting channel without including vulnerability details or secrets.
This is a volunteer project; no response-time guarantee is offered.

## Development safeguards

- Store credentials in the OS credential vault. Never add plaintext fallback
  tokens to configuration, logs, test fixtures, or update metadata.
- Use generated loopback test accounts; fixture scripts reject paths inside
  this checkout. Keep reports and screenshots free of personal data.
- Review local dependency patches when updating their upstream versions.
  See [dependency review](docs/DEPENDENCY_SECURITY.md).
- Use read-only CI credentials, pinned actions, and approved fork workflows.
  Signing and release credentials belong in a future protected release
  environment and must never be supplied to pull-request builds.

The existing [release checks](docs/RELEASE_CHECKLIST.md) apply before installer
distribution. No current artifact should be treated as a secure production
messaging client.

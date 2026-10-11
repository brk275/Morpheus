# Delegated OAuth acceptance

The Linux Matrix acceptance harness includes an opt-in protocol test against real
Matrix Authentication Service (MAS), Synapse, PostgreSQL, and a disposable OS
Secret Service vault. It creates a fresh local account without email or an
external identity provider. No account is registered on a public homeserver.

This test is implemented but does not constitute release evidence until its
hosted run passes for the candidate being reviewed.

## What the test checks

`scripts/run_matrix_acceptance.py` creates one fresh authentication fixture and
runs `crates/morpheus-matrix/tests/oauth_local.rs` in two separate test processes:

1. **Login and refresh:** discover delegated authentication through Synapse,
   register a native OAuth client with MAS, cancel a pending attempt and check
   its store is removed, then complete a new authorization code flow with S256
   PKCE. Authenticate with the fixture's password, submit the real MAS consent
   form, reject a repeated callback, and verify the Matrix user/device identity.
   Explicitly refresh the real tokens, require both access and refresh tokens to
   rotate, and wait for the new tokens to reach the OS vault. Inspect the private
   account profile for plaintext credentials.
2. **Restart and automatic refresh:** open the same profile in a new process,
   restore the delegated session from the vault, and verify the same device.
   Wait for the server's supported 60-second access-token lifetime to expire.
   Require a direct request using that token to receive HTTP 401, then require
   the SDK's authenticated request to succeed and rotate both tokens
   automatically. Verify the vault update and absence of plaintext profile
   tokens, then log out remotely and require local account/vault removal.

The harness rejects skipped tests, empty test results, and missing or false
stage evidence. Public evidence contains only allowlisted check names, booleans,
test counts, source locations, durations, and fixed diagnostic stage/failure codes.
On failure, the last attempted stage distinguishes fixture setup, browser forms,
login, refresh, restore, remote logout, account removal, and vault removal without
exposing a URL, command line, or server error. Each logout result is required
separately; a successful remote result means the SDK accepted the revocation
response, not an independent probe that every previously issued token is unusable.
Raw test output is scanned before
any public test summary and is never printed or uploaded.

## Browser protocol boundary

The helper uses Python's standard HTTPS client, a cookie jar, and the pinned MAS
login/consent forms. It posts their CSRF fields and credentials only to the
fixture issuer, follows redirects manually, and intercepts the final callback
without fetching it. The Rust test passes that URL to Morpheus's normal account
manager API. It reserves a loopback callback port but does not exercise the app's
HTTP callback listener.

This provides protocol evidence for Morpheus's delegated account lifecycle. It
does **not** verify opening an OS browser, the real browser window, app focus,
callback listener delivery, accessibility, or an external OIDC provider. Those
desktop acceptance checks still apply on macOS, Windows, and Linux. Upstream
SSO, email verification/recovery, MFA, public HTTPS deployment, multiple providers, and
provider-specific policy are outside this fixture.

## Isolated services and credentials

`scripts/local_matrix_auth.py up --root PRIVATE_DIRECTORY` requires a disposable
GitHub-hosted Linux runner: its containers use host networking, and it temporarily
installs its public CA certificate in that runner's system trust store. PostgreSQL,
Synapse, and both MAS listeners bind only to `127.0.0.1` at separately reserved
ephemeral ports. The fixture disables
public registration, federation, URL previews, and password recovery. MAS's CLI
creates its disposable account; no registration-policy or native redirect-policy
exception is installed. Morpheus's production OAuth validation is unchanged.
The helper checks the runner's existing OpenSSL, `update-ca-certificates`, `sudo`, GNU `timeout`,
`install`, and `rm` executables before creating fixture ownership or keys. It
does not install tools, change runner class, or use a paid service.

MAS serves its advertised issuer, authorization, token, and revocation URLs over
native TLS using a one-day certificate for `127.0.0.1`. The SDK and Python use
ordinary certificate verification. No `SSL_CERT_FILE`, `SSL_CERT_DIR`, Requests,
or curl CA-bundle override is accepted during fixture creation. Synapse uses a
separate HTTP loopback listener for MAS discovery and introspection: the pinned
Synapse code derives both URLs from its configured internal endpoint, while
discovery still advertises the HTTPS issuer. The native callback remains HTTP
loopback as permitted for desktop OAuth; authorization pages never downgrade.

This corrects a deterministic fixture mismatch found after candidate `3f1a3df`
passed restore, expired-token rejection, automatic refresh, and vault persistence,
then failed at logout. The pinned `oauth2` 5.0.0 library rejects an HTTP revocation
URL before sending a request, as required by RFC 7009. The prior public log did
not expose the precise returned error, so additional unrelated logout failures
remain possible until the corrected hosted fixture passes.

The root must be fresh and outside the checkout. Directories use mode `0700`;
generated passwords, keys, configuration, fixture metadata, and diagnostics use
mode `0600`. Docker's logging driver is disabled. Subprocess failures expose
fixed diagnostic labels, never command lines or server bodies. The browser
driver receives credentials on stdin and returns its callback through a captured
pipe. Neither goes in environment variables or shell arguments. The upstream
MAS registration CLI only accepts its password through an argument; that Docker
command is captured privately and its argv is never formatted in failures.

Each run has a random ownership label. Cleanup resolves full container IDs and
rechecks the label before removing those containers and their anonymous volumes.
It never stops unrelated fixtures. Failed runs keep their private diagnostic
directory after container cleanup; successful cleanup removes the service data.
The exact public CA hash and nonce-derived system filename are recorded before
installation. Cleanup validates that identity, removes only that certificate,
and refreshes system trust even after a partially completed prior removal. It
attempts CA cleanup after Docker errors or interruption, and retains the ownership
record whenever cleanup fails. Private CA/server keys remain in the private
fixture directory and are never installed into system trust.
Docker cleanup has a 45-second total budget. Each privileged trust command runs
under GNU `timeout` inside `sudo` (20 seconds, then a two-second kill grace), with
process-group signalling enabled. The parent waits on its retained child handle,
including after interruption, for at most 30 seconds plus a three-second reap
limit. These cleanup bounds fit within the parent's 120-second limit. Killing
only the unprivileged `sudo` wrapper is not used as the privileged command's
containment mechanism.
The runner's temporary profile/keyring and raw acceptance logs are not uploaded.

## Pinned inputs

The helper pins version tags and immutable multi-architecture manifest digests:

| Service | Version | Manifest SHA-256 |
| --- | --- | --- |
| MAS | 1.26.0 | `e089f1048a1d4a9a492ed17b9fe759100f1bd619407b001f5927928d88b780c4` |
| Synapse | 1.162.0 | `6b84a7bbac36f080b2d2e51e0289cf1b08b349598ea44a558df38d558f2c2311` |
| PostgreSQL | 17.10-bookworm | `9b18b78397054fce88a9552e9d5a3ad5bb7fd258c5b3cc1c5028e46373d6ea8f` |

The fixture starts from vendor-generated MAS and Synapse configuration, retaining
MAS's generated encryption/signing keys and bundled templates and policy. Its
loopback/database overlays replace complete relevant sections to avoid retaining
a generated public listener or database URI. MAS uses PostgreSQL; Synapse uses a
fresh private SQLite store. No existing Matrix or RTC volume is reused.

Primary references for the pinned fixture:

- [MAS 1.26.0 release](https://github.com/element-hq/matrix-authentication-service/releases/tag/v1.26.0)
- [MAS homeserver integration](https://github.com/element-hq/matrix-authentication-service/blob/v1.26.0/docs/setup/homeserver.md)
- [MAS configuration reference](https://github.com/element-hq/matrix-authentication-service/blob/v1.26.0/docs/reference/configuration.md)
- [MAS management CLI](https://github.com/element-hq/matrix-authentication-service/blob/v1.26.0/crates/cli/src/commands/manage.rs)
- [MAS native-client registration policy](https://github.com/element-hq/matrix-authentication-service/blob/v1.26.0/policies/client_registration/client_registration.rego)
- [MAS experimental token TTL schema](https://github.com/element-hq/matrix-authentication-service/blob/v1.26.0/crates/config/src/sections/experimental.rs)
- [Synapse delegated-authentication configuration](https://github.com/element-hq/synapse/blob/v1.162.0/synapse/config/mas.py)
- [MAS native TLS listener configuration](https://github.com/element-hq/matrix-authentication-service/blob/v1.26.0/crates/config/src/sections/http.rs)
- [Synapse internal metadata and introspection endpoints](https://github.com/element-hq/synapse/blob/v1.162.0/synapse/api/auth/mas.py)
- [RFC 7009 token revocation requires HTTPS](https://www.rfc-editor.org/rfc/rfc7009#section-2)
- [Official PostgreSQL image entrypoint](https://github.com/docker-library/postgres/blob/master/docker-entrypoint.sh)

## Running checks

Follow `AGENTS.md`: do not start a native build or services merely because this
document lists them. The existing free Linux native job runs the complete
acceptance harness inside its disposable D-Bus session. A fixture/helper-only
change selects Linux through `scripts/ci_scope.py`; shared native changes still
require all supported platforms.

Offline checks do not start Docker or compile Rust. The real HTTPS cookie-jar
test is skipped outside the disposable Linux Actions runner; local checks use
mocks for certificate tooling and privileged operations:

```sh
python3 -m unittest scripts.tests.test_local_matrix_auth scripts.tests.test_matrix_acceptance scripts.tests.test_ci_scope
```

The Rust test does nothing unless `MORPHEUS_MATRIX_OAUTH_FIXTURE`,
`MORPHEUS_MATRIX_OAUTH_PROFILE`, and `MORPHEUS_MATRIX_OAUTH_PHASE` are configured by
the harness. The two phase values are `login_refresh` and `restore_refresh`.
Fixture files and profiles must remain private and outside the checkout.

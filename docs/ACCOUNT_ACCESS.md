# Account access

Morpheus keeps separate Matrix accounts and aliases in one application. Each
saved account has its own device identity, credentials, encrypted local store
and drafts. The bottom-left menu changes the selected account; inactive accounts
pause by default.

## One local app password

Open **Manage accounts → Set app password** to require a local password when
opening another saved account or restoring an account after startup. SSO and
password-based Matrix accounts use the same local protection. A new Matrix
sign-in saves the account without bypassing this prompt. Re-selecting the
already open account does not constitute a new switch.

Use at least 12 characters, with a maximum of 1024 UTF-8 bytes. Changing or
turning off protection requires the current password. A failed or cancelled
unlock does not authorize another account. Calls and recovery operations must
finish before account changes; account operations also exclude an update
restart.

The app password is an application access control. It does not replace Matrix
encryption or protect against someone who controls the operating-system user
account. A fixed-parameter Argon2id verifier is held in the OS credential vault;
neither the password nor its verifier belongs in `config.toml`. A separate
non-secret policy file records whether protection is enabled. An unavailable
or invalid verifier refuses protected activation. Password fields clear on
submission and cancellation; Rust-owned password buffers are zeroized when
dropped. Toolkit and operating-system memory cannot be guaranteed erased.

### Missing policy files in pre-alpha profiles

Startup refuses an existing profile whose `account-access.json` is missing or
invalid. It preserves the account registry and stores; it does not silently
turn protection off or read the vault to guess the old policy. A disabled
policy is created only for a new profile with no account registry and an empty
account directory. This deliberately does not migrate older pre-alpha
profiles that lack the policy file. Restore that profile's original policy
file from your own backup, or keep the old profile intact and use a separate
new profile to sign in again. Do not fabricate a replacement policy file.

## Quick removal

Choose **Remove** beside any saved account, then confirm the account shown.
You do not need to open it or know the app password to remove it. Removal
revokes local account access, stops its workers and deletes its vault credential
and local store. It does not select another account automatically.

This path works offline and makes no homeserver logout request. The homeserver
may still list the device; revoke it from another client when needed. Keep a
recovery key or another verified device before removing your last verified
device. The separate **Log out** action attempts homeserver logout and reports
when that remote step could not be confirmed.

If the vault is locked, a token write is unfinished or another component holds
a database file open, the account stays visible as **Removal pending** and
cannot be opened. Choose **Retry removal**, restarting Morpheus first if files
remain open. Success is reported only after both local credential and store
cleanup are confirmed.

An interrupted or failed sign-in can also leave an account marked **Removal
pending**. Its registry row owns any partial vault write, so it cannot be
opened and remains available for explicit cleanup. Startup reads the local
commit marker and registry without querying the vault; an incomplete commit
is blocked before restore. A locked or malformed marker refuses startup and
preserves the files. It is never treated as permission to delete account data.

Morpheus synchronizes the registry file before replacement and the parent
directory on Unix; Windows registry replacement requests write-through. These
are the filesystem's durability mechanisms, not a guarantee against faulty
storage or every power-loss scenario. If marker cleanup is interrupted after
sign-in, restart conservatively lists that account for cleanup even when its
credential was saved. Removing it deletes its local keys and history, so keep
the recovery key or another verified device as described above.

If you forget the app password, remove every saved account first. Only after
all account credentials and stores have been removed does **Reset password
after removing all accounts** become available. This explicit reset cannot
unlock existing accounts.

## Verification status

At `a38fe43` ([run 38737708120](https://github.com/brk275/Morpheus/actions/runs/38737708120)),
the Linux live fixture passed protected A→B→A switching, rejection of direct,
wrong-password and cancelled activation, locked restart and unlock, inactive
and active removal, credential/store deletion, and reset after complete removal.
It uses two disposable homeservers and an isolated real OS credential vault.
Protected classic login stayed paused until authorization; both OAuth refresh
phases also passed.

Fresh native Linux captures passed visual review of Manage accounts at 100%
and the password dialog at 100% and 200% text sizes. These complement the mock-vault
regressions and real Slint keyboard/pointer harness; they do not establish
VoiceOver, Narrator or Orca acceptance. Both Mac architectures passed isolated
native-vault core checks at `3fe1285`; Windows passed them at `cad1abc`
([run 39057684016](https://github.com/brk275/Morpheus/actions/runs/39057684016)).
Installed-app and real Matrix sign-in checks on those platforms remain open. Exact-intent logout and the generated-Slint sign-in keyboard
regressions passed on all four targets. Linux's separate system Wayland capture
test failed, so aggregate CI remains failed. These results are not beta acceptance. See
[verification](VERIFICATION.md).

### Isolated native vault checks

The manual [native-vault workflow](../.github/workflows/native-vault-qa.yml)
checks both Mac architectures and Windows x64 on fresh standard hosted runners.
After selecting and hashing the exact compiled core test, it runs parser and
cleanup contracts, then exercises two separate processes using a temporary
profile and isolated generated OS-vault entries. It covers password changes and
restart persistence, wrong/cancelled/stale authorization, exact inactive account
removal, and reset only after all accounts have been removed.

On macOS it creates a dedicated keychain and restores the original keychain
preference metadata after every owned process has stopped. On Windows it uses
exact generated credential targets and a private directory restricted to the
runner and SYSTEM. Unknown cleanup remains a failed check. Credentials and raw
OS output are not published or uploaded. This fixture performs no Matrix login
or successful account activation; Linux's two-homeserver test and installed UI
checks cover different requirements.

The fixture and wrapper have source reviews and pure/mocked regression coverage.
Both Macs passed all five contracts, 13 preparation and 11 restart/removal checks
at `3fe1285`; Windows passed the same checks at `cad1abc`. Every owned child and
private profile was cleaned up. These synthetic core checks do not authenticate
a real Matrix account or verify installed UI. Select `platform=all` for
initial acceptance, or only the affected platform when diagnosing a failure.
Check active runs and completed cache posts before dispatching. This workflow
does not distribute a package or complete the beta gate.

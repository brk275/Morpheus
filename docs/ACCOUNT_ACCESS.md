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

If you forget the app password, remove every saved account first. Only after
all account credentials and stores have been removed does **Reset password
after removing all accounts** become available. This explicit reset cannot
unlock existing accounts.

## Verification status

This implementation is awaiting native compilation and regression execution on
the four supported runner targets, real credential-vault/restart checks and
interactive keyboard/screen-reader acceptance. Source tests and review are
not beta acceptance. See [verification](VERIFICATION.md).

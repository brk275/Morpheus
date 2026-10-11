# AppImage update transaction

This is an implemented recovery path awaiting native and installed-package
acceptance. It is not a completed signed-update release gate. Update controls
and the public feed remain disabled. No signing credentials ship in source.
macOS and Windows installation still returns `InstallNotValidated`; they need
separate platform transactions, signature/notarization checks where applicable,
and installed-package failure/recovery evidence.

## Packaging order

1. Build the native Linux x86_64 application and package its Type 2 AppImage.
2. Append the bounded compatibility footer using
   `scripts/prepare_appimage_update.py --binary target/release/morpheus-app <artifact>`.
   `scripts/package.py` performs this step for newly produced QA AppImages.
   The metadata command returns before UI initialization or account storage.
3. Sign the **complete final AppImage**, including the footer, with the official
   Cargo Packager signer. Its pinned 0.11.8 CLI supports
   `cargo packager signer sign <artifact>` and creates `<artifact>.sig`.
   Keep signing secrets out of arguments/logs; the actual credential-backed
   release process is not configured or verified yet.
4. Generate feed entries from the exact signed artifact and its official
   signature. Do not repackage, append, or otherwise change the signed bytes.

Cargo Packager's combined package-and-sign operation signs before this external
footer step. Accordingly, the QA package script rejects the signing-key
environment variable on Linux, and the footer helper rejects existing `.sig`
or `.minisig` files. This avoids quietly invalidating or reusing signatures.
The footer adds compatibility information; it does not replace the official
minisign format or authenticate an unsigned QA artifact.

## Compatibility and account data

The signed footer binds product identifier, version, architecture, updater
protocol, and a conservative persistence fingerprint. The latter includes
Matrix/configuration source, app draft/voice-space persistence code, Cargo
manifests, enabled app features, vendored dependency patches, and the dependency
lock. Only the app package version is excluded. A dependency, feature, or
persistence-source change therefore requires a separately verified migration
release rather than an automatic update in this initial implementation.

The updater checks the signed bytes and footer without executing a downloaded
application to discover compatibility. It also checks that the installed
AppImage's footer matches the running application's embedded metadata. Older
packages without this protocol need a manual installation first.

**The updater never copies, snapshots, restores, or migrates Matrix databases,
cryptographic state, credentials, drafts, or user configuration.** Rollback is
limited to application bytes before the new process opens account storage.

## Transaction and restart

- A packaged app retains an exclusive per-installation lifetime lock until its
  account, call, and runtime shutdown completes. Unsupported read-only/shared
  installations can run without in-app update support.
- Preparation writes a verified executable sibling and an independent copy of
  the current AppImage in a private directory beside the installed file. Both
  files and the journal are flushed. The public application path is unchanged.
- Dropping or cancelling a prepared restart removes the owned preparation and
  releases the call/update exclusion. The helper must signal readiness before
  the old application is asked to quit; launch failure leaves it running.
- The helper waits for the old process's lifetime lock, writes durable intent,
  and atomically renames the verified sibling over the installed AppImage on
  the same filesystem. The application path is never removed first. Both
  affected directories are flushed, and the previous binary remains available.
- The new process checks its transaction identity, artifact hash and embedded
  metadata, acquires the lifetime lock, and initializes/shows its Slint window.
  It writes a durable commit **before `AccountManager::open` or configuration
  migration**. The 45-second supervisor deadline covers this pre-store phase.
- If this phase fails, the helper stops its owned child, obtains the lifetime
  lock, rereads the journal and atomically restores the known previous binary.
  A committed marker always prevents rollback, including the timeout/commit
  race. Later store or application failures keep the committed version.
- Restart after interruption recognizes whether the installed hash is old or
  new even if the crash occurred between rename and journal update. Unknown
  replacements, corrupted backups, invalid journals and filesystem failures
  preserve recovery material and stop before account storage is opened.

The UI integration contract is `StagedUpdate::prepare_restart` with the real
shared `UpdateActivity` and startup `AppImageSession`, followed by explicit
`PreparedRestart::launch_supervisor`, then orderly app shutdown. The call
controller retains the same exclusion through join and final cleanup.

## Verification still required

Unit tests cover bounded metadata, compatibility/version mismatches, concurrent
locks, cancellation, failed helper execution, application replacement, every
journal/rename interruption boundary, corrupt staging, unrelated replacements,
pre-store commit, and preservation of account-data sentinel files. Python tests
cover metadata/footer preparation, unchanged ELF/SquashFS bytes, signature
ordering, permissions, symlinks and malformed metadata. Synthetic filesystem
fixtures do not claim successful AppImage mounting or encrypted-store recovery.

Before enabling a feed, run real signed old/new AppImages through native launch,
upgrade, power/process interruption, recovery, cancellation, moved paths,
read-only and full filesystems, Wayland/X11, quit/restart and corrupted downloads.
Verify the appended footer is accepted by the actual AppImage runtime and that
the official signature covers it. Test the full shared call exclusion and prove
that no account database opens before trial commit. Run ordinary post-update
account/encryption acceptance separately; a pre-store commit does not claim
that those later operations succeeded.

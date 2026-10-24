# AppImage update transaction

This is an implemented recovery path awaiting native and installed-package
acceptance. It is not a completed signed-update release gate. Settings now has
explicit Check, Download, Discard and Restart controls; they remain disabled
without a compiled feed and verification key. The public feed is not enabled.
No signing credentials ship in source.
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
lock. The app package version and ordinary UI/updater implementation are not
persistence inputs. A dependency, feature, or
persistence-source change therefore requires a separately verified migration
release rather than an automatic update in this initial implementation.

The updater checks the signed bytes and footer without executing a downloaded
application to discover compatibility. It also checks that the installed
AppImage's footer matches the running application's embedded metadata. Older
packages without the current protocol need a manual installation first.
Protocol **2** requires the successful ordinary-shutdown authority and original
process capability described below. Both forward directions between protocol1
and protocol2 fail before preparation; a new candidate cannot silently use an
older lock-only helper. The existing `METADATA-V1` footer marker still identifies
the bounded footer encoding, not the forward-update protocol. Existing Linux
journal1 records remain readable only for the original hash-bound interrupted
rollback or committed cleanup, never as a new forward authorization. An unknown
manual replacement, mismatched bytes or an unresolved legacy state preserves
recovery material and requires explicit manual recovery; this is not a general
promise of automatic migration from legacy installations.

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
- Before readiness, the helper captures a kernel pidfd for the original socket
  creator's complete thread group and binds the full prepared journal to an
  unnamed private socket. Kernel socket credentials must match the original
  preparing process and effective owner. `SO_PEERPIDFD` is preferred; a kernel
  without it requires an exact direct-parent check before and after
  `pidfd_open(..., 0)`. A runtime wrapper or unavailable capability fails closed
  rather than permitting a numeric-PID wait. Older-kernel/runtime acceptance
  remains unproved until exercised on that exact installed route.
- The complete ordinary application function must return success and drop its
  UI/runtime/owner locals, including all mandatory typed cleanup results and
  acceptance completion. Unique startup-session ownership is then recovered
  and the full prepared journal and all three image hashes are rechecked before
  consuming a one-shot success authority. An error, panic, failed unique-owner
  recovery, partial write or ordinary Drop never generates that authority.
- The helper requires the exact transaction-bound token and EOF, then actual
  original thread-group death, independently reacquires the lifetime lock and
  rechecks the complete original journal and image hashes. Missing authority
  leaves preparation recoverable. The token is a successful application-scope
  receipt, **not** a kernel exit-status or complete SDK/backend-disposal proof.
  It writes durable intent only after these requirements,
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

The application update controller runs discovery, download, verification and
preparation on blocking workers. It calls `StagedUpdate::prepare_restart` with
the real shared `UpdateActivity` and startup `AppImageSession`, then
`PreparedRestart::launch_supervisor`. Only the current helper-ready result can
request UI shutdown. The startup session is owned outside the controller and
outlives UI, account workers and the runtime. Normal window close also drains
controller work; cancelling a transfer discards its result after the existing
bounded network operation finishes. There is no automatic polling or install.

The call controller retains the same exclusion through join and final cleanup.
Restart also reserves the account security gate, so active recovery and an
unacknowledged new recovery key block it. Once restart preparation starts, a
new recovery operation cannot begin. A known preparation failure releases that
reservation; successful handoff keeps it through orderly shutdown.

## Verification still required

Unit tests cover bounded metadata, compatibility/version mismatches, concurrent
locks, cancellation, failed helper execution, application replacement, every
journal/rename interruption boundary, corrupt staging, unrelated replacements,
pre-store commit, and preservation of account-data sentinel files. Python tests
cover metadata/footer preparation, unchanged ELF/SquashFS bytes, signature
ordering, permissions, symlinks and malformed metadata. Synthetic filesystem
fixtures do not claim successful AppImage mounting or encrypted-store recovery.
Controller tests cover explicit transitions, stale/cancelled results, failed
call exclusion, uncertain worker failure and draining concurrent shutdowns.
The new controller/UI integration and security-gate regressions await hosted
native verification; model tests are not installed signed-update acceptance.

Before enabling a feed, run real signed old/new AppImages through native launch,
upgrade, power/process interruption, recovery, cancellation, moved paths,
read-only and full filesystems, Wayland/X11, quit/restart and corrupted downloads.
Verify the appended footer is accepted by the actual AppImage runtime and that
the official signature covers it. Test the full shared call exclusion and prove
that no account database opens before trial commit. Run ordinary post-update
account/encryption acceptance separately; a pre-store commit does not claim
that those later operations succeeded.

# macOS update payloads

The native Mac bundle transaction is implemented behind a closed installation
admission gate. Source inspection and unsigned payload generation do not prove
installed updates, signing, notarization or power-loss recovery. Those release
checks remain required on Apple Silicon and Intel Mac.

## One bundle, initial installer and update input

The Mac branch of `scripts/package.py` uses the existing Cargo Packager build
once. A DMG request explicitly retains `app,dmg`: Packager builds the `.app`
first, then uses it for the DMG. Keeping that same intermediate bundle prevents
Packager's normal DMG-only cleanup from deleting the update input.

`scripts/prepare_macos_update.py` then produces
`dist/Morpheus.app.tar.gz` from the unchanged `dist/Morpheus.app`. It never
builds, launches, installs, signs or rewrites the application. Existing output
is never overwritten. Ordinary installed-package smoke checks still mount and
inspect the DMG, and installers remain on the ephemeral QA runner.

The archive is **unsigned signing input**. Automatic Cargo Packager signing
variables are rejected before packaging; they would select its generic archive
producer. A separately approved release step must sign these final bounded
archive bytes with the official Cargo Packager signer and publish the matching
signature/feed only after the release checks pass. Apple code signing and
notarization remain separate requirements. The producer does not read a key.

## Identity and complete contents

The producer parses the native Mach-O executable's immutable v1 identity
section without running it. Product, native architecture, requested version
and persistence cohort must match. It validates the same closed XML plist
shape as the native transaction: one root dictionary of unique string/boolean
scalars, including the actual top-level bundle identity and all permission
purpose strings.
The separately parsed plist bytes must match the initial tree's regular-file
record, length and SHA-256 before they can establish that identity.

Every file, directory, mode and internal relative symbolic link is preserved.
The complete tree is hashed before and after archive production. The required
bundled resources must also match the source bytes:

- `themes/aurora.toml` → `Contents/Resources/aurora.toml`
- `LICENSE` → `Contents/Resources/LICENSE`
- `licenses/Symphonia-MPL-2.0.txt` → `Contents/Resources/Symphonia-MPL-2.0.txt`
- `licenses/Symphonia-NOTICE.txt` → `Contents/Resources/Symphonia-NOTICE.txt`

All remaining bundle resources and code-signing files are included unchanged;
the producer does not replace the generated plist or reduce the bundle to its
main executable.

## Bounded format

The output is one gzip stream containing concrete USTAR records under one
`Morpheus.app` root. No hard links, special filesystem entries, PAX/GNU extension
records, external/dangling links, case aliases, special permission bits or
shared-writable regular files/directories are accepted. Ordinary symbolic-link
permissions, including Darwin's normal `0777`, are preserved exactly. Extraction
and backup copying use Darwin's no-follow `lchmod` API on the owned link and
verify its restored mode and inode; they never chmod its target. Filesystems
that cannot restore the exact mode fail closed. Paths must be ASCII, fit USTAR directly and stay within the
native reader's path/depth bounds. Unsupported input fails before signing;
there is no permissive archive fallback.

Limits are 512 MiB per file and compressed archive, 1 GiB for expanded archive
bytes, 32,768 total concrete entries, and 32 path components. File contents are
checked again while streaming into the archive; failures remove only the
producer's newly created temporary/output inode. The application is read-only
input throughout.

A successful producer receipt records the exact archive and complete bundle
hashes, entry count, architecture, version and `signed: false`. It is a build
receipt, not an installed-update or distribution approval.


### Symlink permission admission

Update archives and installed bundle identity/hash/copy paths admit only
owner-readable ordinary symlink modes: `0400` must be set, all other ordinary
permission bits are retained exactly, and special bits/hardlinked links remain
rejected. Standard `0777`, private `0700`, `0640`, `0400` and `0444` links remain
supported. Unreadable modes, including `0000`, owner write/execute-only and modes
with only group/other read permission, are rejected before archive link creation
or requested link-mode restoration; producer snapshot/header admission matches.
No installed bundle permission is temporarily changed to inspect a link target.

The restriction follows the Darwin source contract: Rust 1.99 canonicalization
calls libc `realpath`, Apple's implementation calls `readlink` for link text,
and XNU authorizes `readlink` with `KAUTH_VNODE_READ_DATA`, which requires the
owner-read bit for these owned links. The earlier generic statement that link
access bits are ignored when following a path does not imply that an application
may read the link's stored target text. ARM run 39934040994 failed in the prior
positive mode loop with EACCES; attribution to its `0000` iteration is derived
from this source chain because the original log did not label the iteration.
The positive regression now labels mode-specific failures.

Failed-stage cleanup uses its existing separate bounded no-follow walker. It
checks owner/type/link-count/special bits, concrete path/count/size bounds, and
link text length from `lstat`, then unlinks only known entries under the owned
bundle. It never needs to read or resolve a target, so unreadable, dangling and
external links can be removed without touching their targets. This cleanup path
cannot admit a bundle for identity, copying, installation or execution.

## Failed trial recovery and process cleanup

The supervisor retains the actual spawned `Child` and observes its exit with
`waitid(WNOWAIT)` before any anchored process-group signal. It never caches a
numeric PID/PGID for use after reaping that child. The generic stop operation
still rejects `EPERM`; readiness paths propagate the error, while `Drop` remains
best effort. Neither a successful group signal nor an exited leader establishes
that every descendant has stopped.

Darwin's group signal implementation excludes zombies and returns `EPERM` when
no eligible target is found. The same error can mean that live group members
are not signalable. For an already observed exited **pre-health trial** only,
that error now permits reaping the exact owned leader and reports an explicit
group-cleanup-unproven failure. Live or unobserved leaders and other errors do
not receive this recovery exception. There is no `ps` snapshot, orphan PID
signal, permission downgrade or blanket `EPERM` suppression.

Rollback is a separate operation: reacquire the installed app's lifetime lock,
reread the durable journal, and require the exact protocol, nonce, candidate,
old/new tree identities and current candidate bundle. An `AwaitingHealth` trial
may then restore the previous app; a matching `Committed` health/store boundary
always preserves the new app. A surviving lock holder or any identity mismatch
blocks recovery. Startup rejects a late trial with a removed or mismatched
journal, and the original pre-store health check again validates the nonce,
phase, embedded/bundle identity and complete new tree before opening stores.

Restoring and relaunching the old app does **not** resolve uncertain group
cleanup: the supervisor still returns `RecoveryRequired` in that case. Actual
descendant quiescence, normal UI shutdown and the old parent process's exit
remain separate installed acceptance gates. Installation activation stays false;
source regressions and this recovery path do not establish those gates.

### Actual old-process exit before replacement

The ordinary update UI already requests Slint event-loop quit after confirmed
helper readiness. Main then cancels its UI owners, drains update/call/incoming
work, requests attachment playback release, flushes drafts and awaits account
sync/token/readiness shutdown. The runtime and UI/store owners are dropped
before the root startup session releases its lifetime lock and install lease.
This ordering stays in the real main path; a synthetic lease drop is not a
substitute for process exit.

The Mac helper now registers `EVFILT_PROC`/`NOTE_EXIT` on its actual direct
parent before publishing readiness. The launching app supplies its own PID;
the helper rejects init/reparenting, a noncanonical PID, and any disagreement
with `getppid` both before and after registration. It retains the owned kernel
queue and per-transaction opaque cookie until the exit event is received.
An immediate successful registration receipt is not an exit event.

Only the exact registration's `NOTE_EXIT` with its expected process, filter,
cookie and termination flags may complete the bounded wait. Registration
errors, `EV_ERROR`, unexpected or lost events and timeout fail closed. There
is no `kill(0)` check, cached-PID signal, process-name lookup or process snapshot.
After the kernel exit event, the helper independently acquires the installed
app lock and revalidates the full prepared transaction before replacement.
Releasing the lock just before main returns can no longer authorize apply
while the original process is still alive.

The parent-exit boundary does not establish graceful native cleanup. Main still
logs call cleanup errors. Attachment playback now retains an owner that joins
the actual download/decoder/output worker, including its thread-local teardown,
before reporting release. A bounded wait retains pending or failed ownership;
its typed error is returned after draft flushing and account-manager shutdown,
so an unconfirmed attachment release cannot produce a successful main result.
That application-worker join proves return of the public stream destructor,
while backend monitor joining, native stop/dispose success, device quiescence
and actual normal AppWindow shutdown remain independent open gates. The original
synthetic installed driver kept its old actor alive as a monitor and could not satisfy
this new exit requirement. The composed route now retains that actor's actual
helper Child through a nonce-bound external kernel-observer acknowledgment,
then allows the synthetic parent to really exit. Both the production parent's
NOTE_EXIT and external helper exit/status must be observed, with no PID signal
fallback. Forced after-apply interruption uses compile-only `_exit(86)` before
any trial child and must be independently observed before startup recovery.
These synthetic process observations do not establish normal AppWindow shutdown.
Fresh native installed acceptance of the composed candidate remains mandatory.
Installation activation remains false.

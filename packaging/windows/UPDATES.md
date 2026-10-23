# Windows transaction draft

Installation activation is **disabled**. These sources implement the producer,
installed identity, native handoff and recovery boundary; they are not evidence
of installed upgrade acceptance or authorization to publish installers.

## One supported installation

The initial/manual package remains NSIS, current user only. The ordinary Windows
package command builds the existing Rust target once with
`MORPHEUS_PACKAGE_FORMAT=nsis-payload-v1`. It reads the native, immutable
`.mrphupd` PE section, generates `morpheus-install.json`, and includes that marker
as an explicit installed resource. The template and exact resource contract are
pinned by `update-contract.json`. Legacy packages and loose executables remain
manual-only.

The adapted template comes from Cargo Packager 0.11.8's `installer.nsi`:

- Crate SHA-256: `9aa0f5056c4191007702931dce47054c98fe78cbbebbaff8c7c39d5ebeed123d`.
- Original template SHA-256: `3ff5fde09ae24dfc031ab91f939b290f394e2c6d3549f2c43c37414716a47b7a`.
- License: Apache-2.0 OR MIT; original copyrights are retained in the template.

The initial installer only accepts a fresh, unregistered, nonexistent destination
on a fixed drive with existing parents and no reparse points. It checks the exact
product uninstall key in both registry views and hives and refuses existing
registrations without reading or executing their values. It neither enumerates
other products nor runs a legacy uninstaller, MSI command or pre-install hook.
Language selection remains in memory without registry persistence callbacks.
The obsolete manufacturer-key default is not used as a destination or admission
signal, so an ordinary supported uninstall followed by a fresh install works.

The changes replace process-name killing with the same share-denying installed
path lock held by the app and updater helper. All installation file writes occur
after fresh-destination admission, lock acquisition and a second admission check.
Install/uninstall refuses a pending journal. The template cannot select per-machine installation and contains no
profile-deletion UI or directives. Existing verified NSIS tools and plugins
remain unchanged. App launching releases the installer lock first.

## Signed data payload

The updater format is `windows-payload-v1`, distinct from `nsis`. Its bytes are
the magic `MORPHEUS-WINDOWS-PAYLOAD-V1\n`, a little-endian u32 marker length, the
strict JSON marker, and the five file bodies in ledger order. The whole envelope
is limited to 512 MiB; the marker is limited to 16 KiB. It contains no local
paths, registry snapshot, user identity or installer executable. Use the official
signer on the final envelope separately; the producer does not sign or upload.

The common identity has exactly `protocol`, `product`, `version`, `arch` and
`persistence`. The signed manifest binds each resource's exact path, length and
SHA-256. Native parsing additionally requires one bounded, read-only,
nondiscardable identity section in an x64 GUI PE. The pinned vendor updater's
generic executable-installer method remains blocked. Discovery, download,
staging and restart preparation retain official signature checks over the exact
retained staged bytes.

The payload changes the app, theme, three license resources and marker. The
original NSIS uninstaller remains valid because this contract fixes its complete
resource, shortcut, registration and scope behavior. Its **actual bytes** are
hashed separately in the private journal and retained in the complete backup.
Changing that contract, resources or installer customization requires a new
reviewed manual installation; a version-only claim is insufficient.

## Handoff and recovery

The app acquires the installed-path lock before stores and retains it through
shutdown. Preparation also retains the shared call/update exclusion lease. It
validates the complete owned file ledger, private current-user/default-token-owner
ACLs, an explicit `GetDriveTypeW` fixed-volume result bound to the retained opened
path's canonical drive root, no reparse points/hardlinks, and the exact HKCU registration;
an HKLM installation is unsupported. Registry children are opened relative to
retained handles without following registry symbolic links.

Before `ApplyIntent`, all original payload files, marker and actual uninstaller
are durably copied into the owned sidecar workspace. The copied current Rust app
acts as the supervisor, validates its exact hash and journal, and acknowledges
readiness after acquiring a real parent process handle bound to its creation
time. It waits for that process to exit, rather than a PID poll, process-name
kill, detached shell or forced owner-app termination.

Replacement uses the fixed file ledger with the main executable last, retaining
the installed directory so a normal startup can recover interrupted file steps.
Recovery accepts only each exact old/new hash, recognizes a leftover replacement
temp against every journal-bound payload/marker length and hash (including a later
EXE temp before an earlier marker rollback), restores the old complete payload
before commit, and conditionally restores both registry version and size without
overwriting unrelated values. The original uninstall registration/path is never
redirected. New UI health commits before any store opens. A committed cohort
cannot roll back, including a timeout racing health acknowledgement. Committed
startup recovery retries the checked journal flush before cleanup or stores;
a timeout that already stopped its own child relaunches the validated committed
cohort after releasing the installed-path lock.

A normal startup that finds an unfinished applying/health/rollback journal opens
no stores and hands recovery to the exact copied supervisor, then exits. The
copied process waits for that actual parent process to exit before it may restore
the installed executable; startup never overwrites its own mapped image. A strict
private recovery request binds the full immutable journal snapshot, phase, nonce
and parent creation stamp. Readiness additionally binds the exact request bytes
and the real owned child's process ID and creation stamp. The helper rechecks
the unchanged request and journal under the installed-path lock before recovery.

One nonce-specific **Global named mutex** is used only as a process-lifetime
object during spawn/readiness. The installed-path file lock and real process
handles remain the mutation and exit boundaries. The extra lifetime object is
necessary to distinguish a dead request from a spawned helper that has not yet
reached readiness: parent creates it with a current-user/System-only DACL and
rejects any preexisting object; the helper can only open it. A late helper cannot
recreate an object lost when the parent died. Handles are noninheritable and
retained through readiness/recovery. There is no thread ownership, mutex wait,
release, local-session fallback, elevation or privilege change. The 256-bit nonce comes from Windows' system-preferred `BCryptGenRandom`
cryptographic RNG, with no seed or weak fallback. Its validated hexadecimal form
prevents prefix/alias collisions and fixed-name cross-user squatting. Stale request cleanup
requires the global object and both creation-bound processes to be absent; unknown
request/readiness bytes are retained. Global mutexes span login sessions; Microsoft's
`SeCreateGlobalPrivilege` requirement applies to global file mappings and symbolic
links, rather than mutexes. These are source contracts awaiting Windows acceptance:
[object namespaces](https://learn.microsoft.com/en-us/windows/win32/termserv/kernel-object-namespaces),
[CreateMutexW](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-createmutexw),
[OpenMutexW](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-openmutexw),
[GetDriveTypeW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getdrivetypew),
[BCryptGenRandom](https://learn.microsoft.com/en-us/windows/win32/api/bcrypt/nf-bcrypt-bcryptgenrandom).

Cleanup
deletes only hash-bound known files and tolerates already removed files after a
terminal cleanup interruption; unknown files are retained and fail closed.

## Required acceptance

Before changing the explicit false activation constant, independently review
the complete composed source and run fresh standard Windows hosted checks using
synthetic data only. Required installed checks include actual NSIS install,
package marker and registration, ordinary launch and uninstall; signed upgrade;
bad signature, damaged stage, wrong cohort and unknown-file rejection; reparse,
hardlink and foreign-writer rejection; app/helper/installer mutual exclusion;
old-process timeout; copied-recovery parent death before/after readiness, stale
request retry and cross-login-session helper exclusion; fixed/network/removable
volume admission; failed helper/child startup; interruption at each file,
journal and partial registry boundary; failed health rollback; pre-store commit;
subsequent launch and cleanup; and no account/profile/vault crossover. Storage
durability, Narrator, real accounts/devices, signing and public release gates
remain separate. No local application execution is implied by this draft.

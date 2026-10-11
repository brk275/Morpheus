# Verification log

Morpheus is pre-alpha software with no approved installer release. This log records checks actually completed;
the [release matrix](FEATURE_MATRIX.md) remains the beta gate.

## Beta gate implementation batch, 2026-10-10

Candidate `e411b0e` added delegated OAuth/OIDC and cancellation/refresh cleanup,
signed-update staging with installation disabled, remote-video track replacement
handling, installed-package smoke checks, and real Linux Matrix/RTC integration
on disposable services. Review fixed pending-store creation, partial OAuth token
revocation, a refresh-subscription race, and a stale video conversion that could
repaint after a track stopped. The Docker RTC fixture excludes the container's
unreachable loopback ICE socket while preserving host-loopback endpoints.

Before hosted validation, 39 Python helper tests, Rust formatting, Actionlint,
and whitespace checks passed locally. No native build was run locally for this
batch. [Run 38100413791](https://github.com/brk275/Morpheus/actions/runs/38100413791)
tested `e411b0ebdf6200cf68c3c4708f2b33bbf4c17596` on all four standard public
runners. Security/scope passed. Every native job reached the Matrix tests:
30 passed and the new OAuth fixture test failed. Linux, Windows, and Intel Mac
rejected incomplete fixture provider metadata; Apple Silicon exposed an
accepted-socket nonblocking inheritance race and selected the fixture's legacy
fallback. Later native build and integration steps were skipped, so this run
does not verify the desktop candidate. Its four native logs passed credential
scanning. No installers were built or uploaded.

The follow-up fixes the fixture's required response modes and accepted socket
handling without relaxing production OAuth validation. Delayed/split requests,
malformed successful discovery responses, and a fresh-device RTC late join now
have explicit regressions awaiting hosted execution. Independent desktop checks
can finish even when model tests fail; the final job still fails in that case.

The follow-up also adds a Wayland screen-capture adapter through the system
portal and PipeWire, with cancellable source selection and no source fallback.
A disposable Sway/wlr portal acceptance test checks actual selected-output
pixels, live changes, source removal, denial, cancellation, and cleanup of the
owned process groups. This virtual display check awaits Linux CI and cannot
replace GNOME/KDE window selection or physical desktop acceptance.

The Linux [AppImage transaction](APPIMAGE_UPDATES.md) now provides durable
replacement/recovery before account stores open, with cancellation and shared
call/update exclusion. Its tests use synthetic files; real signed installed
updates are still required. Package QA can select one Linux format and records
only that format as evidence. A missing Debian project contact fails preflight.

Before this follow-up's hosted validation, 61 Python helper tests passed
locally; three Linux process tests were skipped on macOS. Rust formatting,
locked offline dependency metadata, Actionlint, package configuration and
whitespace checks passed. Full-history scanning found no unresolved credentials;
the working-tree scan matched only the documented public upstream test JWT,
whose hash matches the reviewed fixture.

[Run 38102279481](https://github.com/brk275/Morpheus/actions/runs/38102279481)
tested follow-up `ec8e7f55ca20816d6ade41b60cac1ec95c3ffec5` on all four free
standard public runners. Security/scope and both Mac jobs passed. Each Mac ran
32 Matrix, 50 desktop, and 31 native RTC/device tests, plus configuration,
protocol, and vendor security suites. Opt-in integration wrappers without
fixture credentials are not counted as live acceptance.

Windows built the native desktop and passed its 50 desktop and 31 native
RTC/device tests, but one of 32 Matrix tests failed: cancellation left an
unfinished browser-login store behind. Its final outcome gate rejected the
candidate despite the model step's `continue-on-error` presentation. The fix
keeps the cleanup marker locked and present until the database payload can be
removed, with regressions for a real Windows delete-denied handle, interrupted
deletion, unknown files, and symlinks. The next run below verifies this fix.

Linux passed 32 Matrix, 65 desktop, and 35 native RTC/device tests, its native
build, and Xvfb startup. The first real Wayland portal open failed before frames
arrived. Later vendor and live Matrix/RTC steps were skipped. Source review found
that the stock wlr backend requires a DMA-BUF format that the fixture's Sway
software renderer does not supply. The [revised fixture](WAYLAND_FIXTURE.md)
builds a pinned backend with a small, isolated SHM compatibility adaptation;
it still requires actual selected-source frames and cancellation checks to pass.
Neither the failed run nor this unverified adaptation establishes stock desktop
compatibility. CI now collects the
independent Matrix/OAuth/RTC results after a portal failure, but still fails its
final gate when either the model tests or Wayland acceptance fail.

A new [delegated OAuth fixture](OAUTH_ACCEPTANCE.md) uses real MAS, Synapse,
PostgreSQL, and an OS vault. It checks login, cancellation, token rotation,
restart, automatic refresh after expiry, and logout in separate processes.
Its bounded HTTP form driver tests the browser protocol; actual OS-browser
handoff and callback-listener acceptance remain separate. This fixture has not
yet passed a hosted live run.

Before the next hosted run, the correction batch passed 97 Python helper tests
locally; three Linux process tests were skipped on macOS. Rust formatting,
Actionlint, and whitespace checks passed. Independent reviews covered pending
store deletion/registry sequencing, OAuth fixture isolation and diagnostics, and
the pinned Wayland backend adaptation. The Wayland harness now rejects a
zero-test success and requires its exact named capture test to pass. No native
build or service was started locally for this batch.

[Run 38104445127](https://github.com/brk275/Morpheus/actions/runs/38104445127)
tested `10f472e4266ffdb57744685b6faa19fabab8c335` on all four standard public
runners. Security/scope, Windows, and both Mac jobs passed. Each of these native
jobs passed 37 Matrix, 50 desktop, and 31 native RTC/device tests, plus the
configuration, protocol, and vendor suites. The Windows cancellation cleanup
and fresh-registry recheck regressions passed. Live opt-in wrappers remain
excluded from this evidence. All four native job logs passed credential scans.

Linux passed 37 Matrix, 65 desktop, and 36 native RTC/device tests, all vendor
suites, its native build, and Xvfb startup. The adapted Wayland backend received
CreateSession, SelectSources, and Start and announced an SHM buffer format;
the capture test still failed without an identifiable test phase or panic in
the sanitized evidence. No native signal or memory-lifetime cause is confirmed.
The independent live Matrix harness then failed during `local_matrix_env-up`,
before chat, OAuth, or RTC acceptance could run. The old log did not identify
which fixture operation failed. The candidate failed its final gates.

The next diagnostic batch records fixed Wayland test stages in a private file
that survives a libtest abort, bounded child exit/signal facts, and known panic
categories. The Matrix fixture reports fixed stages and failure categories,
plus numeric Docker/HTTP/OS statuses. Arbitrary service output, credentials,
and response bodies remain private. Synapse is now pinned to the same published
immutable image digest as the OAuth fixture. These changes improve failure
evidence; they do not establish that either Linux failure is fixed.

The next batch also adds four editable shortcut actions, a keyboard room picker
limited to the current Home or Space, portable binding validation, and shared
settings/TOML reload behavior. Independent review caught and corrected a
Shift+digit binding that Slint would deliver as a different logical character.
Native shortcut tests and actual keyboard/screen-reader acceptance are pending.

The [stock Element Call fixture](ELEMENT_CALL_ACCEPTANCE.md) pins an unmodified
official RC and requires the two live rosters, distinct decoded video in both
directions, browser-to-native audio, and departure. It explicitly tests a
pre-synced Sticky2025 baseline. No result is claimed yet, and it does not verify
the production Current default or the previously failing stable peer.

Before this batch's hosted validation, 158 Python checks passed and five
Linux/hosted process checks were skipped locally. Rust formatting, Actionlint,
JavaScript syntax, Python compilation, and whitespace checks passed. Review
fixed terminal-exit handling and owned-child cleanup in the Element harness.
A mandatory hosted regression must prove that killing its guarded namespace
wrapper also removes detached descendants before any real peer is launched.
The final namespace teardown and native/UI checks remain unverified locally.
During development, one cached Slint build-script invocation generated Rust
source in an automatically deleted temporary directory; it did not run Cargo,
a native compiler/linker, native tests, the app, or Matrix/RTC services. Later
UI edits still require the hosted compiler and keyboard acceptance.

Live Linux protocol tests and installed-package checks remain pending. Prior
candidate results below do not verify these changed inputs. OIDC provider/browser
acceptance, physical capture, screen readers, signed distribution, and updater
replacement/recovery remain open; this batch does not establish beta readiness.
Apple Developer and Windows signing credentials are not yet configured.

## Explicit update controls and shortcut compilation fix, 2026-10-10

[Run 38106808405](https://github.com/brk275/Morpheus/actions/runs/38106808405)
tested `b1d4631d6b3f5f701f28411eb5771235d77ad8c6` on all four standard public
runners. Each passed 12 configuration, 37 Matrix, and eight RTC protocol model
tests. Every desktop check then failed because the new shortcut module omitted
the `slint::ComponentHandle` trait import. Native builds, desktop/device tests,
and Linux live acceptance were skipped. All four native logs passed credential
scanning. The missing import is corrected in this follow-up; the Linux fixture
failures from the earlier run remain unresolved until their checks execute.

The follow-up connects Settings to explicit signed-update Check, Download,
Discard, and Restart actions. It keeps blocking transfers/preparation off the
UI thread, reads current status when UI notifications arrive, and retains the
startup transaction until application workers and the runtime stop. Missing
compiled feed/key or an unsupported installation disables the controls. No
public feed, private signing key, or runtime trust override is added.

Independent review fixed two lifecycle races: closing the window stops
notifications immediately and drains call cleanup alongside updater work;
queued account-view clearing cannot release an active recovery reservation.
A newly returned recovery key must be acknowledged before restart can reserve
the security gate. Nine controller tests and two security regressions were
added and await hosted execution. Rust formatting, Python helper tests,
Actionlint, whitespace checks, and a fresh staged-source secret scan passed
locally. The only scanner match is the previously reviewed public upstream
JWT fixture; no unresolved credential finding remains. No local native builds,
source generation, native tests, or Matrix/RTC services ran for this follow-up.

Manual package workflow edits no longer trigger an unrelated automatic native
matrix; package/install verification still requires an explicit dispatch for
the affected platforms. This source follow-up does not satisfy installed
signed-update, physical-device, screen-reader, or beta release acceptance.

### Hosted result and focused follow-up

[Run 38107865401](https://github.com/brk275/Morpheus/actions/runs/38107865401)
tested `95f61dab0bbc0b4c33a159a9b449170a63afef2a` on all four standard public
runners. Security/scope, Windows, Apple Silicon and Intel Mac passed. Each
native target passed 12 configuration, 37 Matrix and eight RTC protocol tests.
Windows and both Macs passed 65 desktop and 31 native RTC/device tests; Linux
passed 80 desktop and 36 native RTC/device tests. The native app built on every
target, Linux Xvfb startup passed, and all vendor security suites passed. This
includes the updater controller/security and shortcut regressions. The opt-in
test wrappers without configured fixtures do not count as live acceptance.

Linux failed its live gates. Its capture test exited via SIGSEGV (signal 11)
at `opening_display_1`, before recording a first frame. The portal backend had
received CreateSession, SelectSources and Start and announced SHM; all five
desktop services remained running. These facts narrow the failure but do not
by themselves identify the crashing function. The independent Matrix fixture
failed at `ready_a` with OS error 104, a connection reset, before live chat,
OAuth or RTC tests. Its disposable credential-vault checks and service cleanup
passed. All four native logs passed credential scans. No installers were built
or uploaded; the overall run failed.

The readiness follow-up retries an unwrapped `ConnectionResetError` within the
existing 120-second deadline in both Matrix and OAuth startup waits. Empty or
malformed responses also wait between attempts, and per-request timeouts cannot
exceed the remaining deadline. Exact OAuth issuer and redirect/proxy policies
are unchanged. Four new mocked regressions passed, including persistent reset
timeout and propagation of unrelated OS errors; the 47 existing OAuth/Matrix
helper tests and six diagnostic tests also passed. Independent source review
cleared the change. Actual service readiness still requires the next Linux run.

The next source batch also adds an explicit per-account/room Element Call
Matrix 2.0 choice and separately selected installed AppImage update and keyboard
harnesses. Their source review and offline tests are recorded in their focused
documents; none has yet passed installed or live interoperability acceptance.

Read-only inspection of the pinned WebRTC archive found weak PipeWire stubs
that can collide with the system API. The [isolation patch](../vendor/webrtc-sys/MORPHEUS_PATCH.md)
verifies the exact archive, renames all stub definitions and references in a
build copy, and removes optional LLVM address-significance metadata whose
indexes cannot safely be assumed unchanged. The Wayland fixture must verify
all 33 actual linked addresses before initialization, then still pass real
capture. This is a reviewed correction candidate, not proof of the crash's
cause or resolution. Separately, corrupted frame chunks are dropped before
pixel access, and namespace containment replaces the fixture's stale-PID
cleanup fallback.

Before hosted verification, the combined follow-up passed 207 Python checks;
five platform-specific checks were skipped on macOS. Rust formatting, locked
offline Cargo metadata, Actionlint, JavaScript syntax, Python compilation,
package configuration and whitespace checks for Morpheus edits passed. The
new vendored WebRTC source retains upstream whitespace unchanged; comparison
with the pinned registry source confirms only the documented patch files differ.
Independent reviews
covered protocol selection, package/update harnesses, readiness, capture
cleanup and archive isolation. No local native build, archive transformation,
link, source generation, native test or Matrix/RTC service ran for this batch.

### Linux capture and fixture follow-up, 2026-10-11

[Run 38110401901](https://github.com/brk275/Morpheus/actions/runs/38110401901)
tested `16fbd5b1e66b88096838e9955899a48cb5b36e08` on the four standard public
runners. Linux passed 14 configuration, 37 Matrix, eight protocol, 80 desktop,
41 native RTC/device tests, all four vendor suites, the native app build and
Xvfb startup. Its linked-address check confirmed all 33 PipeWire bindings use
the system library. Capture advanced past the earlier initialization crash,
then failed awaiting its first frame after the portal backend exited.
This is progress in diagnosis, not capture acceptance.

Windows, Apple Silicon and Intel Mac passed their native jobs: each ran 14 configuration,
37 Matrix, eight protocol, 65 desktop and 33 native RTC/device tests, plus the
vendor suites. Opt-in wrappers without fixture credentials do not establish
live acceptance. All four native logs passed credential scanning. Cache saves
completed on the three successful targets before the next candidate was pushed.
The overall run failed its Linux gates. No installers were built or uploaded.

The independent Matrix fixture now started both homeservers successfully and
passed its disposable vault checks. RTC setup failed before live acceptance;
the retained diagnostics cannot identify that operation. The follow-up retries
direct connection resets within the existing RTC readiness deadline and adds
fixed phase/error categories, without publishing exception or server bodies.
Read-only checks found no mismatch in the pinned RTC image manifests or the
inspected upstream configuration fields. Chat, draft, recovery and OAuth checks
now precede RTC startup; a failure still rejects the candidate and triggers
cleanup.

The [capture correction](WAYLAND_FIXTURE.md#spa-format-enumeration-correction)
adds BGRx to the accepted SPA alternatives as well as the default slot. A
serialized-offer regression awaits the next Linux native run. This verified
protocol defect is consistent with the backend failure, but its exact cause
is still unconfirmed. Native CI can now save dependency caches after a failed
acceptance check; the existing trusted-main-only write guard remains in place.

Independent source review cleared both fixes and the fixture sequencing.
The combined follow-up passed 233 portable Python checks with five platform
skips, Rust formatting, Actionlint, Python compilation, JavaScript syntax,
package configuration and whitespace checks. No local native build, source
generation, native test, archive transformation or Matrix/RTC service ran.

A subsequent Linux harness correction makes cleanup failures fatal, preserves
the original test failure or interrupt, and still attempts both service stops
when summary writing fails. Checked Docker inventories must prove fixture
absence before the harness takes ownership and after cleanup. An unavailable
daemon cannot count as an absent fixture. Retained test volumes and private
fixture data remain on the ephemeral runner; this check does not claim they
were wiped. All 33 focused Python harness checks passed with mocked lifecycle
and Docker operations; actual hosted execution of this correction is pending.

### Thread drafts and Windows capture follow-up

Per-thread drafts now share the encrypted SDK store and the existing flush
worker, with separate account/room/root keys. Cached restoration is immediate;
late disk results cannot overwrite edits or repopulate a logged-out account.
Review caught that SDK queue acceptance is not server acknowledgement: the
thread send path now observes the exact transaction, clears only an unchanged
submitted revision on acknowledgement, and retains drafts on paused, cancelled
or unknown outcomes. A per-thread session guard prevents duplicate submissions
while still allowing edits and other threads. Unknown outcomes require a restart
and checking the thread before resending. Cross-restart queue reconciliation
and retry/cancel controls remain open.

Four draft-cache regressions and extensions to the existing encrypted draft
restart/isolation and live thread-send fixtures are prepared. They have not yet
run natively for this follow-up. The separately selected
[Windows capture fixture](WINDOWS_CAPTURE_FIXTURE.md) exercises an owned
synthetic window and covered monitor through the production capture API, with
Job-object process containment and strict fixed-fact evidence. Its actual
WinForms/GDI execution also requires the next hosted candidate.

The combined follow-up passed 222 portable Python checks; five platform-specific
checks were skipped on macOS. Rust formatting, Actionlint, Python compilation,
JavaScript syntax, package configuration and whitespace checks passed.
Independent source reviews cleared both changes. No local native build, native
test, source generation, archive transformation or Matrix/RTC service ran.

## Public source publication, 2026-10-10

- Published `brk275/Morpheus` as public pre-alpha source at
  `bc1614bbea5e8513c04c4cbbe4387cfb80fae54e`. Anonymous GitHub API access
  confirmed public visibility. Commit identities remain pseudonymous.
- The private cleanup push [run 38092927224](https://github.com/brk275/Morpheus/actions/runs/38092927224)
  skipped all hosted jobs. It performed no native build.
- Verified secret scanning and push protection enabled, private vulnerability
  reporting enabled, approval required for all outside contributors' workflows,
  and read-only default workflow permissions. GitHub reported no open
  secret-scanning alerts at publication; scans do not guarantee future safety.
- Standard hosted runners provide free public-repository build compute, not
  account credits ([GitHub runner documentation](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)).
  Earlier charges, storage rules, and paid-runner restrictions remain separate.
  Billing caps were not changed.

### Complete native CI matrix

[Run 38092977119](https://github.com/brk275/Morpheus/actions/runs/38092977119)
checked source revision `bc1614bbea5e8513c04c4cbbe4387cfb80fae54e` on standard
public runners. All native build and test steps passed on the first attempt:

| Target | Runner | Result |
| --- | --- | --- |
| Linux x64 | `ubuntu-24.04` | Native build, tests, and Xvfb startup smoke passed |
| macOS Apple Silicon | `macos-15` | Native build and tests passed |
| macOS Intel | `macos-15-intel` | Native build and tests passed |
| Windows x64 | `windows-2025` | Native build and tests passed |

The workflow completed successfully. Actual Rust host targets were verified
for all four jobs; 23,893 native-job log lines passed credential scanning with
zero unresolved findings. Private evidence retains the completed job records.

Each target checked the default desktop configuration, built `native-calls`,
and tested desktop controllers, native device guards, and the vendored security
patches. Real homeserver tests remain explicitly invoked local checks; hosted
jobs had no Matrix credentials. The standalone theme job was intentionally
skipped because the full native jobs already test configuration.

This run did not build or upload installers. It is not installed-package,
physical-device, signing, accessibility, or interoperability acceptance.
Nonfatal upstream warnings remain, including `block` future-Rust compatibility,
macOS duplicate JPEG symbols/debug unwind size, and deprecated native APIs.
The pinned checkout action also reports its Node 20 declaration while GitHub
runs it on Node 24. These need dependency/toolchain follow-up before release.

## 2026-10-10 public-source preparation

Public development does not authorize installer distribution. The original
binary release gates, including real devices and all supported platforms,
remain open.

- Removed automatic QA-account bootstrap from the desktop app. Explicit
  disposable tests remain; fixture generators reject checkout destinations,
  preserve owner-only files, and reject symlink writes.
- Passed 53 `lru` unit tests, 32 LiveKit tests, 10 MatrixRTC transport tests,
  31 native RTC/device tests, and 37 desktop controller tests on Apple Silicon.
  The cancellation cases cover a queued permission grant, a late grant after
  cancellation, permission denial/timeouts, and a cancelled open before native
  enumeration. They do not establish physical camera behavior on every OS.
- Local listeners confirmed authenticated requests do not follow
  301/302/303/307/308 redirects. Invalid foci fail before OpenID acquisition;
  unsafe SFU and region URLs are rejected before credential use. Captured logs
  omit reflected dummy bearer/proxy credentials. Unsupported proxy schemes
  open no socket; HTTP CONNECT remains supported.
- Passed 25 Python tests for CI scope, platform packaging configuration,
  private fixture paths, the loopback relay, and stale fixture mounts. Rust
  formatting, Actionlint, Python compilation, JavaScript syntax, and package
  configuration checks passed without building installers.
- Fresh disposable accounts on two local homeservers passed login, encrypted
  room, and token-isolation checks. The native RTC fixture's discovery,
  OpenID-to-JWT exchange through its private relay, returned loopback URL, and
  SFU reachability checks passed. The fresh native two-device encrypted call
  test passed 1/1 in 19.41 seconds: synthetic audio, camera and screen tracks,
  signaling reconnect, participant departure, leave-triggered key rotation,
  and media after rejoining. This is loopback acceptance on Apple Silicon,
  not physical-device or Element Call interoperability acceptance.
- Fresh secret scans found no live credentials. The upstream LiveKit public
  JWT test vector and one historical property-name comparison are documented
  scanner false positives with narrowly scoped fingerprints.
- Preserved a private manifest of historical run/source/artifact identities,
  then deleted three old package runs and all ten unsigned installer artifacts.
  A subsequent GitHub API read reported zero remaining Actions artifacts and
  zero releases. Historical package results do not verify this candidate.
- Hosted jobs are gated while private. Public CI uses standard runners,
  read-only permissions, pinned actions, approval for outside contributors,
  trusted-main cache writes, and no installer uploads. The complete public
  native matrix is recorded separately after publication.

## 2026-10-09 local desktop and Matrix checks

- Built the macOS Apple Silicon desktop binary with `native-calls` and packaged
  an unsigned `.app` from it. The bundle has microphone, camera, and screen
  capture purpose strings. Signing, notarization, and updater behavior remain
  untested.
- Launched the native Slint app and inspected its sign-in, bottom-left account
  menu, light theme, and font-scale controls through macOS accessibility and
  screenshots. The menu listed the active account, another signed-in account,
  Add account, Manage accounts, and Log out.
- In the desktop preview, starting a call and choosing another account opened
  a leave-call confirmation. Escape cancelled it without switching; keyboard
  confirmation ended the preview call and switched accounts. The same guard is
  wired for native calls, but a connected native call has not passed this check.
- The settings UI changed font scale to 110% and back to 100%, switched density
  between Compact and Comfortable, and persisted the selected values in
  `config.toml`. Unread-first sorting is also wired through that config.
- Signed in disposable Alice on loopback Synapse A and Carol on independent
  loopback Synapse B. Each account showed only its own rooms. A message sent
  from Alice's composer appeared in the room and was confirmed through the
  server API. Carol did not see Alice's unsent draft; it returned when switching
  back to Alice. Reopening the app restored Carol as the selected account with
  only her room visible.
- One account switch briefly showed an empty Alice timeline after a local
  homeserver restart. A later switch restored the server-confirmed message.
  The strengthened acceptance test now confirms the cached feed retains a
  sent message across switches and manager restart. A separate presentation
  bug that could briefly show the prior account's messages when both accounts
  selected the same room ID was fixed by clearing on account identity change.
- Two local Synapse servers passed password login, room-membership, encrypted
  room-state, and cross-server token rejection checks. The first gated Rust
  `AccountManager` acceptance test passed: account isolation, switching,
  selected-account restore, and Alice-to-Bob encrypted message decryption
  before and after Bob restarted. The stronger test passed with cached-feed
  checks and with a forced classic-sync session alongside native sticky
  sliding sync. Both current Synapse fixtures advertise sliding sync, so the
  classic policy is forced explicitly for that adapter check.
- A second gated acceptance test used fresh disposable Alice/Bob identities and
  an encrypted room. New-device Alice could not read a historical message
  before recovery; after importing the backup recovery key, the same timeline
  decrypted it. This establishes the core key-backup recovery path. The native
  Settings view now displays device verification, recovery, and key-backup
  status for the active account, and wires recovery setup and import to that
  core. A new key appears in a one-time reveal/copy dialog with explicit save
  guidance; the input clears after submission or leaving Settings. A shared
  gate prevents queued account changes and normal window close from dropping
  the only returned key before acknowledgment. The Security panel also has an
  own-device SAS flow: request verification, wait for the other device, show
  emoji and decimal comparisons, explicitly acknowledge a human comparison,
  confirm, or cancel. It never confirms a comparison automatically. The
  native-call app suite passed 24/24 tests, including gate, close-response,
  and explicit comparison checks; the default app check and native binary
  build passed. Fresh-device recovery and two-device SAS verification through
  the actual desktop UI still need live acceptance.
- A gated two-Synapse room workflow passed private room and Space creation,
  invitation acceptance and decline, room-ID joining, and joining a registered
  room alias. Repeated runs initially hit Synapse's local invitation rate limit
  (`rc_invites.per_user`, HTTP 429); the loopback fixture now raises that limit.
  The app wires those room controls and protects each pending action against
  account changes. A newly created Space has an account-scoped temporary rail
  item while the SDK's room classification catches up. The Space UI path has
  passed a native compile, but not a live desktop click-through.
- After native UI QA found new private rooms were cleartext, creation now
  includes Megolm state in the initial server request. A fresh loopback test
  passed 1/1 in 8.57 seconds: an independent client read encryption state in
  four new private rooms and a new DM, with each private room checked before
  invitation. The formatted two-device test passed 1/1 in 10.29 seconds on a
  newly created room without a separate encryption step. Existing cleartext
  rooms and Spaces are not converted by this change.
- The gated encrypted-room messaging test passed reply, edit, reaction, and
  thread send/read through the Matrix core. Bob received the changes, and
  operations bound to Alice were rejected after switching to Carol. A packaged
  macOS app then clicked through Reply, Edit, Heart, and Thread in a disposable
  local Synapse room. The UI displayed reply context, the edited body, a heart
  count, and the thread reply. Direct Synapse reads confirmed `m.in_reply_to`,
  `m.replace`, `m.annotation`, and `m.thread` relations to the intended events.
  This checks the native UI to server path for one account on macOS; encrypted
  UI actions and other desktop platforms still need acceptance checks. Visual
  QA found missing glyphs for Edit and Thread; those actions now use drawn
  Slint paths, and the thread panel keeps its reply count and short reply rows
  directly below the root message in the rebuilt macOS preview.
- A gated two-Synapse draft test saved different room drafts for accounts on
  separate homeservers, restarted the account manager, restored both drafts,
  cleared one, and removed each account. A raw scan of its account-data files
  found neither unique draft body in plaintext. Focused draft tests covered
  keeping text through queue acceptance, restoring it on server send failure
  when unchanged, and preserving a newer edit. Pending transaction metadata
  is currently in memory: if Morpheus exits after queueing but before send
  confirmation, a message sent after restart can leave a stale encrypted draft.
- A stable-path, ad hoc signed macOS QA bundle stalled in the OS credential-vault
  read on two launches. A SecurityAgent process appeared, but no prompt was
  exposed through the available app UI. The 30-second vault-read timeout then
  displayed an explicit sign-in error and enabled offline preview. This QA
  bundle reused the same account registry and Keychain item as earlier preview
  bundles, but `codesign -dr -` showed different designated CDHashes for each
  ad hoc signature. An access-control approval for the new code identity is
  the likely reason SecurityAgent appeared; the prompt's visibility and the
  exact blocked OS operation remain unverified. Repeat restore with a fresh
  test profile, a stable real signing identity, and a relaunch of the same
  signed build before treating credential restore as verified. No Keychain
  entries were read, changed, or removed during this diagnosis.
- A macOS offline-preview keyboard pass reached Home, room rows, the
  bottom-left account switcher, message Reply, and Join/Leave call controls.
  Tab, Space, Return, and Escape were exercised for account switching and the
  leave-call dialog; Reply focused the labeled composer. A later source-synced
  build confirmed the account menu focuses its first visible account, forward
  Tab cycles within the account manager and room dialog, and Escape returns
  focus to the trigger. The automation delivered Shift+Tab without a Shift
  modifier, so reverse traversal requires a physical-keyboard check. Native
  screen-reader checks and Windows/Linux keyboard runs remain outstanding.
- A loopback MatrixRTC stack passed authenticated transport discovery and
  OpenID-to-LiveKit JWT exchange. Synapse advertises the sticky sliding-sync
  feature required by the native call path. The gated native call test reached
  LiveKit signaling after adding the upstream macOS `-ObjC` link workaround.
  Docker Desktop's loopback-published media ports offered no viable ICE pair.
  A native LiveKit instance bound to macOS loopback connected both disposable
  participants over UDP and populated both rosters. An initial encrypted-key
  import failed because the devices lacked verified cross-signing identities.
  A fresh isolated fixture bootstrapped each identity and signed its device.
  The initial gated test passed in 17.07 seconds: Bob imported Alice's media
  key, decoded nonzero synthetic microphone audio, left, and rejoined. An
  expanded fresh-fixture run passed in 18.27 seconds: Bob also subscribed to
  Alice's `Camera` and `ScreenShare` tracks and decoded the expected red/green
  I420 pattern at 640×360 from each. This checks local macOS two-device media
  transport inside an encrypted call. The screen track carried generated pixels;
  OS capture, Morpheus call-stage video rendering, automatic network reconnect,
  and Element Call interoperability were untested at that point; a later
  legacy-format browser-peer run passed, as recorded below. A separate native Slint
  proof displayed moving synthetic frames and rotation on macOS; it did not
  consume the received RTC track. See [the video feasibility note](VIDEO_FEASIBILITY.md) and
  [the RTC test runbook](../tests/LOCAL_MATRIX_RTC.md).
- A follow-up macOS capture proof used real display and application-window
  pixels. The capture adapter produced nonblank 640×360 I420 frames without
  saving image files. In two fresh disposable-device loopback MatrixRTC runs,
  Bob decoded a nonblank encrypted screen track from the selected display
  (20.03 seconds) and window (18.32 seconds), respectively; both runs also
  retained synthetic camera/audio and leave/rejoin checks. These tests used the
  development binary and a local call service. Packaged-app permission prompts,
  continuous capture controls, Morpheus call-stage rendering, Element Call
  screen-share interoperability, and Linux/Windows capture remain unverified.
  After tightening the decoded-frame check to visible rows, a fresh display
  call passed again in 19.83 seconds. During setup, a generated password
  beginning with `-` exposed an argument-parsing bug in the disposable
  Synapse registration script; it now uses `--password=value`, and the next
  fresh fixture registered successfully. All local services were stopped.

## 2026-10-10 focused follow-up checks

- `ConfigStore::save` now writes a unique temporary file beside `config.toml`,
  flushes it, and persists it over the destination. The focused configuration
  suite passed 5/5 tests, including saving over an existing config and reporting
  a malformed theme after a selected theme file was previously missing. This
  checks the replacement path; forced process termination during a write and
  Windows filesystem behavior have not been exercised at runtime.
- A gated two-Synapse public-directory test passed after the disposable fixture
  allowed public-room publication. Alice searched two rooms over two pages,
  resolved an alias, and Bob joined one through a routed `matrix.to` link.
  Switching to Carol on independent Synapse B rejected delayed Alice actions
  and returned only B's directory result. The isolated servers do not test
  federated directory or join behavior.
- A gated moderation test created a disposable room on Synapse A and passed
  rename/topic, default-power permission rejection, promotion to moderator
  level 50, rejection of a moderator's self-promotion to 100, demotion, and a
  second permission rejection. The creator redacted Bob's message; an
  authoritative event GET showed redacted content and both client timelines
  removed the original body. Kick and reinvite, ban and blocked rejoin, then
  unban and rejoin also passed. A follow-up run asserted the account-scoped
  member list showed Bob as invited, joined, and banned at the corresponding
  server states and rejected a mismatched account ID. It passed 1/1 in 20.99
  seconds and removed its test devices/stores. This verifies local core
  operations, not moderation UI, federation, or every room-version permission
  rule.
- An isolated macOS native-app profile signed in disposable Alice on loopback
  Synapse A. The discovery view rendered a newly published room's name, alias,
  topic, member count, and accessible Join control; clicking Join opened the
  room and composer. A routed `matrix.to` alias for a second disposable room
  also joined and opened its conversation. The temporary QA login bootstrap
  was removed from source, Alice logged out, both rooms were unpublished, and
  the protected app profile was deleted. Federation and other desktop
  platforms remain untested.
- A gated encrypted-message search test passed on loopback Synapse. Alice sent
  a unique message and found both a common-word match and its exact marker,
  with sender, room, timestamp, body, and event ID. After switching to Carol on
  independent Synapse B, Alice's query was rejected and Carol found no matching
  result. Cross-room search returned the same room and event, and an
  event-focused timeline loaded the message in its context. Alice's index
  returned the same event after manager restart. A raw
  scan of both account stores found no unique message body in plaintext. The
  search index is encrypted per account and indexes locally as events sync or
  paginate. The same gated test passed again with forced classic `/sync`.
  An added edit case then found the replacement's new text and mapped the hit
  to the original visible event ID, with the edited body in its event-focused
  context. This expanded test passed on both sync paths in 15.87 and 15.53
  seconds respectively.
  A native current-room query/results panel and selected-event context are
  wired with account/room generation guards. Its sanitizer unit test and the
  13/13 native-call app suite passed. In an isolated macOS app profile, Alice
  sent a new message, searched its exact marker, saw one result with
  sender/time/body, and opened context containing that event and its neighbor.
  The disposable app account was logged out, both QA profiles and residual
  vault entries were removed, and the temporary login bootstrap was removed
  from source and binary. All-room UI, large-history behavior, and other
  platforms remain untested. The QA screenshot is at
  `/tmp/morpheus-search-ui-qa.png`.
- The native Members panel now fetches the selected room's joined, invited,
  and banned entries with account/room guards, sanitized display text,
  explicit membership and power labels, loading/error/empty states, a Refresh
  action, and a virtualized list. The then-current native app suite passed 20/20
  tests; default app check and native-call binary build passed.
  An isolated macOS app signed in disposable Alice using a protected fixture
  read locally at runtime. In Alice's test room, the panel showed Alice as
  Creator and Bob as Invited; in the encrypted test room it showed Bob as a
  joined Member. Refresh repopulated the real server rows. A same-account
  background refresh closed the Add-account dialog during Carol sign-in.
  The dialog persistence fix passed unit and app compilation checks. A later
  isolated macOS app added Carol on the second homeserver and switched between
  Alice's and Carol's distinct room lists. Both were logged out and the app
  registry contained zero accounts. Bob's temporary invite was revoked; the
  earlier QA app, profile, and credential helper were removed.
- A fresh disposable Alice/Bob MatrixRTC test passed in 18.16 seconds after
  Alice wrote the `io.github.brk275.morpheus.voice` room-state marker and Bob
  read it. The first attempt exposed that native sliding sync did not include
  this custom state event; the reader now fetches the state directly from the
  homeserver. The same passing run checked Morpheus's participant-roster
  wrapper, encrypted synthetic microphone, camera, and screen tracks, and
  leave/rejoin. It verifies the marker and roster core against local Synapse,
  not a voice-space settings or navigation UI.
- The native call stage now has a participant list and a remote video tile.
  Its worker selects a remote screen share ahead of a camera, bounds I420-to-RGB
  conversion to 2.07 million pixels with a 1920-pixel edge limit and about
  15 frames/s, and clears stale frames by
  call generation. The native-call app check and six focused app tests passed
  on macOS. A live RTC frame has not yet been viewed in the Morpheus call UI;
  this remains a separate acceptance step from the successful transport tests.
- A fresh isolated macOS app joined an **unencrypted** room call with a
  synthetic Bob peer. Its live roster showed Alice and Bob and retained the
  correct room title. A CoreAudio device open blocked the call worker before
  either roster or video could run; device opening now occurs on a detached,
  single-in-flight OS thread with a 30-second bound. The app then showed a
  view-only local-audio failure, kept the roster visible, and returned to idle
  immediately on Leave. The bottom-left account switcher guarded the active
  call. The camera tile did not receive a frame. A separate two-peer native
  probe kept Bob's camera publication alive and pushed 640×360 frames for 30
  seconds. Bob's own roster advertised `Camera, muted=false`, while Alice saw
  Bob with `streams=[]` and no camera `remote_track` in all 30 samples. The
  pinned RTC transport maps remote members to LiveKit identities using their
  sender device ID; current-format cleartext membership events provide no
  attributable device ID. Morpheus now rejects current-format calls in
  unencrypted rooms before opening or joining, with guidance to use an
  encrypted room. The same disposable Alice/Bob fixture passed the encrypted
  native audio/video test in 18.59 seconds, including decoded camera and
  screen-share I420 frames, leave, and rejoin. This does not yet validate
  Slint frame rendering or an encrypted GUI call.
  An app-owned Alice/Carol account switch across the two local homeservers
  passed afterward. The final native app suite passed 22/22 tests, including
  call generation and Security transition/close guards.
- A gated Alice/Bob encrypted attachment roundtrip passed twice on local
  Synapse. Alice queued a binary file, Bob downloaded and decrypted identical
  bytes, and the test checked the filename, MIME type, declared size, file
  kind, encrypted metadata, account isolation, and absence of a unique payload
  prefix in both account stores. The final run logged out its disposable
  devices and removed their temporary stores. The encrypted roundtrip passed
  again after Morpheus replaced the SDK's unbounded media convenience download
  with a streamed request that rejects each chunk above 25 MiB. Twelve focused
  Matrix core tests passed, including an oversized-stream rejection. Attachment
  controls, preview/open behavior, image/video/audio variants, an adversarial
  homeserver response, and macOS UI upload/download have not passed acceptance.
- A media hardening pass disabled automatic HTTP redirects, checked each
  authenticated-media 307/308 target before connecting, dropped the Matrix
  token after leaving the homeserver origin, and bounded redirect count,
  response streaming, and token-error parsing. The focused Matrix library
  suite passed 13/13 and the encrypted Alice/Bob attachment roundtrip passed
  again. Public HTTPS CDN redirects and an expired-token refresh still need
  live fixture checks. A further pass routes every off-origin CDN request
  through a no-proxy client with a custom reqwest DNS resolver. It rejects
  any answer set containing a non-public IP before reqwest connects to the
  checked socket addresses. The original homeserver retains normal proxy
  behavior; CDN redirects requiring a system proxy fail closed. Seven focused
  media unit checks passed, including public/private address classification,
  mixed-answer rejection, and a local HTTP connector refusing loopback. The
  disposable encrypted Alice/Bob attachment roundtrip passed once more. A live
  public CDN redirect and DNS-rebinding fixture have not yet been exercised.
- A follow-up code review found that the SDK retains requests after both
  recoverable and unrecoverable send errors. Morpheus now preserves the exact
  transaction and observes its eventual result; the selected account's room
  queues retry recoverable errors after 2–60 seconds. Re-selecting the active
  account also resumes paused room queues. A retained error no longer restores
  a draft or suggests a duplicate send. The focused Matrix suite passed 9/9,
  including a retained-then-sent event test. Automatic retry during a real
  network outage has not yet had a fault-injection acceptance run.
- Native attachment selection and save dialogs, a timeline attachment card,
  progress, cancellation, and same-transaction retry for a wedged upload are
  wired in the app. Save uses a user-chosen path and an atomic file replacement;
  the account and selected-room guards remain held through that write. App
  native-call check and 8/8 focused app tests passed. The gated encrypted
  Alice/Bob attachment test passed again with a receiver timeline filename
  assertion; disposable Synapse servers were stopped afterward. Live GUI
  click-through and image/video/audio previews remain open.
- The RTC core now exposes call-bound microphone mute and local deafen
  controls and rejects stale publication/control handles after leave and
  rejoin. Its device-audio suite passed 24/24. The app now has call-bound
  mute/deafen controls with generation guards; live device and cross-platform
  behavior remain open.
- The Element Call v0.26.1 browser peer passed the opt-in legacy `RoomState`
  compatibility test against a fresh local encrypted room. Morpheus saw the
  browser join, decoded microphone PCM and red/green I420 video, and observed
  departure; the final fresh-fixture run passed in 16.48 seconds. The browser
  used generated tracks and a disposable, cross-signed device. Native
  Alice/Bob current-mode media, roster, leave, and rejoin passed again in
  19.04 seconds with the pinned core's lowercase `m.call#room` default. An
  uppercase `m.call#ROOM` current-mode native regression failed roster
  discovery, so it is not the default. Element Call's current `matrix_2_0`
  mode did not pass the local fixture; see the [RTC runbook](../tests/LOCAL_MATRIX_RTC.md).
- A fresh stock Element Call v0.26.1 `matrix_2_0`/`Sticky2025` attempt
  received and decrypted both membership events, and its SDK calculated two
  members from them. Its live call-view and session-manager arrays remained
  empty. The sticky update callback had fired while both events still had
  type `m.room.encrypted`; after decryption no further update refreshed the
  live arrays. Morpheus observed the browser participant, but camera
  subscription timed out at 49.25 seconds, so the interop test failed. A
  separate explicit `refreshRoom` diagnostic had passed media transport;
  that intervention is outside stock Element Call behavior. See the
  [RTC runbook](../tests/LOCAL_MATRIX_RTC.md).

- A separate fresh account on loopback Synapse B, which advertises no
  MatrixRTC LiveKit transport, created an encrypted private room. An opt-in
  native test passed 1/1 in 4.74 seconds: discovery, slot opening, and joining
  each returned `NoLiveKitTransport`. Complete server room-state reads showed
  no `m.rtc.slot` or `org.matrix.msc4143.rtc.slot` before or after the call
  attempts. The same account then sent an encrypted chat message; its queue
  completed, the SDK timeline showed the body, and Synapse stored ciphertext.
  This checks the chat-preserving service gate on B, not other homeservers or
  the desktop error view. See the [RTC runbook](../tests/LOCAL_MATRIX_RTC.md).

## 2026-10-10 local administration and recovery checks

- A new ad hoc signed macOS `.app` and isolated profile signed into an existing
  disposable account. Before recovery it showed Device Unverified and Recovery
  Incomplete. Importing that account's protected recovery key in the native
  Security UI completed with Device Verified, Recovery Enabled, and Key backup
  Enabled. A second fresh unverified app requested own-device SAS; the
  recovered device accepted, the requester started, and the responder
  accepted the SAS method. Both displayed identical emoji and three decimal
  numbers. After a private human comparison, each app explicitly confirmed;
  both devices reached Verified. The requester briefly displayed “Could not
  check verification status” after confirmation because the completed request
  was pruned; manual Refresh showed Verified. The completion UI now checks
  durable device trust, with a focused test passing; a live retest of this fix
  remains open. No recovery key, emoji, number, or account secret is recorded
  here. This was a macOS live UI check; other platforms remain open.
- A focused loopback Synapse moderation test passed (1 test, 0 failures;
  22.80-second run after a 9.35-second compile). It
  exercised room rename/topic, server rejection of an unauthorized rename,
  power promotion/demotion, redaction of another member's message, invite,
  kick/reinvite, ban/blocked join, and unban/rejoin. The account-bound
  invite/kick/ban/unban and rename/topic/redaction wrappers rejected a
  mismatched selected account.
- In a separate disposable macOS Slint profile, a room creator invited a
  guest, raised the guest's exact Matrix power level to 50, kicked and
  reinvited the guest, banned the guest, then unbanned them. The UI reported
  each action and direct server reads confirmed the power and membership
  changes. The destructive actions required confirmation; Cancel, Tab to
  Confirm, and Escape were exercised. The roster needed a manual Refresh after
  each action because the SDK snapshot briefly retained older membership
  state. A bounded settle-and-refresh change is awaiting build and UI retest.
  Permission failure was covered in the core test, not this UI pass.
- A second fresh macOS profile created a disposable room, saved its new name
  and topic through the Room details dialog, sent a message, and opened its
  redaction confirmation. Escape canceled the first attempt; on reopening,
  Tab moved from Cancel to Redact and Return confirmed. The UI displayed the
  saved room details and a redaction success notice. Direct Synapse reads
  confirmed the exact room name and topic state values, a redacted message
  event, and absence of the original message body. This checks one macOS UI
  path; a GUI permission denial, the revised automatic roster refresh, and
  Windows/Linux paths remain untested.
- A fresh macOS recovery profile imported an existing backup key and then
  displayed two historical encrypted events. Loading earlier history before
  and after the import produced no UI error. The SDK still showed Backup
  Enabled, Recovery Incomplete, and Device Unverified in this fixture; the
  second device had not accepted SAS. This is a decryption UI pass, not proof
  of complete recovery or verified identity.
- Opt-in Markdown outbound content passed 4/4 focused serialization tests:
  the Matrix plain `body` retains source text, safe Markdown becomes formatted
  HTML, raw HTML tags remain literal text, and unsafe link schemes do not
  survive. The updated native-call app suite passed 28/28, including the
  completed-SAS-request fallback and notification policy checks. The app
  binary built on macOS Apple Silicon. This preceded the later native rich
  timeline renderer.
- A separate opt-in loopback test passed 1/1 in 9.92 seconds against a fresh
  encrypted two-person room. Alice sent a formatted main message, reply, and
  edit; Bob's SDK decrypted each event. The test checked readable Matrix
  `body`, sanitized `formatted_body`, the reply and replacement relations,
  and an outer `m.room.encrypted` event on Synapse for all three sends. After
  switching from Alice to Carol on the second homeserver, stale Alice send,
  reply, and edit commands were rejected. This verifies the core send path on
  local Synapse, not a native composer click-through or another platform.
- After adding native `StyledText` display, four focused converter tests passed
  for supported formatting, escaped literal markup, safe links/lists, and
  bounds. A fresh encrypted two-device test passed 1/1 in 10.61 seconds:
  Bob's decrypted main message and edit both reached the safe rich projection,
  with the unchanged Matrix plain body still available. That native-call app
  suite passed 31/31, including the browser-link guard. A live decrypted
  rich-rendering click-through and other-platform acceptance remain open.
- A native macOS click-through reused the exact signed canonical-ID QA app and
  protected two-homeserver session from notification testing, then created a
  fresh private room on loopback Synapse B and enabled Megolm before sending.
  The **Md** button changed from “Use Markdown formatting” to “Clear Markdown
  formatting”; the UI sent `morpheus-ui-md-qa-200a **bold**
  <script>alert(1)</script>` and showed the source in its timeline. With **Md**
  off, it sent a second literal-source control message. Direct server reads
  confirmed both corresponding events (`$uHPp_CFocFIQvSXQtzjkkYEs6jQgv6CijQMWWUMwCpY`
  and `$uCBqr_O3Iyy1sN6eRbBm6RQmUqjSyCUAqy1baZ2_fKE`) were
  `m.room.encrypted` with ciphertext and no plaintext formatting fields.
  The choice was plain in another room, restored to on when returning to the
  QA room, stayed plain on account A, and restored to on after switching back
  to account B. This proves the native toggle, send route, and session-local
  account/room mode scoping; the exact UI-sent ciphertext was not decrypted
  to inspect its inner `formatted_body`. The separate two-device core test
  above covers formatted payload and decryption. Windows/Linux UI acceptance
  remains open.
- A rebuilt, ad hoc signed macOS QA app used a fresh account-free profile and
  a deterministic styled sample in the interactive `#general` preview. In a
  native screenshot, Sam's `great` rendered bold in the timeline. Its
  accessibility text remained the unmarked plain sentence. Opening Sam's
  thread initially exposed a plain-text root; after changing the root to the
  same safe `StyledText` component used by the timeline and replies, a rebuilt
  screenshot showed `great` bold in both places, with the thread root's plain
  accessibility label and `REPLIES · 0` composer intact. A final rebuild after
  adding a display-aligned accessible projection and preserving rich edit
  source passed the native-call app suite 33/33; another native screenshot
  showed bold `great` in both places and AX returned the unmarked sentence
  for both. Clicking Edit on a plain preview message loaded its original
  source and the “Save edit” control. The copied two-account
  notification QA profile did not restore in the re-signed bundle: the app
  stayed at disabled “Signing in…” despite receiving its profile path. No
  credentials were entered. This verifies native preview rendering and thread
  layout, not a live encrypted event displayed by the new renderer.
- A fresh encrypted two-device native MatrixRTC test passed 1/1 in 18.34
  seconds. After Bob left, Alice's local call reported media-key index 1;
  after Bob rejoined, he imported Alice's key and decoded newly published
  camera frames. This checks leave-triggered key rotation and fresh-join
  media, not recovery from a dropped network transport. The earlier screen
  and camera assertions in the same test also passed.
- A later fresh-fixture native RTC run passed 1/1 in 18.72 seconds after
  closing Alice's LiveKit signalling socket through the pinned test hook.
  The existing call reported degraded then healthy media connection without
  leaving its MatrixRTC membership; Bob decoded a new encrypted camera
  publication after the resume. Leave-triggered key rotation and post-rejoin
  media passed again. This is signal-resume evidence, not a full network outage
  or SFU restart check.
- A signed canonical-ID macOS QA app installed in `~/Applications` signed in
  to two disposable homeservers. An alternate-room message for active account
  A generated a `usernoted` request under `io.github.brk275.morpheus`; after
  temporarily enabling the app in Notifications and Sleep Focus, macOS logged
  delivery to alert, lock screen, and Notification Center with no Focus
  suppression. A message in the visible room produced no request; after
  switching to B, an A message produced none; and after both QA app processes
  fully quit, a B message produced none. The prior notification and Focus
  settings were restored and the QA installation removed. This is OS delivery
  evidence, not a captured banner or preview-content check. Linux/Windows,
  notification sound, and packaged release behavior remain open.
- A separate ad hoc signed macOS Apple Silicon debug app used a fresh,
  account-free offline profile to click through the new Notifications settings.
  Accessibility state showed desktop notifications change On→Off, sounds
  On→Off, and message previews Off→On. `config.toml` saved the corresponding
  `enabled = false`, `sounds = false`, and `show_message_preview = true` values.
  Adding `  Release   Candidate  ` displayed and saved `Release Candidate`;
  adding the same phrase in lowercase showed a duplicate error, and a phrase
  containing U+202E showed the control/direction-character error. The switches
  and phrase survived Home→Settings navigation and a quit/relaunch of that
  profile. An external, atomic TOML edit then changed the switches to On, On,
  Off and added `Watcher QA`; all four changes appeared live in Settings.
  Referencing a malformed theme pack showed the TOML parse error while the
  last valid notification controls remained visible; removing the reference
  cleared the error. That first binary left a highlight validation error
  visible after the input was cleared. In a later rebuilt, account-free macOS
  QA app, adding `Release Candidate`, attempting its lowercase duplicate,
  clearing the input, and navigating Home→Settings showed the duplicate error
  disappear on edit and stay cleared after navigation; the saved phrase stayed
  visible. This is preferences UI and file watcher evidence on macOS, not a
  notification banner, sound, or Linux/Windows acceptance check.

## Native outgoing video GUI acceptance (partial)

An ad hoc signed macOS Apple Silicon QA app used a temporary, debug-only
in-process login bootstrap with a protected disposable fixture. This bypassed
a Keychain ACL timeout when a separately signed test seeder tried to restore
its account in the GUI; the app's ordinary sign-in path has separate earlier
QA coverage. On the fresh `46632fc9428249d8` encrypted room, Bob's verified
device was running **before** Alice opened the call. The GUI joined, displayed
both participants, and reported active display sharing. Bob saw Alice as
reachable and decoded **one nonblank encrypted ScreenShare I420 frame**. The
GUI returned Share to Off after Stop and cleared its local call state on Leave.
This establishes a real GUI-to-peer frame path, but one frame does not establish
sustained picture or remote publication removal after Stop.

On a second fresh encrypted room (`93241672ba46d084`), Bob was again signed
in and verified before Alice opened the slot. Alice's signed GUI showed both
participants and an active display share. Bob decoded **10 nonblank encrypted
ScreenShare I420 frames** from Alice. This establishes sustained GUI display
picture delivery on the tested macOS machine. Alice then stopped sharing and
left; the GUI cleared both local controls. Bob-side publication removal after
Stop was not asserted in this run.

The same run's FaceTime HD Camera selection never reached Active: the UI
showed Starting, then Stopping, then a 20-second no-frame error. The pinned
Nokhwa macOS backend maps the camera's native 8-bit `420v` format incorrectly
and had an unbounded frame receive. A locally patched Nokhwa build in the
second run bounded frame receive and requested 8-bit NV12; the FaceTime camera
still produced no first frame, ending with a three-second channel timeout.
Further session/delegate diagnostics and camera publication remain open.
Earlier fresh-device observer attempts started Bob **after** Alice's encrypted
sticky membership was sent. Bob saw only himself for 45 seconds although
Alice saw Bob. When Bob's device existed first, Bob saw Alice as reachable.
The working hypothesis is that Bob could not decrypt Alice's historical
Megolm membership and the RTC sticky feeder projected only decrypted live
events. This is a beta call-join limitation requiring recovery or membership
republication; it is not evidence that every late call join fails.

Docker Desktop initially stopped responding but recovered; local Synapse,
native LiveKit, and RTC transport probe passed. Generated-pixel and command-line
capture proofs remain separate from the GUI evidence above.

When the disposable stack is healthy, run one bounded macOS acceptance pass:

1. Use the [RTC fixture runbook](../tests/LOCAL_MATRIX_RTC.md) to create a fresh
   encrypted Alice/Bob room. Keep credentials in its mode-0600 fixture file.
   Start Bob as a separately verified device in the headless RTC observer,
   then sign Alice into an isolated Morpheus app profile and join that room's
   call. Verify the two-person roster first. Repeat with a newly created Bob
   device joining after Alice to exercise encrypted membership recovery.
2. Open Camera, choose one labeled source, and wait at most 30 seconds for the
   UI to report it active. Bob must observe an unmuted Camera track and decode
   at least 10 nonblank frames with nonzero dimensions in 30 seconds. Stop
   camera; Bob must see that publication end within 15 seconds and the UI must
   show it off. Repeat once to catch a stop/start device race.
3. Share a display, then a window, one at a time. For each, inspect the picker
   label before selection, confirm the OS permission prompt if shown, require
   the UI active state and 10 nonblank decoded ScreenShare frames at Bob
   within 30 seconds, then Stop share and require publication removal within
   15 seconds. A changed window title or display geometry deliberately ends
   capture and requires reselection; check that the UI reports this rather
   than continuing on a changed source. The publication uses 1280×720 frames
   while its selected source identity remains unchanged.
4. Start camera and share together, then Leave. Require both local active
   indicators to clear and both Bob publications to end within 15 seconds.
   Repeat while choosing the bottom-left other account: the leave-call dialog
   must appear; cancelling retains the call and tracks, confirming leaves and
   switches only after both publications stop. No Alice source may appear in
   the new account's call stage.

Record app build, macOS and permission state, fixture mode, Bob's decoded
frame counts/dimensions, and any timeout. Keep the disposable credential file
out of logs. This checks one macOS run; Windows/Linux and packaged-app
permissions remain separate release gates.

## Earlier GitHub CI history

These results predate public-source publication. The newest candidate and its
remaining gates are recorded at the top of this log. Historical unsigned
package runs and their downloadable artifacts were removed before publication.

[Run 38012987752](https://github.com/brk275/Morpheus/actions/runs/38012987752)
passed on macOS Apple Silicon and Intel; Windows lacked `sqlite3.lib`, and
Ubuntu lacked `glib-2.0.pc` for the native-call build. Those CI dependencies
were fixed before [run 38019553752](https://github.com/brk275/Morpheus/actions/runs/38019553752),
which passed Ubuntu and both macOS targets. Its Windows native app link failed
because the pinned WebRTC library uses the static MSVC CRT while Rust had used
the dynamic CRT. A target-specific static-CRT build flag was checked in
[run 38021445771](https://github.com/brk275/Morpheus/actions/runs/38021445771):
Ubuntu and both macOS jobs passed; the Windows tests, desktop check, and
native-call app build passed. The final run conclusion was success on all four
jobs. The native audio mixer test runs on macOS Apple Silicon and passed there;
it is skipped on the other runners. [Run 38023378684](https://github.com/brk275/Morpheus/actions/runs/38023378684)
passed all four jobs for the subsequent room, keyboard, and video-feasibility
commit.
[Run 38024603658](https://github.com/brk275/Morpheus/actions/runs/38024603658)
passed all four jobs for the encrypted-drafts and macOS real-capture commit.
[Run 38026269910](https://github.com/brk275/Morpheus/actions/runs/38026269910)
passed all four jobs for the encrypted-attachment and native call-stage commit.
At that point, the newer discovery, streaming media, call-control, and slot
changes had not yet run in GitHub CI.
[Unsigned internal package QA run 38029335100](https://github.com/brk275/Morpheus/actions/runs/38029335100)
passed macOS Intel, Linux x64, and Windows x64 packaging jobs; its macOS
Apple Silicon job failed before steps due Actions capacity/budget. The latest
[CI run 38032533729](https://github.com/brk275/Morpheus/actions/runs/38032533729)
did not start any platform job because GitHub reported an Actions budget
preventing further use. That candidate had no four-platform CI result.
[Run 38042914642](https://github.com/brk275/Morpheus/actions/runs/38042914642)
for commit `31a2d65` also failed before any job steps on all four runners:
GitHub reported the same Actions budget restriction. This is a CI capacity
failure, not evidence of a source or platform build failure.

## Native timeline stress preview

An isolated macOS Apple Silicon debug app loaded 10,000 mixed-height synthetic
messages in the offline preview. The status was visible within 531 ms of the
UI action plus accessibility observation. Full accessibility trees exposed
only 5–7 numbered message rows per sampled viewport, including correct
consecutive ranges at the beginning, around message 5,200, and through
message 10,000. Ten page scrolls and three scrollbar seeks completed without
blank sampled views; the full timings, memory comparison, renderer counts,
and measurement limits are in [the 10,000-row QA record](../tests/TIMELINE_10K.md).
This synthetic check does not close the real Matrix room performance gate.

## Still required for beta

The current checks do not establish current-format Element Call interoperability,
live received video in the Morpheus call stage,
packaged-app screen-sharing controls and permissions, Linux or Windows GUI
behavior, screen-reader acceptance, install/update/remove behavior, signed
packages, or Cinny-level messaging and administration breadth.

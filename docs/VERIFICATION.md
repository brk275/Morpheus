# Verification log

Morpheus is a private pre-alpha. This log records checks actually completed;
the [release matrix](FEATURE_MATRIX.md) remains the beta gate.

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
  the only returned key before acknowledgment. The native-call app suite
  passed 22/22 tests, including gate and close-response checks; the default
  app check and native binary build passed. Fresh-device recovery through the
  actual desktop UI and SAS verification still need live acceptance.
- A gated two-Synapse room workflow passed private room and Space creation,
  invitation acceptance and decline, room-ID joining, and joining a registered
  room alias. Repeated runs initially hit Synapse's local invitation rate limit
  (`rc_invites.per_user`, HTTP 429); the loopback fixture now raises that limit.
  The app wires those room controls and protects each pending action against
  account changes. A newly created Space has an account-scoped temporary rail
  item while the SDK's room classification catches up. The Space UI path has
  passed a native compile, but not a live desktop click-through.
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
  call. The camera tile did not receive a frame: a third native session saw
  Bob as reachable, but `streams=[]` and no camera `remote_track` over 20
  samples while Bob retained his publication and pushed frames. This isolates
  the failed acceptance to media advertisement/transport in this unencrypted
  fixture; it does not validate Slint frame rendering or encrypted GUI calls.
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

## GitHub CI

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
The newer discovery, streaming media, call-control, and slot changes above
have not yet run in GitHub CI.

## Still required for beta

The current checks do not establish current-format Element Call interoperability, live
fresh-device key-backup recovery through the UI, live received video in the Morpheus call stage,
packaged-app screen-sharing controls and permissions, Linux or Windows GUI
behavior, screen-reader acceptance, install/update/remove behavior, signed
packages, or Cinny-level messaging and administration breadth.

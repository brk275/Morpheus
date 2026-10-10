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
  decrypted it. This establishes the core key-backup recovery path, not its UI.
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
  and Element Call interoperability remain untested. A separate native Slint
  proof displayed moving synthetic frames and rotation on macOS; it did not
  consume the received RTC track. See [the video feasibility note](VIDEO_FEASIBILITY.md) and
  [the RTC test runbook](../tests/LOCAL_MATRIX_RTC.md).

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
native-call app build passed. Windows post-job cache upload was still running
when this entry was written. The native audio mixer test runs on macOS Apple
Silicon and passed there; it is skipped on the other runners.

## Still required for beta

The current checks do not establish Element Call interoperability, the
key-backup recovery UI, native video rendering, screen sharing in a call,
Linux or Windows GUI behavior, screen-reader acceptance, install/update/remove
behavior, signed packages, or Cinny-level messaging and administration breadth.

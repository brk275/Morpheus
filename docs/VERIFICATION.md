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
- The gated encrypted-room messaging test passed reply, edit, reaction, and
  thread send/read through the Matrix core. Bob received the changes, and
  operations bound to Alice were rejected after switching to Carol. The new
  Slint controls compile, but their live click-through still needs desktop QA.
- One macOS app launch stalled in the OS credential-vault read and stayed on
  “Signing in…”. A 30-second vault-read timeout is now implemented so the UI
  can show an error. The underlying OS thread may remain blocked; the timeout
  and quit behavior still need a desktop retest.
- A loopback MatrixRTC stack passed authenticated transport discovery and
  OpenID-to-LiveKit JWT exchange. Synapse advertises the sticky sliding-sync
  feature required by the native call path. The gated native call test reached
  LiveKit signaling after adding the upstream macOS `-ObjC` link workaround.
  Docker Desktop's loopback-published media ports offered no viable ICE pair.
  A native LiveKit instance bound to macOS loopback connected both disposable
  participants over UDP and populated both rosters. An initial encrypted-key
  import failed because the devices lacked verified cross-signing identities.
  A fresh isolated fixture bootstrapped each identity and signed its device.
  The gated test then passed in 17.07 seconds: Bob imported Alice's media key,
  decoded nonzero synthetic microphone audio, left, and rejoined the call.
  This proves one local macOS two-device encrypted-audio path. Automatic network
  reconnect and Element Call interoperability remain untested. See
  [the RTC test runbook](../tests/LOCAL_MATRIX_RTC.md).

## First GitHub CI run

[Run 38012987752](https://github.com/brk275/Morpheus/actions/runs/38012987752)
passed all configured checks on macOS Apple Silicon and Intel. Windows failed
while linking Matrix tests because `sqlite3.lib` was absent. Ubuntu passed
the default checks and failed its native call build because `glib-2.0.pc` was
absent. The working tree adds bundled SQLite on Windows and GLib development
headers on Ubuntu; these fixes require a new CI run after push.

## Still required for beta

The current checks do not establish Element Call interoperability, the
key-backup recovery UI, native video rendering, screen sharing in a call,
Linux or Windows GUI behavior, screen-reader acceptance, install/update/remove
behavior, signed packages, or Cinny-level messaging and administration breadth.

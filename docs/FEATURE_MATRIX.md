# Feature status

Morpheus is **pre-alpha**. This page summarizes implemented behavior and the
remaining beta work. Passing a focused test does not establish release readiness.

[Behavior checklist](CHAT_CHECKLIST.md) · [Verification record](VERIFICATION.md) · [Release checklist](RELEASE_CHECKLIST.md)

The native builds below are covered by the fresh [PR 27 four-platform check](VERIFICATION.md#current-status-2026-10-22).
Later packaging and helper results retain their separate producing revisions.
Selected regressions do not establish every feature interaction or installed behavior.

## Accounts and security

| Area | Implemented | Still required for beta |
| --- | --- | --- |
| Multiple accounts | Separate sessions, encryption stores, drafts and vault entries; bottom-left switcher; inactive sync and send queues pause. | Installed switching and real sign-in across supported platforms. |
| Account protection | One optional local app password for switching and startup restore; password change/disable; direct removal with durable cleanup retries. | Installed interaction and screen-reader acceptance. |
| Sign-in | Password, system-browser SSO/OIDC, session restore and vaulted refresh tokens. | Real provider/browser handoff in installed apps. |
| Encryption | Encrypted messages/media, own-device verification, key backup and recovery controls. [Other-person SAS](PEER_VERIFICATION.md) is prepared in source. | Peer SAS SDK/controller/native UI checks, revised verification completion UI, failed-recovery behavior and cross-platform recovery acceptance. |

Two-homeserver isolation, restart and removal have disposable-account test
coverage. Fresh [native-vault run 39838186967](https://github.com/brk275/Morpheus/actions/runs/39838186967)
at `51b3c15` passed on Apple Silicon, Intel Mac and Windows: five contracts,
13 preparation checks, 11 restart/removal checks and cleanup on each target.
These fixtures use inactive accounts and do not prove loaded-account watcher
shutdown. Three separate watcher lifetime regressions passed on all four native
targets in [run 39821925679](https://github.com/brk275/Morpheus/actions/runs/39821925679).
Installed sign-in, account interaction and screen-reader behavior remain open.
See [account access](ACCOUNT_ACCESS.md) for password and removal behavior.

## Messaging and rooms

| Area | Implemented | Still required for beta |
| --- | --- | --- |
| Navigation | DMs, Spaces, rooms in multiple Spaces, discovery, invites with a searchable invitations center (deliberate per-invitation answers and a confirmed bounded bulk decline; see [invitations](INVITATIONS.md)), private room/Space creation and alias/ID joining. | Full navigation QA, federation and installed interaction across platforms. |
| Composer | Plain text and optional Markdown, mentions, replies, edits, reactions, encrypted room/thread drafts, saved-send recovery review and a per-room attachment queue (multiple files, reorder or remove before send, the homeserver's upload limit checked before reading, per-file progress, cancel and retry; compiled in native builds; live interaction remains open), and a per-account transfer center (side panel › Files, or Settings search "Transfers") listing uploads in every room, queued saves of received attachments with cancel and retry, and recently ended transfers by name only (compiled in native builds; installed interaction remains open). | Live thread and attachment UI, retry/cancel behavior, restart reconciliation, sticker composition (an implemented picker and gated send have native build coverage; see [stickers](STICKERS.md)) and IME coverage. |
| Timeline | Bounded native rich text, threads and virtualized message rows; instrumented encrypted 10,000-message fixture coverage described below. | Installed variable-height timeline interaction and cross-platform large-room and hardware performance acceptance. |
| Emoji and images | Offline Unicode emoji search; explicit, bounded PNG/baseline-JPEG attachment viewer; encrypted image previews off by default. Additions compiled in native builds include GIF and WebP previews, paused-by-default animated GIF/WebP/PNG playback with Play/Pause and Replay (opt-in autoplay, never under reduced motion), progressive JPEG, upright EXIF-rotated photos, 16-bit still PNG, quarter-turn rotation, pointer-centred zoom and Home/End in the viewer; see [image previews](IMAGE_PREVIEWS.md). | Full picker interaction, platform font coverage, viewer accessibility, and real-file/privacy acceptance of the viewer additions on each platform. |
| Search | Account-scoped encrypted local index; open-room and all-room search panel with debounced, cancellable, paged queries; opening a hit reveals its room, event and thread with nearby context; older hits open the conversation around them with Return to live; threads load earlier replies. A loaded hit, notification, permalink, pin or unread target is brought into the conversation's or thread's view from measured row geometry, bounded and cancelled with its owner, and the reader's top row is kept when earlier rows arrive (compiled in native builds; see [timeline navigation](TIMELINE_NAVIGATION.md)). | Interaction acceptance of the new panel and the measured reveal and anchoring on variable-height rows, large histories and cross-platform interaction. |
| Pinned messages | A permission-aware Pin/Unpin timeline action and a searchable Pins side-panel tab. They use authoritative list reads and changes that keep every other field. Redacted, undecryptable and unavailable pins are shown as such. See [pinned messages](PINNED_MESSAGES.md). | Further live fixture coverage, exact-event opening through the shared navigation seam, and installed interaction. |
| Administration | Room details, avatars, supported join rules, member roles, invite/kick/ban/unban and redaction controls. | Permission-error UI, roster refresh and full administration QA. |
| Notifications | Active-account native alerts with room visibility, privacy, sound, age and duplicate policies; searchable per-room and default Matrix push-rule levels; a room-header bell and panel for the open room with its level and per-device sound, preview and highlight switches, without changing call ringing; withdrawal of banners for rooms read here or elsewhere, with no automatic receipts of its own; click-to-open of the exact account, room and thread through the protected switch; per-room replacement and withdrawal where the OS backend supports it. | Visible/audible notification acceptance, click activation and withdrawal on each platform; push-rule and read-state withdrawal behavior against real homeservers; the room panel, its device switches and call-ring separation (compiled in native builds; live interaction remains open). |

Encrypted formatting, drafts, attachments, search and moderation have live
fixture coverage. Search has been tested through both native sliding sync and
classic `/sync`. Exact revisions and limitations are in the
[verification record](VERIFICATION.md).

Room/thread sends keep encrypted saved records before queueing. Restart recovery
offers explicit review when server acceptance is uncertain; it does not infer
delivery. Installed recovery and retry/cancel behavior remain release gates.

Image preview formats and privacy limits are documented in
[image previews](IMAGE_PREVIEWS.md). At source `a8404f8b`, the existing
[optimized encrypted 10,000-message check](https://github.com/brk275/Morpheus/actions/runs/39756940224)
passed on Linux with native sliding sync and classic `/sync`: each observed
10,000 distinct decrypted records and 240 accepted wheel inputs with render-hook
observations, plus switch-back, reopen and cleanup. This instrumented build is
not shipping-app or hardware-performance acceptance; GPU/presentation timing
and delegate instance counts were not measured. Experimental invitation-history bundles remain disabled
for sending and receiving; see the [policy](INVITATION_HISTORY_POLICY.md).

## Customization

| Available controls | Remaining acceptance |
| --- | --- |
| Nocturne, Terminal and Paper presets; editable TOML themes; independent fonts, scale, density and geometry. | Platform font coverage, installed controls and reload edge cases. |
| Theme studio (compiled in native builds): Settings look changes are a draft shown at once and saved by Apply, with Undo/Redo, Discard, per-section reset, explicit conflict choices and account fences; five starting-point variations; advisory text contrast; composer line height and weight; per-control icon colors; theme pack import, preview, install and export. See [theme studio](THEME_STUDIO.md). | Further interaction tests, installed interaction, concurrent-edit and account-switch checks. |
| Stacked/compact/nickname-column messages; avatar, timestamp and sender styling; semantic colors. | Complete rendered-text and accessibility checks. |
| Rail/sidebar navigation, optional Space labels, grouped/combined conversations, unread ordering and pane sizes. | Long names, large Space lists and physical keyboard navigation. |
| Favorites and Low priority sections from Matrix `m.favourite`/`m.lowpriority` tags, room and header menus, accessible favorite reordering with standard order values. | Tag writes on real homeservers, other clients' orders, context menus per platform, keyboard and screen-reader use. |
| Highlights, notifications, editable shortcuts and next-call audio-device preferences. | Actual sound/routing behavior and unavailable-device handling on hardware. |

Configuration and generated-UI guards have passed across all four native targets.
Real Linux preview captures are in the [gallery](screenshots/README.md).
See the [customization guide](CUSTOMIZATION.md) for the configuration reference.

## Calls and screen sharing

**Experimental.** Calls require a homeserver advertising compatible MatrixRTC
services. Current-format calls require encrypted rooms. Chat remains available
when call services are unavailable.

| Area | Implemented | Still required for beta |
| --- | --- | --- |
| Voice/video | Native encrypted transport, participant roster, mute/deafen, audio-device selection, reconnect handling and camera controls. | Reliable camera startup, received video in the native stage, physical routing/permissions/unplug and all-platform runtime acceptance. |
| Incoming calls | Call-and-notify, bounded incoming offers, account-bound Answer, a one-attempt protocol Decline, local-only Dismiss, a native ringtone, silent desktop alerts for the active account and stopping when your other verified device declines. | Live UI Answer and Decline, audible output, desktop alerts across all platforms and live declines from other devices. |
| Voice spaces | Shared/local room designation, navigation, participant roster and quiet join. | Complete drop-in UI behavior across platforms. |
| Screen sharing | Window/display selection, Start/Stop controls and encrypted picture transport alongside microphone audio. | Remote unpublish after Stop, native received tiles, packaged permissions and cross-platform interoperability. |

Calls stay joined while they recover: media reconnects inside the join,
outgoing audio and video pause while signaling reconnects (up to 25 seconds),
and a lost join is left and rejoined at most four times within 60 seconds.
This recovery is implemented but has not been exercised against a real outage.
Disposable encrypted-call fixtures cover participant departure, key rotation,
signaling reconnect and selected browser-peer scenarios. Full network outage,
SFU restart, physical-device behavior and current interoperability remain open.
At source `256205f9`, the [system Wayland fixture](VERIFICATION.md#current-status-2026-10-22)
passed first static frame, moving-pixel reopen, source-loss, stop, denial and
cancellation with all 33 function origins in the owned fixed client.
Untouched-static reopen, native-window capture, GNOME/KDE desktops, physical
devices and packaged capture remain separate open gates.

See [call acceptance](ELEMENT_CALL_ACCEPTANCE.md),
[incoming calls](INCOMING_CALLS.md) and [voice spaces](VOICE_SPACES.md).
Computer-audio sharing is later work.

## Platforms and distribution

The supported target matrix is Apple Silicon Mac, Intel Mac, Windows x64 and
Linux x64 on X11/Wayland. Tests apply to their recorded source revision; results
from different revisions do not establish a current release candidate.

- Native builds and selected regressions have passed on all four targets with
  the [PR 27 producing source and test-merge tree](VERIFICATION.md#current-status-2026-10-22).
- Generated-UI keyboard tests do not replace physical keyboard/IME testing or
  VoiceOver, Narrator and Orca acceptance.
- Unsigned package checks have partial results. The installed AppImage capture
  check [39842840653](https://github.com/brk275/Morpheus/actions/runs/39842840653)
  at `989a786` failed during pinned host-tool provisioning before compilation;
  no installed capture acceptance is claimed. Complete installation, normal
  close, removal, resource and permission checks remain required.
- Private [source generation](https://github.com/brk275/Morpheus/actions/runs/39841925560)
  at `989a786` passed all 1,174 locked packages and cleanup, with no uploaded
  artifacts. Source-content review and complete license, native/system/compiler
  source and relinking requirements remain open; distribution stays disabled.
- Signing, macOS notarization, a signed update feed, update/rollback and
  migration acceptance remain open. Update controls stay disabled without
  the required feed/key and supported installation.
- Debian packaging is deferred until a project Maintainer email is available.
- Installer downloads remain disabled. No public beta is available.

Use the [release checklist](RELEASE_CHECKLIST.md) for the complete gates and
the [verification record](VERIFICATION.md) for run-specific evidence.
Mobile clients and executable plugins are outside the first release.

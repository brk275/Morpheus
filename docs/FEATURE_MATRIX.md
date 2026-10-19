# Feature status

Morpheus is **pre-alpha**. This page summarizes implemented behavior and the
remaining beta work. Passing a focused test does not establish release readiness.

[Behavior checklist](CHAT_CHECKLIST.md) · [Verification record](VERIFICATION.md) · [Release checklist](RELEASE_CHECKLIST.md)

## Accounts and security

| Area | Implemented | Still required for beta |
| --- | --- | --- |
| Multiple accounts | Separate sessions, encryption stores, drafts and vault entries; bottom-left switcher; inactive sync and send queues pause. | Installed switching and real sign-in across supported platforms. |
| Account protection | One optional local app password for switching and startup restore; password change/disable; direct removal with durable cleanup retries. | Installed interaction and screen-reader acceptance. |
| Sign-in | Password, system-browser SSO/OIDC, session restore and vaulted refresh tokens. | Real provider/browser handoff in installed apps. |
| Encryption | Encrypted messages/media, device verification, key backup and recovery controls. | Revised verification completion UI, failed-recovery behavior and cross-platform recovery acceptance. |

Two-homeserver isolation, restart and removal have disposable-account test
coverage. Isolated synthetic-vault checks have passed on both Mac architectures
and Windows. These are core checks, not installed-app sign-in acceptance.
See [account access](ACCOUNT_ACCESS.md) for password and removal behavior.

## Messaging and rooms

| Area | Implemented | Still required for beta |
| --- | --- | --- |
| Navigation | DMs, Spaces, rooms in multiple Spaces, discovery, invites, private room/Space creation and alias/ID joining. | Full navigation QA, federation and installed interaction across platforms. |
| Composer | Plain text and optional Markdown, mentions, replies, edits, reactions, encrypted room/thread drafts, saved-send recovery review and attachment controls. | Live thread and attachment UI, retry/cancel behavior, restart reconciliation, sticker composition (a source-only picker and gated send exist; see [stickers](STICKERS.md)) and IME coverage. |
| Timeline | Bounded native rich text, threads and virtualized message rows. | Rendered encrypted-message UI and a real 10,000-message room with variable-height content. |
| Emoji and images | Offline Unicode emoji search; explicit, bounded PNG/baseline-JPEG attachment viewer; encrypted image previews off by default. | Full picker interaction, platform font coverage and viewer accessibility. |
| Search | Account-scoped encrypted local index, room/cross-room queries and event context. | All-room UI, large histories and cross-platform interaction. |
| Administration | Room details, avatars, supported join rules, member roles, invite/kick/ban/unban and redaction controls. | Permission-error UI, roster refresh and full administration QA. |
| Notifications | Active-account native alerts with room visibility, privacy, sound, age and duplicate policies; searchable per-room and default Matrix push-rule levels; private read receipts only for messages actually seen in the active window (scroll-aware), with a first-unread divider, an unread counter with Jump to latest and scroll anchoring; click-to-open of the exact account, room and thread through the protected switch; per-room replacement and withdrawal where the OS backend supports it. | Visible/audible notification acceptance, click activation and withdrawal on each platform; push-rule and receipt behavior against real homeservers; native acceptance of row-visibility receipts, divider, counter and scroll anchoring. |

Encrypted formatting, drafts, attachments, search and moderation have live
fixture coverage. Search has been tested through both native sliding sync and
classic `/sync`. Exact revisions and limitations are in the
[verification record](VERIFICATION.md).

Room/thread sends keep encrypted saved records before queueing. Restart recovery
offers explicit review when server acceptance is uncertain; it does not infer
delivery. Installed recovery and retry/cancel behavior remain release gates.

Image preview formats and privacy limits are documented in
[image previews](IMAGE_PREVIEWS.md). The synthetic 10,000-row preview does not
establish real-room performance. Experimental invitation-history bundles remain disabled
for sending and receiving; see the [policy](INVITATION_HISTORY_POLICY.md).

## Customization

| Available controls | Remaining acceptance |
| --- | --- |
| Nocturne, Terminal and Paper presets; editable TOML themes; independent fonts, scale, density and geometry. | Platform font coverage, installed controls and reload edge cases. |
| Stacked/compact/nickname-column messages; avatar, timestamp and sender styling; semantic colors. | Complete rendered-text and accessibility checks. |
| Rail/sidebar navigation, optional Space labels, grouped/combined conversations, unread ordering and pane sizes. | Long names, large Space lists and physical keyboard navigation. |
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
| Incoming calls | Call-and-notify, bounded incoming offers, account-bound Answer/Dismiss and a native ringtone. | Live UI Answer, audible output, incoming OS alerts and protocol decline. |
| Voice spaces | Shared/local room designation, navigation, participant roster and quiet join. | Complete drop-in UI behavior across platforms. |
| Screen sharing | Window/display selection, Start/Stop controls and encrypted picture transport alongside microphone audio. | Remote unpublish after Stop, native received tiles, packaged permissions and cross-platform interoperability. |

Disposable encrypted-call fixtures cover participant departure, key rotation,
signaling reconnect and selected browser-peer scenarios. Full network outage,
SFU restart, physical-device behavior and current interoperability remain open.
The Linux system Wayland first-frame failure also remains open; earlier passes
do not resolve it. Packaged capture is a separate gate.

See [call acceptance](ELEMENT_CALL_ACCEPTANCE.md),
[incoming calls](INCOMING_CALLS.md) and [voice spaces](VOICE_SPACES.md).
Computer-audio sharing is later work.

## Platforms and distribution

The supported target matrix is Apple Silicon Mac, Intel Mac, Windows x64 and
Linux x64 on X11/Wayland. Tests apply to their recorded source revision; results
from different revisions do not establish a current release candidate.

- Native builds and selected regressions have passed on all four targets.
- Generated-UI keyboard tests do not replace physical keyboard/IME testing or
  VoiceOver, Narrator and Orca acceptance.
- Unsigned package checks have partial results. Complete installation, normal
  close, removal, resource and permission checks remain required.
- Signing, macOS notarization, a signed update feed, update/rollback and
  migration acceptance remain open. Update controls stay disabled without
  the required feed/key and supported installation.
- Debian packaging is deferred until a project Maintainer email is available.
- Installer downloads remain disabled. No public beta is available.

Use the [release checklist](RELEASE_CHECKLIST.md) for the complete gates and
the [verification record](VERIFICATION.md) for run-specific evidence.
Mobile clients and executable plugins are outside the first release.

# Feature and release matrix

This is the beta gate, not a claim that the features are already implemented.
Use the [Cinny comparison checklist](CINNY_BENCHMARK.md) for the detailed
chat-behavior benchmark.
Mark an item complete only after it works on macOS, Linux, and Windows where
platform behavior is relevant.

| Area | Beta requirement | Status |
| --- | --- | --- |
| Accounts | Multiple isolated sessions, bottom-left switcher, safe logout | Wired; two-server isolation, switch, and restart test passed. Inactive account send queues pause with sync; an already in-flight request may finish. Selected-account room queues now retry recoverable send errors with bounded backoff. Preview call-switch confirmation passed; connected native call pending |
| Authentication | Password and browser-based SSO/OIDC, session restore | Password and session restore tested locally; Matrix SSO wired; OIDC and browser flow untested |
| Encryption | Verification, encrypted media, key backup and recovery | Two-device encrypted message, restart, new-device backup recovery, and encrypted file roundtrip passed in local tests; verification/recovery UI and live media UI acceptance pending |
| Navigation | DMs, Spaces, multi-parent rooms, discovery, invites | Space graph and rail wired; private room/Space creation, room ID/alias join, and invitation accept/decline UI wired; local core workflow passed; public discovery and full UI click-through pending |
| Messaging | Rich composer, replies, edits, reactions, threads, media, drafts | Plain-text reply/edit/heart reaction and a text thread panel wired; encrypted live action test passed. Encrypted per-room drafts passed two-account restart/isolation and plaintext-scan checks; a send completing after app exit can leave a stale draft. Bounded encrypted attachment upload/download core passed a local Alice-to-Bob roundtrip; native attach/save dialogs, progress, cancel/retry, and attachment card are wired but need live UI QA. Formatting, media previews, thread drafts, and full UI QA remain open |
| Search | Message and room search | Core API; UI pending |
| Administration | Room creation, roles/power levels, bans, redactions, settings | Private room/Space creation UI wired and local core test passed; permission management, bans, redactions, and settings UI pending |
| Notifications | Active-account native notifications while process runs | Planned |
| Configuration | In-app editor, TOML, live reload, themes, shortcuts | Theme, font scale, density, pane size, and unread-first settings wired; config saves now replace atomically across platforms. Remaining controls and malformed-edit UI acceptance pending |
| Calls | Encrypted DM and room voice/video with Element Call interop | Experimental native room audio with group mixer wired behind `native-calls`; macOS loopback two-device encrypted audio, `Camera` and `ScreenShare` I420 transport, roster, and leave/rejoin passed. Bounded remote video tile and live participant roster compile in the call stage. RTC core now has call-bound mute/deafen controls, but the app has not exposed them; actual app video, Element Call interop, automatic reconnect, full controls, and Linux/Windows runtime checks pending |
| Voice spaces | Drop-in room calls and visible participant roster | Shared state marker write/read passed between local devices, using an explicit server read because sliding sync omits this custom event; joined-call roster UI wired. Voice-space navigation, local designation UI, and drop-in acceptance pending |
| Screen share | Window/display picture plus microphone audio | Native macOS display and window pixels each reached Bob through an encrypted 640×360 MatrixRTC screen track in separate loopback tests; audio also passed. Remote call-stage tile is wired but has not passed live UI QA. Packaged-app permissions, share controls, Element Call interoperability, and Linux/Windows capture pending |
| Accessibility | Keyboard and VoiceOver/Narrator/Orca checks | Main macOS offline-preview keyboard path, menu autofocus, forward modal Tab loops, and Escape focus return checked; physical Shift+Tab, screen-reader, and other-platform checks pending |
| Distribution | Signed, installable, updateable packages | Planned |

Computer-audio sharing, mobile clients, executable plugins, and a Morpheus
hosted call service are later work.

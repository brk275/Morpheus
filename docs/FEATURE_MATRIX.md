# Feature and release matrix

This is the beta gate, not a claim that the features are already implemented.
Use the [Cinny comparison checklist](CINNY_BENCHMARK.md) for the detailed
chat-behavior benchmark.
Mark an item complete only after it works on macOS, Linux, and Windows where
platform behavior is relevant.

| Area | Beta requirement | Status |
| --- | --- | --- |
| Accounts | Multiple isolated sessions, bottom-left switcher, safe logout | Wired; two-server isolation, switch, and restart test passed; preview call-switch confirmation passed, connected native call pending |
| Authentication | Password and browser-based SSO/OIDC, session restore | Password and session restore tested locally; Matrix SSO wired; OIDC and browser flow untested |
| Encryption | Verification, encrypted media, key backup and recovery | Two-device encrypted message, restart, and new-device backup recovery passed; verification/recovery UI pending |
| Navigation | DMs, Spaces, multi-parent rooms, discovery, invites | Space graph and rail wired; private room/Space creation, room ID/alias join, and invitation accept/decline UI wired; local core workflow passed; public discovery and full UI click-through pending |
| Messaging | Rich composer, replies, edits, reactions, threads, media, drafts | Plain-text reply/edit/heart reaction and a text thread panel wired; encrypted live action test passed. Encrypted per-room drafts passed two-account restart/isolation and plaintext-scan checks; a send completing after app exit can leave a stale draft. Formatting, media, thread drafts, and full UI QA pending |
| Search | Message and room search | Core API; UI pending |
| Administration | Room creation, roles/power levels, bans, redactions, settings | Private room/Space creation UI wired and local core test passed; permission management, bans, redactions, and settings UI pending |
| Notifications | Active-account native notifications while process runs | Planned |
| Configuration | In-app editor, TOML, live reload, themes, shortcuts | Theme, font scale, density, pane size, and unread-first settings wired; remaining controls pending |
| Calls | Encrypted DM and room voice/video with Element Call interop | Experimental native room audio with group mixer wired behind `native-calls`; a macOS loopback two-device encrypted call passed synthetic audio, `Camera` I420 transport, and leave/rejoin; a separate synthetic Slint renderer passed. Call-stage video rendering, Element Call interop, automatic reconnect, full controls, and Linux/Windows checks pending |
| Voice spaces | Drop-in room calls and visible participant roster | State marker implemented; UI pending |
| Screen share | Window/display picture plus microphone audio | Native macOS display and window pixels each reached Bob through an encrypted 640×360 MatrixRTC screen track in separate loopback tests; audio also passed. Packaged-app permissions, call-stage rendering, controls, Element Call interoperability, and Linux/Windows capture pending |
| Accessibility | Keyboard and VoiceOver/Narrator/Orca checks | Main macOS offline-preview keyboard path, menu autofocus, forward modal Tab loops, and Escape focus return checked; physical Shift+Tab, screen-reader, and other-platform checks pending |
| Distribution | Signed, installable, updateable packages | Planned |

Computer-audio sharing, mobile clients, executable plugins, and a Morpheus
hosted call service are later work.

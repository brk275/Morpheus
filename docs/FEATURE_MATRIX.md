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
| Navigation | DMs, Spaces, multi-parent rooms, discovery, invites | Space graph and rail wired; discovery/invite UI pending |
| Messaging | Rich composer, replies, edits, reactions, threads, media, drafts | Plain-text reply/edit/heart reaction and a text thread panel wired; encrypted live action test passed; formatting, media, persistent drafts, and full UI QA pending |
| Search | Message and room search | Core API; UI pending |
| Administration | Room creation, roles/power levels, bans, redactions, settings | Core API; UI pending |
| Notifications | Active-account native notifications while process runs | Planned |
| Configuration | In-app editor, TOML, live reload, themes, shortcuts | Theme, font scale, density, pane size, and unread-first settings wired; remaining controls pending |
| Calls | Encrypted DM and room voice/video with Element Call interop | Experimental native room audio with group mixer wired behind `native-calls`; one macOS loopback two-device encrypted-audio and leave/rejoin test passed; Element Call interop, automatic reconnect, video, full controls, and Linux/Windows checks pending |
| Voice spaces | Drop-in room calls and visible participant roster | State marker implemented; UI pending |
| Screen share | Window/display picture plus microphone audio | Native capture adapter compiles on macOS; publication/interop pending |
| Accessibility | Keyboard and VoiceOver/Narrator/Orca checks | Accessible labels added; assistive-tech checks pending |
| Distribution | Signed, installable, updateable packages | Planned |

Computer-audio sharing, mobile clients, executable plugins, and a Morpheus
hosted call service are later work.

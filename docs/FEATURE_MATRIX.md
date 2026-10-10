# Feature and release matrix

This is the beta gate, not a claim that the features are already implemented.
Mark an item complete only after it works on macOS, Linux, and Windows where
platform behavior is relevant.

| Area | Beta requirement | Status |
| --- | --- | --- |
| Accounts | Multiple isolated sessions, bottom-left switcher, safe logout | Wired; two-server test pending |
| Authentication | Password and browser-based SSO/OIDC, session restore | Password and Matrix SSO wired; OIDC and live tests pending |
| Encryption | Verification, encrypted media, key backup and recovery | Core APIs; UI and two-device tests pending |
| Navigation | DMs, Spaces, multi-parent rooms, discovery, invites | Space graph and rail wired; discovery/invite UI pending |
| Messaging | Rich composer, replies, edits, reactions, threads, media, drafts | Plain-text timeline/send and in-session account/room drafts wired; remaining UI pending |
| Search | Message and room search | Core API; UI pending |
| Administration | Room creation, roles/power levels, bans, redactions, settings | Core API; UI pending |
| Notifications | Active-account native notifications while process runs | Planned |
| Configuration | In-app editor, TOML, live reload, themes, shortcuts | Theme/layout settings and TOML wired; remaining controls pending |
| Calls | Encrypted DM and room voice/video with Element Call interop | Experimental native room audio path wired behind `native-calls`; live interop, video, and full controls pending |
| Voice spaces | Drop-in room calls and visible participant roster | State marker implemented; UI pending |
| Screen share | Window/display picture plus microphone audio | Native capture adapter compiles on macOS; publication/interop pending |
| Accessibility | Keyboard and VoiceOver/Narrator/Orca checks | Accessible labels added; assistive-tech checks pending |
| Distribution | Signed, installable, updateable packages | Planned |

Computer-audio sharing, mobile clients, executable plugins, and a Morpheus
hosted call service are later work.

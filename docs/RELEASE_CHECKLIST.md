# Private beta and public release gate

The repository stays private, and installers stay private, until every relevant
item below has evidence from real accounts, homeservers, and installed packages.
The preview shell and unit tests alone do not satisfy this gate.

## Functional beta gate

- [ ] Sign in with password and system-browser SSO/OIDC on supported homeservers.
- [ ] Restore two accounts on different homeservers after restart. Confirm each
      account keeps its own room cache, device identity, cryptographic store,
      drafts, and credentials.
- [ ] Switch accounts with cached content shown immediately. Confirm only the
      active account syncs by default and active calls block switching until
      the user leaves.
- [ ] Verify devices and recover encrypted messages/media after restart using
      key backup. A lost credential-vault entry must request sign-in without
      reading a plaintext token file.
- [ ] Validate room lists and timelines with native sliding sync and classic
      `/sync`, including invite, leave, unread, and a room in multiple Spaces.
- [ ] Exercise the Cinny v4.12.7 messaging/navigation/admin benchmark in
      [the feature matrix](FEATURE_MATRIX.md), plus replies, edits, media,
      threads, search, drafts, notification settings, and room moderation.
- [ ] Test a 10,000-message room with variable-height content; scrolling must
      remain responsive and not instantiate every message delegate.
- [ ] Complete encrypted DM and group calls with Element Call: ringing,
      participant departure, reconnect, key rotation, mute/deafen, device
      selection, video, and window/display sharing with microphone audio.
- [ ] Join a voice-space room without ringing everyone and show the current
      participant roster. A homeserver without compatible MatrixRTC service
      must keep chat usable and explain why calls are unavailable.
- [ ] Exercise malformed theme/config edits. The last valid appearance remains
      active and the error is visible. Test settings and TOML round trips.
- [ ] Fully quit Morpheus and verify it sends no notifications.

## Platform gate

| Platform | Build and install | Calls and screen capture | Accessibility | Update and uninstall |
| --- | --- | --- | --- | --- |
| macOS Apple Silicon | [ ] | [ ] | VoiceOver [ ] | [ ] |
| macOS Intel | [ ] | [ ] | VoiceOver [ ] | [ ] |
| Windows x64 | [ ] | [ ] | Narrator [ ] | [ ] |
| Linux x64 X11 | [ ] | [ ] | Orca [ ] | [ ] |
| Linux x64 Wayland | [ ] | [ ] | Orca [ ] | [ ] |

Before public release, sign the binaries, notarize macOS packages, verify
signed updater metadata and rollback behavior, inspect bundled dependency
licenses, publish the AGPL source and notices, and document the homeserver
MatrixRTC service requirement. Keep signing keys and updater private keys out
of the repository and CI logs.

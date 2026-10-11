# Source publication and binary release gates

The source is available for public pre-alpha development. Public CI results are
not a beta release. Installer downloads remain disabled until every relevant
item below has evidence from real accounts, homeservers, and installed packages.
The preview shell and unit tests alone do not satisfy the binary release gate.

Before publishing source, scan the candidate and its Git history for secrets,
review screenshots and hosted logs, remove private QA installer artifacts,
verify least-privilege CI and private vulnerability reporting, and fix identified
credential-transport and capture-cancellation defects. Keep personal account
data and signing material outside the repository. Repeat the relevant checks
when publication inputs change.

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

## Update and distribution gate

- [ ] Produce signed updater artifacts and a versioned HTTPS feed with the exact
      signature and URL for each target. Reject an invalid signature and an
      unsupported format before installation.
- [ ] Keep in-app updates specific to installed macOS `.app`, Windows NSIS,
      and Linux AppImage builds. Serve an `.app` update bundle for macOS,
      including apps originally installed from a DMG. Give `.deb` and Pacman
      users a package-manager update path.
- [ ] For private beta packages, restrict both feed and asset access without
      embedding a GitHub repository token in Morpheus. Keep automatic updates
      disabled until access and signing are verified.
- [ ] Give private beta recipients the Corresponding Source required by the
      [AGPL](https://www.gnu.org/licenses/agpl-3.0.html) for distributed builds,
      matching each distributed build and its dependency patches.
- [ ] Verify the macOS DMG mounts without a full-AGPL agreement and the
      installed macOS, Windows, and Linux packages contain a readable license
      and required source notices.
- [ ] Verify the Windows package contains a GUI-subsystem executable and
      launches without a console window.
- [ ] Install and launch the Linux Debian package and AppImage on Ubuntu 24.04
      under Wayland and X11; confirm the generated `Depends` and ELF glibc and
      libstdc++ symbol requirements match that baseline.
- [ ] Include `libxi6` in a rebuilt Debian package and verify a clean X11
      installation launches without manually installing extra libraries.
- [ ] Put a project maintainer identity and working project contact address in
      the Debian package's `Maintainer` field; a personal name or email is not
      required. Debian packaging is deferred by the owner until this is provided.
- [ ] Establish and verify a Linux package build path for distributions older
      than Ubuntu 24.04 before claiming support for them.
- [ ] Test upgrades from installed older builds, failed or interrupted updates,
      recovery or rollback, retained account data, and update behavior during
      a call on every applicable platform.

Before public release, sign the binaries, notarize macOS packages, verify
update signatures and recovery behavior, inspect bundled dependency
licenses, publish the AGPL source and notices, and document the homeserver
MatrixRTC service requirement. Keep signing keys and updater private keys out
of the repository and CI logs.

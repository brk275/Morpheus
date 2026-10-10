# Cinny v4.12.7 comparison checklist

Cinny v4.12.7 is the product breadth reference for the first Morpheus beta.
This is a behavior checklist, not a source-code dependency or a promise that
every behavior is implemented. Review the released Cinny app alongside
Morpheus before marking a line complete. The [v4.12.7 release](https://github.com/cinnyapp/cinny/releases/tag/v4.12.7)
is the pinned comparison point; its [release history](https://github.com/cinnyapp/cinny/releases)
documents earlier voice/video rooms, DM and room video starts, code-block
formatting, media, and device-management changes.

## Navigation and discovery

- [ ] Home, direct messages, Spaces, nested Space navigation, and rooms shared
      by several Spaces behave correctly with both sync backends.
- [ ] Join and leave, room and Space invitations, room directory, aliases,
      public discovery, room creation, and Matrix links work end to end.
- [ ] Unread counts, read markers, favorites and room sorting stay accurate
      after restart and account switching.

## Conversation

- [ ] Send and display plain text, formatted text, links, mentions, emoji,
      stickers, replies, edits, redactions, reactions, and threaded messages.
- [ ] Upload, download, preview, and safely open images, video, audio, and
      files in encrypted and unencrypted rooms. The Matrix core passed an
      encrypted binary-file upload/download roundtrip with metadata, account
      isolation, and a local plaintext-store scan. Native attach/save dialogs,
      a timeline card, progress, cancel, and same-transaction retry are wired;
      live UI acceptance, media previews, type coverage, and unencrypted-room
      checks remain open.
- [ ] Search rooms, users, and message history; show results in their room and
      timeline context.
- [ ] Preserve per-room drafts, including across restart; failed sends keep the
      original text. Morpheus room drafts now survive restart in encrypted
      account storage, and a server rejection restores unchanged composer text.
      Pending sends completed after app exit can leave a stale draft; thread
      drafts and full UI acceptance are still pending.
- [ ] Render long and mixed-height timelines with virtualization and keyboard
      navigation; benchmark a 10,000-message room.

## Security and room management

- [ ] Verify devices and recover encrypted history with key backup after a
      fresh install. Show actionable errors for missing keys.
- [ ] Manage room name, topic, avatar, join rules, invites, roles, power levels,
      bans, kicks, and redactions subject to Matrix permissions.
- [ ] Configure notifications, muted rooms, highlights, and local privacy
      behavior; a fully quit app sends no notifications.

## Calls and desktop behavior

- [ ] DM and ordinary room calls ring appropriately; voice spaces allow drop-in
      joins without ringing every member and show a participant roster. A
      shared voice-space room-state marker and Morpheus roster snapshot passed
      a local two-device test. The native call-stage roster compiles, but
      voice-space navigation and live roster UI acceptance remain open.
- [ ] Encrypted voice and video, reconnect, device selection, and screen share
      interoperate with Element Call on compatible homeservers. Local encrypted
      transport has carried synthetic camera/audio and real macOS display and
      window pixels. The remote tile compiles; received-frame playback in that
      tile, publish controls, reconnect, and Element Call interop are unverified.
- [ ] Settings, themes, accessibility, and platform packaging work on macOS,
      Windows, and Linux.

See [the feature matrix](FEATURE_MATRIX.md) for current implementation status
and [the release gate](RELEASE_CHECKLIST.md) for required test evidence.

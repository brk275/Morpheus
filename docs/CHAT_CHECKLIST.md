# Chat and desktop behavior checklist

This checklist defines the behavior required for Morpheus's first beta.
Unchecked items remain release requirements, including partially implemented
features. Mark an item complete only after testing the full behavior and
recording the source revision, supported platforms and results.

The completed `a38fe43` verification in
[run 38737708120](https://github.com/brk275/Morpheus/actions/runs/38737708120)
passed the Linux live fixtures, including protected accounts, all four
Current/Sticky2025 DM/room ringing cases and all 13 moderation/settings facts. Native
builds and unit suites passed on all four targets; Windows also passed synthetic
capture. Linux failed its separate system Wayland capture fixture, so the
complete native matrix has not passed. No beta gate is
complete. These results apply to that revision; later source changes require
fresh evidence. See [the verification log](VERIFICATION.md).

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
      Encrypted replies, edits, reactions, and threads passed local core checks;
      the native Markdown toggle and send path passed a macOS click-through.
      A safe subset of rich text renders in the offline timeline and thread
      preview, and decrypted events passed that rich-text projection. Member
      suggestions and the original 44-entry starter picker guards passed on all
      four native targets at `c02a1b7`. The full catalog adds 3,944 canonical
      entries including skin-tone combinations; all five catalog regressions
      passed on all four native targets at `51c04ce`. Picker performance and font
      behavior remain unverified. The room-mention projection and expanded
      encrypted room/reply/thread/edit fixture passed again in Linux at
      `51c04ce`. Composer keyboard/IME/accessibility, live rendered UI, rendered
      emoji breadth, stickers and the complete behavior set remain open.
- [ ] Upload, download, preview, and safely open images, video, audio, and
      files in encrypted and unencrypted rooms. The Matrix core passed an
      encrypted binary-file upload/download roundtrip with metadata, account
      isolation, and a local plaintext-store scan. Native attach/save dialogs,
      a timeline card, progress, cancel, and same-transaction retry are wired;
      live UI acceptance remains open. A memory-only click-to-preview viewer
      for bounded PNG/baseline JPEG is implemented, with encrypted previews off
      by default. Its 13 guards passed on all four targets; the expanded
      encrypted/plain-image API fixture passed in Linux at `c02a1b7`.
      Real viewer acceptance remains open. Animation, WebP, audio/video previews
      and full type coverage remain open; see [image preview limits](IMAGE_PREVIEWS.md).
- [ ] Search rooms, users, and message history; show results in their room and
      timeline context.
- [ ] Preserve per-room drafts, including across restart; failed sends keep the
      original text. Morpheus room drafts now survive restart in encrypted
      account storage, and a server rejection restores unchanged composer text.
      Thread drafts now use separate encrypted account/room/root keys, with
      cached restore, guarded disk loads and revision checks. Thread sends
      retain text until the exact queued transaction is acknowledged; a
      per-thread guard prevents duplicate submissions during that session.
      Controller regressions passed all four native targets at `3f1a3df`;
      Linux live checks passed encrypted persistence/isolation and tracked-send
      acknowledgement. Live thread UI acceptance remains open. Queued room
      and thread sends are now recorded in the encrypted draft row before
      enqueue with their complete typed text, reconciled by exact transaction
      after restart, and offered per-record native retry, cancel, restore,
      copy and unconfirmed-review choices; unreadable saved data, and readable
      drafts other clients stored with attachments or reply/edit targets, are
      kept untouched until an explicit review of exactly that row. That source
      has not been built or run yet.
- [ ] Render long and mixed-height timelines with virtualization and keyboard
      navigation; benchmark a 10,000-message room.

## Security and room management

- [ ] Verify devices and recover encrypted history with key backup after a
      fresh install. Show actionable errors for missing keys.
- [ ] Manage room name, topic, avatar, join rules, invites, roles, power levels,
      bans, kicks, and redactions subject to Matrix permissions. Guarded
      name/topic/avatar/join-rule controls and staged image preparation are
      implemented. At `51c04ce`, native regressions passed on all four targets
      and Linux's disposable-homeserver fixture passed all 13 moderation/settings
      facts, including permissions, text/avatar/access-rule roundtrips, stale
      writes and cancellation. The room-settings preview passed visual review;
      interactive native UI acceptance and the complete behavior set remain open.
- [ ] Configure notifications, muted rooms, highlights, and local privacy
      behavior; a fully quit app sends no notifications.

## Calls and desktop behavior

- [ ] DM and ordinary room calls ring appropriately; voice spaces allow drop-in
      joins without ringing every member and show a participant roster. A
      shared voice-space room-state marker and Morpheus roster snapshot passed
      a local two-device test. A native two-person call-stage roster passed an
      isolated macOS app check; an encrypted macOS GUI call also joined with
      two participants. Voice-space navigation and designation, Call and notify,
      and incoming Answer/local Dismiss are wired. Incoming guards passed at
      `c7ad50c` and subsequent native suites. At `51c04ce`, all four Current and
      Sticky2025 DM/room live actor cases passed, including accepted departure
      observation. Current/DM also exercised account A→B→A and rejected the old
      offer. The native ringtone follows the exact admitted offer, configured
      speaker and local sound policy; its automated guards passed on all four
      targets at `51c04ce`. Actual Answer UI, audible ringing and device routing,
      protocol Decline and voice-space drop-in acceptance remain open.
- [ ] Encrypted voice and video, reconnect, device selection, and screen share
      interoperate with Element Call on compatible homeservers. Local encrypted
      transport has carried synthetic camera/audio and real macOS display and
      window pixels. An opt-in legacy room-state call exchanged encrypted
      microphone and video with an Element Call v0.26.1 browser peer. A
      stock v0.28.0-rc.1 peer passed all 11 facts in the pre-synced `Sticky2025`
      fixture at `51c04ce`; Current-format and fresh-device Element acceptance
      remain open. The remote tile and native Camera/Share controls compiled on
      earlier candidates. A fresh encrypted macOS GUI display share delivered ten
      nonblank encrypted `ScreenShare` I420 frames to its receiving peer.
      Remote removal after Stop, camera publication and Morpheus's received
      tile remain unverified. On the tested FaceTime camera, a format-selection
      change still produced no GUI frame: capture stayed Starting for about 20
      seconds, then moved through Stopping and Failed to Off. Native camera
      capture remains blocked on that macOS device. An unencrypted-room probe
      found no remote camera track despite a publishing peer. Current-format
      calls therefore reject cleartext rooms. Forced signalling socket closure
      recovered in a native test. Persisted next-call microphone/speaker
      selectors passed native compilation and regressions on all four targets
      at `3f1a3df`; real routing, permissions and unplug behavior remain open.
      A full network outage, SFU restart, GUI capture stop/start and
      cross-platform call acceptance also remain open.
- [ ] Settings, themes, accessibility, and platform packaging work on macOS,
      Windows, and Linux.

See [the feature matrix](FEATURE_MATRIX.md) for current implementation status
and [the release gate](RELEASE_CHECKLIST.md) for required test evidence.

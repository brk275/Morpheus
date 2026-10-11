# Cinny v4.12.7 comparison checklist

Cinny v4.12.7 is the product breadth reference for the first Morpheus beta.
This is a behavior checklist, not a source-code dependency or a promise that
every behavior is implemented. Review the released Cinny app alongside
Morpheus before marking a line complete. The [v4.12.7 release](https://github.com/cinnyapp/cinny/releases/tag/v4.12.7)
is the pinned comparison point; its [release history](https://github.com/cinnyapp/cinny/releases)
documents earlier voice/video rooms, DM and room video starts, code-block
formatting, media, and device-management changes.

The current `c02a1b7` Linux run passed expanded encrypted formatting, typed
drafts, image-preview APIs, search and the exact Wayland fixture. Current/room
incoming controller acceptance passed; the other three cases failed departure
observation. No beta gate is complete;
see [the current verification results](VERIFICATION.md).

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
      suggestions and a searchable 44-entry starter emoji picker are implemented;
      their guards passed on all four native targets at `c02a1b7`. The room-mention
      projection repair and expanded encrypted room/reply/thread/edit fixture
      passed in Linux at `c02a1b7`. Native keyboard/IME/accessibility, live rendered
      UI, full emoji breadth, stickers and the complete behavior set remain open.
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
      acknowledgement. Live thread UI acceptance remains open. Pending sends
      completed after app exit can leave stale drafts; queue reconciliation
      after restart and retry/cancel controls remain open.
- [ ] Render long and mixed-height timelines with virtualization and keyboard
      navigation; benchmark a 10,000-message room.

## Security and room management

- [ ] Verify devices and recover encrypted history with key backup after a
      fresh install. Show actionable errors for missing keys.
- [ ] Manage room name, topic, avatar, join rules, invites, roles, power levels,
      bans, kicks, and redactions subject to Matrix permissions. The next reviewed
      room-settings source adds guarded name/topic/avatar/join-rule controls and
      staged image preparation. Core and native UI acceptance remain pending.
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
      `c7ad50c` and subsequent native suites. At `c02a1b7`, all four live actor cases
      progressed through discovery and accepted join identity; Current/room passed,
      while Current/DM and both Sticky2025 cases failed departure observation.
      The bounded missing-reference observation change is source-only and is not
      a confirmed repair of those generic errors. Actual Answer UI, audible ringing,
      protocol Decline and voice-space drop-in acceptance remain open.
- [ ] Encrypted voice and video, reconnect, device selection, and screen share
      interoperate with Element Call on compatible homeservers. Local encrypted
      transport has carried synthetic camera/audio and real macOS display and
      window pixels. An opt-in legacy room-state call exchanged encrypted
      microphone and video with an Element Call v0.26.1 browser peer. A
      stock v0.28.0-rc.1 peer passed the pre-synced `Sticky2025` fixture
      at `c7ad50c`; Current-format and fresh-device Element acceptance remain
      open. The remote tile and native Camera/Share controls compiled on earlier
      candidates. A fresh encrypted macOS GUI display share delivered ten
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

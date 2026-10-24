# Morpheus room-state sync patch

This is `matrix-sdk-ui` **0.19.1** from Matrix Rust SDK revision
`c6ad52ecc09eaa36601f55effb0a48a533eb87dd`, under its original Apache-2.0
license. This does not upgrade the SDK or protocol. `UPSTREAM_FILES.sha256`
records every original crate file; `Cargo.toml.orig` preserves its manifest.
`MORPHEUS_PROVENANCE.json` records their hashes, the original workspace
manifest, license and formatting configuration. Workspace inheritance is
resolved explicitly in the local manifest; SDK siblings retain the same Git
revision and existing dependency versions and features are preserved.

## Change

`src/room_list_service/mod.rs` factors the existing required-state iterator
into one helper used by both the room list and room subscriptions. It retains
every upstream default and additionally requests these event types with all
state keys:

- `m.room.image_pack` for current sticker packs;
- `im.ponies.room_emotes` for legacy packs;
- `io.github.brk275.morpheus.voice` for shared voice-room presentation.

It does not request every state-event type. Existing account-data extensions,
encryption state and member synchronization remain unchanged. Classic sync
is unchanged. There is no new connection, automatic room join, direct room
state fetch, or media download. Sticker catalogs still use only the local
store, enforce joined-room/account selection checks, and require their existing
explicit consent before public media is loaded or sent into an encrypted room.

A focused unit test checks the preserved defaults, all three exact event types,
both request paths and the absence of an all-event-types wildcard. Hosted
unit and disposable-account integration verification are required before this
patch is accepted for a beta candidate.

## Public fixture identifiers

Five upstream test modules name their existing public Curve25519 sender
identifiers as constants, and use those constants in the original fields.
All original identifier bytes, encrypted vectors and assertions are preserved.
This distinguishes public identity data from credential assignments for the
unchanged default secret scanner. There are no scanner exclusions or suppressions.
The provenance record lists the affected files and their original hashes.


## Poll end authorization

The timeline retains the end event sender and its checked moderation
permission through pending aggregation replay. An end affects a poll only
when its sender created that poll or has known permission to redact other
users events. Unreadable or missing permission state grants no moderator
powers; a poll creator can still close their own poll. Local moderator ends
wait for their remote echo before affecting another users poll.

Invalid ends and redundant valid ends leave the item intact, rather than
aborting pending aggregation replay and dropping later votes. The earliest
valid end by server timestamp wins even when it arrives later. Existing
redaction-of-end limitations remain; this patch does not add reversible
poll ends.

The eight timeline source/test files are listed in the provenance record;
original upstream hashes and the SDK pin, manifest, features and license are
unchanged. Focused source regressions cover unauthorized and moderator ends,
relations received before the start, duplicate pending ends and earlier
valid timestamps. Full source review and isolated hosted verification are
required before accepting this patch.

Protocol reference: [MSC3381](https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/3381-polls.md).

## Opt-in mark-as-read without unread-flag writes (unread navigation)

`Timeline::mark_as_read` unsets the room's `m.marked_unread` flag in three
places: when no receipt would move forwards (an unthreaded `DoNotSend`), when a
live timeline has no latest event, and inside `Room::send_single_receipt`
after an unthreaded receipt. Morpheus owns that flag through one lossless,
bounded writer with its own receipts, so its explicit Mark as read and its
notification-center Mark read use an opt-in method instead.

`src/timeline/mod.rs` adds `Timeline::mark_as_read_without_unread_flag(
receipt_type, request_config)`. It keeps the private controller's decisions
unchanged: `latest_event_id`, `infer_thread_for_read_receipt` (an unthreaded
fully-read marker, otherwise the focus's receipt thread) and
`should_send_receipt` with `is_marking_room_as_read = true` (backwards,
own-event and explicit-unread rules). It differs only in that no branch ever
writes the flag, and a sent receipt goes through
`Room::send_single_receipt_without_unread_flag` (vendored `matrix-sdk`) with
the given request config. It never sends a raw receipt for the last visible
row, and its deduplication never mixes with the default method's.
`mark_as_read`, `send_single_receipt`, `send_multiple_receipts` and every
other caller are unchanged.

`tests/integration/timeline/read_receipts.rs` adds
`test_mark_as_read_without_unread_flag_never_writes_the_flag`, on the existing
mock-server fixtures. It mounts a flag-write mock with `expect(0)` and covers
the three branches where `mark_as_read` writes the flag: no latest event, a
sent unthreaded receipt (one receipt request, `expect(1)`), and a receipt that
would not move forwards. Afterwards the room stays marked unread.

| File | Upstream git blob | Current git blob |
| --- | --- | --- |
| src/timeline/mod.rs | 8c0c1fae4c5f9789e54738166d475c849570eee8 | b9d19df62262ff53283fb7325832c77d9869845b |
| tests/integration/timeline/read_receipts.rs | f49836d811a48b74de44462f569953db6a075aa0 | 59f147a69d94b3c97f442a0999da59c9aaa6be98 |

Upstream sha256 241a446a1f141889d11b22eab7dac3bbd789931ab0f1b34b0e6a8ccbdf0c5bd2
and 1f05b13efcedeac84b482bad028863b988db1af1a6d95e8afab1199f4e1a6157; current
sha256 83242025b0d7aff3d3f184a316152d571c1467792b7f5401acf461c1feb5d6d1 and
6a08e8ab10e6366bbfd783bf0cf5388dbb1154bd741c4c898c572f7da44c582d. Both files
are added to `modified_upstream_files` in `MORPHEUS_PROVENANCE.json`; their
upstream records and `UPSTREAM_FILES.sha256` are unchanged. No dependency,
feature, lockfile, workflow or scanner setting changes. Hosted unit and
integration verification are required before this patch is accepted.

## Owned sync startup and shutdown

`src/sync_service.rs` keeps the original supervisor handle until its actual
join, including after a cancelled stop or a terminal state publication. Stop
intent and completed channel submission are distinct; a cancelled submission
retains the same sender, and restart first joins the original stopping owner.

Both child streams acknowledge their first actual poll through the local
`src/sync_service/owned_lifecycle.rs` helper. In these exact pinned wrappers,
that poll constructs `SlidingSync::sync()` and its stop receiver before any
await. The helper preserves the first item, error, EOF or Pending. The
supervisor waits for both receipts, signals both required stops before any
expiration or join, and actually joins both original children even when one
failed. A later encryption-permit wait is stop-selectable before either child
is spawned; the initial start still acquires the permit before returning.

The same helper source is included only in Morpheus Matrix unit tests, in the
existing configuration/Matrix/RTC model step. Its deterministic tests use
local streams, channels, mutex permits and retained Tokio handles, with no
server, credential, device or extra dependency. Morpheus also retains its
original sync/recovery handles through cancelled joins, makes the initial and
retry SDK starts shutdown-selectable, and excludes stopping owners from its
already-active shortcut. No authentication, protocol, retry budget, deadline,
feature, manifest, lockfile, workflow or incoming-call policy changes.

The Linux OAuth restore run reported successful restore and authenticated
identity, then failed before its next phase. Its final marker cannot separate
the pause boundary from token extraction. This patch corrects a source-proven
lazy stop-subscription race and cancellation ownership gaps; it does not
establish that those gaps caused that historical run or claim runtime success.
Independent source review and affected fresh hosted verification remain required.

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

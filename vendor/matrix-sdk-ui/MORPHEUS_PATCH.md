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

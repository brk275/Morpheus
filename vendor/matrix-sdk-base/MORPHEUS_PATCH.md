# Morpheus keyless sticky receipt patch

This is `matrix-sdk-base` **0.19.1** from Matrix Rust SDK revision
`c6ad52ecc09eaa36601f55effb0a48a533eb87dd`, under its original Apache-2.0
license. It is not an SDK or protocol upgrade. `UPSTREAM_FILES.sha256` records
every original crate file; `Cargo.toml.orig` preserves its original manifest.
`MORPHEUS_PROVENANCE.json` records the inventory hash, original workspace
manifest/format/license hashes and added lockfile identities.

## Why a patch is needed

The pinned SDK decrypts MSC4480 extension events internally, then drops
decrypted events without `content.sticky_key` or `msc4354_sticky_key` from its
MSC4354 keyed map. Deployed `m.rtc.notification` events deliberately have no
such key. Generic SDK event handlers see timeline events, not these
extension-only receipts; the SDK notification fetch API needs known event IDs
and does not expose `EncryptionInfo` in its final item.

## Source changes

- `sticky/extract.rs`: classify keyless plaintext separately; preserve valid
  keyless plaintext/decrypted receipts with their actual `TimelineEventKind`.
  Malformed keyed content cannot become a keyless receipt.
- `sticky/keyless.rs`: a separate bounded, in-memory event-ID receipt cache,
  snapshots and update types. No event ID is inserted as a protocol sticky key.
- `sticky/mod.rs`: expose receipt snapshots and bounded broadcast wake-ups;
  include receipt expiry in the existing maintenance task; retain each pending
  encrypted event's original room generation.
- `sticky/decrypt.rs`: deliver successful keyless decryption through the same
  SDK decryptor/settings and late-room-key stream. Serialize retries and retry
  once after initial parking, so a key received just before parking is not lost.
  A late retry/re-park cannot acquire a new room generation.
- `response_processors/room/msc4186/extensions.rs`: collect both timeline and
  extension receipts after the joined room state has been saved.
- `response_processors/changes.rs`: remove receipts using SDK-normalized
  redaction targets after the corresponding store write; revoke their lifetime
  on room leave. This also handles redactions delivered through classic sync.
- `sticky/keyless_tests.rs` and focused unit cases: validate these paths using
  real SDK encryption and key imports, without a server or native device.

The existing `StickyKey`, `EphemeralMap`, `live()` and `subscribe()` keyed APIs
are unchanged. The crate does not promote a decrypted event to Verified, change
crypto trust requirements, add automatic key sharing, invent event fields,
start a second sync connection, persist plaintext receipts, or log event
content. The original SDK logging remains subject to Morpheus's log policy.

## Public contract

The existing `matrix_sdk::Room` dereferences to `BaseRoom`, exposing:

```rust,ignore
let mut updates = room.sticky_events().subscribe_keyless();
let snapshot = room.sticky_events().keyless_live();
```

Subscribe **before** reading the snapshot. `KeylessStickyEventsSnapshot`
contains `generation: u64`, `invalidated: bool` and
`events: Vec<KeylessStickyEvent>`. A receipt has `event_id`, `sender`,
`kind: TimelineEventKind`, and `expires_at: MilliSecondsSinceUnixEpoch`.
Its `raw()` and `encryption_info()` methods retain the SDK's original metadata,
including Unverified status. Call admission must require its own verified
sender/device, current account generation, room, slot, membership, lifetime,
targeting and quiet-room policy checks.

`KeylessStickyEventsUpdate` is a wake-up containing `generation` and
`invalidated`, not an event action or full delta. Reconcile a fresh snapshot
after every wake. On `Lagged`, stop pending admission; only a new complete
snapshot followed by fresh admission checks can establish a new observation.
An application may choose the stricter policy of rejecting the observation.

Limits per room are 128 retained receipts, 64 KiB per raw event, 1 MiB total raw
event bytes, and 1024 remembered redaction IDs of at most 255 bytes each. SDK
pending-list eviction also invalidates keyless receipt observation. Capacity
invalidation empties the cache and **latches for this cache instance**: neither
expiry, a new event nor room leave/rejoin clears it. A new client cache and
fresh sync are required to recover. There is no public reset shortcut.

Room leave invalidates the current generation; joined-room processing starts a
new generation while retaining any capacity latch. Redaction tombstones last
at most one protocol maximum sticky duration and prevent late decryption or
replay from reviving an observed redaction. Repeated delivery while a receipt
is retained keeps its earliest expiry and original decryption metadata.
Snapshots exclude expired entries even before the maintenance task wakes.

These are local receipt snapshots, not authoritative server state. A fresh
client must receive active keyless sticky events again from the homeserver.
Offline/restart delivery remains a hosted acceptance gate; this patch alone
does not prove server replay. Classic-sync incoming discovery still needs its
normal timeline path. No ringing or native capture is started by this crate.

## Manifest and verification

The manifest expands every upstream workspace dependency, feature/default and
lint setting. All SDK sibling/test dependencies retain the exact upstream git
revision and 0.19.1 version. The original workspace formatter settings are
included to avoid formatting unrelated vendored source. The root Cargo patch
replaces only `matrix-sdk-base`; adding it as a workspace member permits its
regressions to execute. Dev/optional resolution added 66 registry identities
and four SDK git identities, with **zero existing package version upgrades**;
the exact list is in the provenance JSON.

Required hosted command (not executed locally):

```sh
cargo test --locked -p matrix-sdk-base --no-default-features --features e2e-encryption,unstable-msc4354 --lib sticky
```

The command includes real extension-only decryption, late key delivery,
redaction/replay, old-generation retries, unchanged Unverified metadata and
Arc retention, extraction negatives, byte/count bounds and broadcast lag,
alongside upstream keyed-map tests. Live application receipt and redaction
tests are separate from these unit regressions. No local native build or test
execution was used to claim acceptance.

## Exact pending-event observation

With E2EE enabled, `StickyEvents::pending_event_state(event_id, sender)` is a
read-only query scoped to this room and one exact event/sender pair. It returns
only `Pending`, `Absent`, or `Retrying`; `as_str()` exposes the fixed labels
`pending`, `absent`, and `retrying`. The query acquires no asynchronous work,
performs no fetch/decryption/key request, changes no queue or cache, and exposes
no ciphertext, key, session identifier or raw event. `Pending` describes outer
encrypted receipt presence, **not authenticated decrypted membership**.
`Absent` describes only the current pending list; it does not prove the event
was never received. `Retrying` prevents a temporarily drained retry list from
being mistaken for absence. The existing 5000-entry pending cap applies.

The Element interoperability fixture samples this only after its unchanged
roster deadline has failed and only for the current Bob event ID observed in
the original peer's decrypted cache. Identifiers stay process-private; one
additional closed failure label is emitted. No production call, sync, trust,
key-sharing or request policy is changed by this observation. Runtime cause
and successful group interoperability remain unproven until hosted checks.

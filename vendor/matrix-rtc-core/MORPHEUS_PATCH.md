# Morpheus encrypted membership renewal patch

This is `matrix-rtc-core` **0.5.0-rc.2** from MatrixRTC revision
`53880ef1ef66b62df555ce71ccd1d6c85b813596`, under its original
AGPL-3.0-only OR LicenseRef-Element-Commercial license. Morpheus uses the AGPL
option. `UPSTREAM_FILES.sha256` inventories the original crate;
`Cargo.toml.orig` preserves its manifest. The vendored manifest expands the
upstream workspace values without changing dependency versions or features.
`MORPHEUS_PROVENANCE.json` records the upstream and license hashes.

## Why renewal is needed

A device created after an encrypted call membership was published may lack
that event's Megolm session. The ordinary delayed-leave heartbeat does not
republish it, and lifetime refresh can take 30 minutes. A fresh ordinary room
send lets the unchanged SDK share its current encryption session according to
the room's existing recipient policy. No historical key export is needed.

## Source changes

- `session.rs` tracks bounded logical peer identities for one local join. Only
  an encrypted, device-attributed membership in a known encrypted room, open
  slot and joined room-member set can request renewal. Own-device, unattributed,
  cleartext, invalid and mismatched membership identities are excluded. Initial
  peers are recorded after the initial send. Refreshes, reordered events and
  disappearance/reappearance cannot repeatedly request renewal for one identity.
- `own_membership.rs` coalesces requests into one pending bit. The existing
  heartbeat and sending mutex own all network work. Optional attempts are at
  most once per ten seconds, including failures. A backwards clock defers them.
  Leave disables pending work; a failed in-flight send cannot requeue after
  disabling. No additional task or public API is introduced.
- Optional renewal follows successful delayed-leave maintenance. Permanent
  lack of delayed-event support permits it only when the original published
  lifetime already equals the configured shorter degraded lifetime. A server
  losing support after a long-lifetime join does not permit optional renewal.
  Ordinary mandatory lifetime refresh keeps its existing behavior.
- Both refresh causes use the exact existing member content, sticky key,
  logical generation, transports, application and fixed published lifetime.
  The current event ID, watch and send timestamp advance only after an
  acknowledged send. Work arriving during that send remains pending.

## Bounds and trust

The ledger retains at most 256 tuples of sender, attributed device and logical
membership ID, each identifier at most 255 bytes. Reaching capacity disables
this optional optimization until the next local join; entries are not evicted
to make replay possible. A participant can exhaust that bound; ordinary expiry
refresh still works. Device-key replacement with the same logical tuple cannot
request another renewal because core does not expose key fingerprints.

Device attribution is only a bounded maintenance hint. This patch changes no
room recipient policy, identity verification, media-key eligibility, incoming
call admission, event format or encryption algorithm. It does not import old
keys or treat a decrypted membership as Verified. The send still uses the
normal pinned SDK backend. Unknown outcomes of interrupted network operations
retain the upstream limits.

## Verification

Seventeen new unit tests cover identity eligibility, baseline seeding, replay,
capacity, rejoin, passive clone/task lifetime, pacing, exact content and
acknowledgement state, delayed support failures, pending-during-send, leave
ordering and mandatory-refresh coalescing. Hosted verification runs these and
the upstream key-rotation integration tests:

```sh
cargo test --locked -p matrix-rtc-core --features testing --lib --test key_rotation
```

The separate live fixtures create the second device only after the first
encrypted membership exists, for both Current and Sticky2025. They require a
newly decrypted event for the same logical membership through production
renewal, with no fixture refresh, key injection or extended roster deadline.
The original event's absence from the sampled live map is not proof that its
historical ciphertext can never be decrypted. Source review and formatting
alone do not establish runtime recovery; see `docs/VERIFICATION.md` for results.

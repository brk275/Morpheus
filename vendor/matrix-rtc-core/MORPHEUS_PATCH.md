# Morpheus MatrixRTC compatibility patches

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

## Original Sticky2025 slot authority

The pinned member adapter maps the original `{application}#ROOM` wire slot into
its internal `{application}#room` session, while the original feeder used the
untranslated room-state key. The feeder now asks its existing per-room dialect
to project the complete slot snapshot. Stable state still wins over unstable
state for each ORIGINAL wire key before projection; malformed content remains
closed and removal still replaces the full snapshot.

Only Sticky2025 maps the reserved uppercase `ROOM` to internal `room` and ignores
the distinct Current lowercase wire slot. An absent or closed uppercase slot
cannot be opened by the Current slot. Custom IDs and Current projection remain
unchanged; RoomState still does not subscribe to slots. The existing core
application, room and encryption resolution remains unchanged. This creates no
public state, backend, key, task, fallback or incoming authorization.

Focused `morpheus-rtc --features native-devices --lib` regressions call this SAME
production collector and real backend-free core slot resolution. They cover
original authority, both type precedences, malformed state, refresh/removal,
custom IDs, room/format separation and the stock per-member encryption spelling.
They do not prove native/stock runtime interoperability or historical-key recovery.

## rc.7 predecessor media-key denial

Backport the public rc.7 fix from MatrixRTC revision
`5b8e73539878e40fa5bd88d637eb8eb2b6b2f0d0` without changing the rc.2 pin or
dependencies. During a delayed key switch, an arrival now receives only the
current outbound key, never the predecessor minted before that arrival. The
predecessor is retained locally for the existing overlap deadline; members
already holding it keep their original bounded continuity. The existing
actual-manager regression now requires the new key and refuses the predecessor.

This changes media-key distribution, not Megolm membership decryption, room
history, Matrix recipient policy or stock-peer behavior. It does not establish
a cause or cure for the fresh-device stock Bob membership failure. Source-only
review does not establish native execution or acceptance.

## rc.7 multi-party predecessor-key integration scenarios

`tests/key_rotation.rs` now uses the rc.7 no-predecessor contract from
`5b8e73539878e40fa5bd88d637eb8eb2b6b2f0d0`. The existing real-manager,
to-device bus and controlled-clock scenarios remain in place. Newcomers cannot
decrypt prejoin predecessor media during a pending switch; all other ordered
member pairs keep decrypting. The original switch deadline admits newcomer
media, without restarting the deadline for an arrival inside the window or
sending that arrival any predecessor. Captured prejoin keys remain unavailable
after switching. Existing departure, retry, rotation and coalescing checks remain.

For an arrival to k established members, each existing sender distributes its
fresh key to k peers and the newcomer distributes to k: k*(k+1) deliveries.
Thus four spaced arrivals cost 2+6+12=20, and eight cost
2+6+12+20+30+42+56=168; key creation remains 36 total/28 rotations for eight.
These derive from recipient counts, not acceptance of observed totals.

The original test bytes are retained in `tests/key_rotation.rs.upstream` and
still match `UPSTREAM_FILES.sha256`. No production, dependency, encryption,
trust, grace, delay or workflow setting changes. This is a source correction to
obsolete integration expectations; affected hosted checks remain required.

# Morpheus MatrixRTC constraint-ordering patch

Source: Element `matrix-rust-rtc`, tag `v0.5.0-rc.2`, exact commit
`53880ef1ef66b62df555ce71ccd1d6c85b813596`, original crate
`crates/matrix-rtc-transport`. All 14 original regular files are recorded in
`UPSTREAM_FILES.sha256`; the exact package and root workspace manifests and both
upstream license texts are retained. Morpheus distributes this code under
AGPL-3.0-only; the original dual-license declaration is preserved.

Local changes:

- Serialize constraint application through each actual connection, including the
  whole async transport operation. Keep one pending latest intent per membership
  and stream; changes supersede queued work, never an already-entered operation.
- Keep the original connection alive until an entered apply returns. Membership
  removal, connection replacement and pool close discard obsolete queued work.
  Recreated entries on the same connection use its existing serialization mutex.
- Reapply the current intent when an adopted or connected transport is installed.
  Keep the original 150 ms debounce, constraint resolution and transport mapping.
- Exercise the actual pool implementation with held async transport operations:
  both pause/active orders, coalescing, removal/recreation, connection replacement,
  queued close and entered-operation ownership.
- Keep an arbitrary backend first-poll panic as a failed worker; recover only
  structurally unchanged private admission state so later intent remains usable.
- Flatten the exact upstream workspace inheritance without changing versions,
  features or the original test dependencies. The sibling core keeps the same
  Git tag, resolved by Morpheus' existing root patch.

The confirmed concurrent-writer defect is not established as the cause of the
captured `initial_media` failure. This patch adds no media success, encryption,
identity, key policy, network or native ABI fallback.

Ownership limit: an entered transport operation is allowed to settle and is not
force-cancelled by removal. If a backend never returns, it retains that original
connection. The Tokio/wasm executor remains the original task owner; this patch
is not an application/native shutdown receipt or a guarantee against arbitrary
backend hangs or panics.

Morpheus applies its default Rust formatting to the copied Rust sources using
the pinned formatter. The original preimage inventory remains unchanged; these
formatting changes do not change the upstream license or dependency versions.

Additional accepted-key sender adoption correction:

- Keep the latest successfully applied key index per identity, separately from
  the existing recorded-key API. A native ring refusal or out-of-range index
  remains recorded and reported, but cannot replace an accepted sender index.
- Register the local sender and adopt its accepted index under the same existing
  sender mutex used by later activations. No key material is copied into this
  metadata and no new task, timer, wire format, trust decision or dependency is
  introduced. A record-only handler preserves its existing successful-apply
  behavior. Existing hooks still run synchronously under that mutex.
- Focused existing-module regressions exercise pre-registration refusal and
  bounds, accepted-index retention, peer isolation and an actual held ring
  operation completing across sender registration. No test has been run locally.

This corrects an attachment path that could select a recorded-but-rejected key;
there is no evidence that such rejection caused the captured first-frame outage.
The existing recorded-key listener is not a native acceptance/decryption receipt.

## Same-ID membership reappearance

Backport the public rc.7 same-member media restoration from MatrixRTC commit
5b8e73539878e40fa5bd88d637eb8eb2b6b2f0d0 without adopting its breaking APIs.
A membership lapse now moves its live handles into the existing private
identity buffer. Only a matching returned Matrix membership releases them;
SFU presence alone never creates a member or exposes its frames.

Private pooled handles carry their actual connection key so buffered media
ends with its real track/connection, idle retirement, replacement or pool close.
Events from a retired generation or a non-Up entry cannot refill that buffer.
Public get/get_many return the same original Arc handles, and original
constraint writer/first-poll/debounce behavior and same-stream replacement
events remain unchanged. No new task, timer, dependency, feature or wire field.

Existing-pool regressions require camera+microphone restoration without a new
TrackAdded/connect, exact Arc identity, no other-member release, actual track/
connection/idle/close retirement, adopted replacement and late dead-source
events, plus late old-focus removals after a replacement. They are source-only until affected hosted checks run; they do not
prove real native media or stock Bob's separate Megolm membership recovery.

Track provenance also includes the immutable connection generation. Parking and
flushing both require that exact key/generation to remain Up. Active handles keep
their existing presentation lifetime through focus replacement/idle retirement,
but cannot reenter the buffer after that source is gone. Track removal matches
the exact incarnation. Existing regressions cover replacement-before-lapse and
idle-retirement-before-lapse, retaining healthy new-incarnation same-Arc recovery.

### Receiver-owned encryption state across a membership lapse

The Morpheus integration of same-ID media restoration with hidden-video key
probes keeps the latest participant-level encryption observation in each exact
pooled receiver. Only an observation received while that receiver is present
on the current connection generation is retained. Remapping emits it after all
restored stream events only if every current receiver has the same known state.
A new or replaced receiver starts unknown and suppresses replay; removed tracks
and retired connections release their observations with their owned handles.
This adds no key sharing, decryption bypass, task, timer, or public API. The
transport callback identifies a participant, not a particular cryptor; replay
preserves that limited meaning and never asserts which track generated it.

Existing pool regression selectors cover retained-state replay, same-generation
receiver replacement, mixed known/unknown receivers, observations while parked,
pre-track observations, and track/connection retirement. The call SDK's existing
engine test module also checks that a same-ID restoration can resume an eligible
hidden key probe without a repeated failure event, preserving explicit release
and low-bandwidth constraints. These source tests still require hosted execution.

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

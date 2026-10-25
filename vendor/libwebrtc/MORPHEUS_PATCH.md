# Morpheus native video-source warm-up ownership patch

This is the same-version `libwebrtc` 0.3.38 registry package. Its complete
59-file published archive has SHA256
`fc0088d99d0757751962e6d60e8fcd42b1bfb5affd2b25da45e0735443330905`.
`.cargo_vcs_info.json` records LiveKit `rust-sdks` commit
`af369aae0fe074762ef0a2051529e7ec63045042`, package path `libwebrtc`.
`UPSTREAM_FILES.sha256` records every original archive byte identity. The
published normalized manifest, original manifest, original package lockfile,
original modified-source preimages, all original assertions and test bodies are retained. The complete Apache-2.0
LICENSE and NOTICE come from that exact official SDK commit; the published
crate omitted those two files. Copyright declarations remain unchanged.

## Production correction

The original warm-up future held a strong source clone until the first explicit
capture. A cancelled or failed publication before its first capture could drop
all caller owners while leaving the native source and its warm-up loop alive.
The native handle and counter now share the existing Arc-owned inner state.
The warm-up future captures only a Weak reference, upgrades after each existing
100 ms tick, and drops that temporary owner before the next wait. A normally
retained LocalVideoTrack keeps its original source clones alive. The first
explicit capture still stops warm-up; original timestamps, frame conversion,
rotation, native adaptation and packet-trailer behavior remain unchanged.
No unsafe implementation or new native API is added.

Three focused tests directly poll the same warm-up future used by the production
constructor. They cover last-owner retirement before polling, retirement while
waiting between ticks, and stopping warm-up after a real native-source capture
while a live clone remains. These tests create only a 16x16 synthetic native
source and I420 frame. They do not open capture devices, construct calls, claim
successful encoding/decryption/delivery, or establish compositor acceptance.

## Managed test metadata

The package is an excluded same-version root path patch, preserving its optional
`glib-main-loop` feature and all normal/target dependency declarations. The
managed normalized manifest omits only the `env_logger = "0.11"` development
table. `Cargo.toml.upstream` preserves that complete original table. The two
ignored-result `env_logger::builder().is_test(true).try_init()` initializers in
`src/native/peer_connection_factory.rs` and `src/peer_connection.rs` are omitted
from managed copies; `.rs.upstream` siblings preserve their exact originals.
No assertion, guard, trace site, test selector, cleanup or production behavior
changes in those two modules. This avoids adding logger dependencies or the
optional GLib family to the application lock solely for focused library tests.
The root lock retains all 1174 records and dependency arrays; only this package's
registry source/checksum fields are removed for the local path identity.

Future focused execution must use the existing default-feature vendor library
route, validate each literal selector, and enforce actual one-test pass/zero
failed/zero ignored accounting. Excluded-package selection and native cfg(test)
compilation still need fresh hosted proof; this source packet executes nothing.
No workflow or acceptance deadline is changed here.

## Limits

The timestamp-zero/relative-clock transition is not established as the cause of
the recorded initial-media failure. The Rust bridge still substitutes Unix time
for zero, and the native bridge still translates timestamps before adaptation.
The separately selected prospective aaeeee native source resets large clock
offsets; that prospective source is not attributed to the old prebuilt asset.
This ownership correction does not prove the recorded failure is repaired.

After the last wrapper drops, another independently retained native SharedPtr or
native track can still legitimately own the C++ source. This patch retires only
the warm-up task's accidental source ownership. The task may retain its synthetic
I420 buffer until its next scheduled tick; it holds no strong source over that
wait. Cancellation of an already-entered native call is not fabricated.

Existing native source/provenance, compiler/system, relinking, real-device,
account/media interoperability and legal distribution gates remain separate.
No source-only review or synthetic lifetime assertion authorizes distribution.

The modified Rust source copies are formatted with the project's authenticated
Rustfmt using stdin only, edition 2021 and disabled child traversal. Exact
original preimages and the original package inventory remain unchanged.

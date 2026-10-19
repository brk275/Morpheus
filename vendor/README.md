# Reviewed local dependency patches

Local patches retain upstream licenses and copyright notices. They are
dependencies of Morpheus, not original Morpheus application code. Keep patch
provenance and regression tests when updating a dependency.

| Crate | Purpose | Provenance |
| --- | --- | --- |
| lru 0.16.4 | Backport panic-safe cache removal | [Patch notes](lru/MORPHEUS_PATCH.md) |
| matrix-rtc-livekit | Secure authorization and media endpoints | [Patch notes](matrix-rtc-livekit/README.md) |
| matrix-rtc-core 0.5.0-rc.2 | Bounded encrypted membership renewal for newly joined devices | [Patch notes](matrix-rtc-core/MORPHEUS_PATCH.md) |
| matrix-sdk-ui 0.19.1 | Required room-state sync and poll end authorization | [Patch notes](matrix-sdk-ui/MORPHEUS_PATCH.md) |
| matrix-sdk-base 0.19.1 | Preserve bounded encrypted keyless sticky receipts and their revocation | [Patch notes](matrix-sdk-base/MORPHEUS_PATCH.md) |
| matrix-sdk-search 0.19.1 | Typed sender/date filters and filters-only search (NOT WIRED: no [patch] entry until root review; needs a matching matrix-sdk patch) | [Patch notes](matrix-sdk-search/MORPHEUS_PATCH.md) |
| livekit-api 0.5.3 | Secure signaling, fallback endpoints, and authenticated redirects | [Patch notes](livekit-api/README.md) |
| webrtc-sys 0.3.35 | Isolate pinned Linux WebRTC PipeWire stubs from the system PipeWire API | [Patch notes](webrtc-sys/MORPHEUS_PATCH.md) |
| webrtc-sys-build 0.3.18 | Verify pinned archive downloads and constrain native SDK extraction | [Patch notes](webrtc-sys-build/MORPHEUS_PATCH.md) |
| cargo-packager-updater 0.2.3 | HTTPS and bounded signed-update staging; installation disabled pending recovery checks | [Patch notes](cargo-packager-updater/README.md) |
| nokhwa 0.10.11 / nokhwa-bindings-macos 0.2.4 | Correct and bound native macOS capture | Details below |

## Nokhwa macOS camera patch

The Nokhwa directories started from `nokhwa` 0.10.11 and
`nokhwa-bindings-macos` 0.2.4 on crates.io, each with its upstream Apache
2.0 `LICENSE` and README. The workspace's `[patch.crates-io]` pins both local
copies so macOS camera behavior is controlled by the same source in builds.

Morpheus patches the AVFoundation path only:

- Match the selected native camera format by resolution, frame rate, and
  FOURCC; keep a selected camera object retained across autorelease-pool
  changes.
- Request 8-bit NV12 output and copy CoreVideo Y and UV planes row by row,
  excluding padding. Reject unexpected pixel formats and invalid plane sizes.
- Bound the native frame queue and frame wait. Report session, authorization,
  and callback counts on a first-frame timeout without logging image data.
- Stop the session, detach its delegate, and drain the serial callback queue
  before dropping sender state and Objective-C objects.

The camera path remains under live acceptance. The signed FaceTime HD Camera
run on this macOS host timed out before its first frame after the initial
format fix; session/callback diagnostics and encrypted camera delivery are
still required before a beta. Retest these changes when updating either
vendored crate, then remove the local patch when upstream fixes cover it.

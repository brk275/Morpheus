# Reviewed local dependency patches

Local patches retain upstream licenses and copyright notices. They are
dependencies of Morpheus, not original Morpheus application code. Keep patch
provenance and regression tests when updating a dependency.

| Crate | Purpose | Provenance |
| --- | --- | --- |
| i-slint-backend-winit 1.18.1 | Prefer known-transparent OpenGL configs before sample count | [Patch notes](i-slint-backend-winit/MORPHEUS_PATCH.md) |
| winit 0.30.13 | Existing macOS delegate termination and modal cleanup observation | [Patch notes](winit/MORPHEUS_PATCH.md) |
| notify-rust 4.18.2 | Owned notifications on the actual native worker | [Patch notes](notify-rust/MORPHEUS_PATCH.md) |
| mac-notification-sys 0.6.15 | Retained native banner records and bounded callbacks | [Patch notes](mac-notification-sys/MORPHEUS_PATCH.md) |
| tauri-winrt-notification 0.8.1 | Same-thread WinRT owners and exact withdrawal observations | [Patch notes](tauri-winrt-notification/MORPHEUS_PATCH.md) |
| cpal 0.18.2 | Retained macOS native audio owners and checked close; Windows event ownership; Linux close-on-exec wake descriptors | [Patch notes and exact upstream archive](cpal/OWNED_COREAUDIO.md) |
| lru 0.16.4 | Backport panic-safe cache removal | [Patch notes](lru/MORPHEUS_PATCH.md) |
| image-webp 0.2.4 | Bound lossless entropy-tree complexity before group allocation | [Patch notes](image-webp/MORPHEUS_PATCH.md) |
| matrix-rtc-livekit | Secure authorization and media endpoints | [Patch notes](matrix-rtc-livekit/README.md) |
| matrix-rtc-core 0.5.0-rc.2 | Bounded encrypted membership renewal for newly joined devices | [Patch notes](matrix-rtc-core/MORPHEUS_PATCH.md) |
| matrix-sdk-crypto 0.19.1 | Same-revision bounded canonical encrypted room-key preparation; original fixtures retained | [Patch notes](matrix-sdk-crypto/MORPHEUS_PATCH.md) |
| matrix-sdk 0.19.1 | Typed search entry points; optional per-request response-body limit; generated recovery-key handoff; opt-in stored-members identity status stream; opt-in receipt without the unread-flag write; explicit existing-room peer SAS preparation | [Patch notes](matrix-sdk/MORPHEUS_PATCH.md) |
| matrix-sdk-ui 0.19.1 | Required room-state sync and poll end authorization; opt-in mark-as-read without unread-flag writes | [Patch notes](matrix-sdk-ui/MORPHEUS_PATCH.md) |
| matrix-sdk-base 0.19.1 | Preserve bounded encrypted keyless sticky receipts and their revocation | [Patch notes](matrix-sdk-base/MORPHEUS_PATCH.md) |
| livekit-api 0.5.3 | Secure signaling, fallback endpoints, and authenticated redirects | [Patch notes](livekit-api/README.md) |
| webrtc-sys 0.3.35 | Isolate pinned Linux WebRTC PipeWire stubs from the system PipeWire API | [Patch notes](webrtc-sys/MORPHEUS_PATCH.md) |
| webrtc-sys-build 0.3.18 | Verify pinned archive downloads and constrain native SDK extraction | [Patch notes](webrtc-sys-build/MORPHEUS_PATCH.md) |
| cargo-packager-updater 0.2.3 | HTTPS and bounded signed-update staging; installation disabled pending recovery checks | [Patch notes](cargo-packager-updater/README.md) |
| nokhwa 0.10.11 / nokhwa-bindings-macos 0.2.4 | Correct and bound native macOS capture | Details below |

## CPAL source and verification boundary

`cpal` 0.18.2 is a local `[patch.crates-io]` library dependency explicitly
excluded from workspace membership. This preserves its optional backend API
without adding unused backend families to Cargo's all-workspace-feature lock
resolution. The existing default-feature-only `-p cpal --lib` regression
selections use the root lock; the managed manifest has no dev dependencies.
Its original crates.io archive has SHA256
`6f02e8d0327b42d3e2e4ab2119af397344eb9fc54a34bf0ddeaa1277af8681f1`
and records upstream commit `e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7`.
The complete Apache-2.0 [license](cpal/LICENSE), original source and hardware
examples/tests, `Cargo.toml.orig` and upstream lockfile stay preserved.
The managed normalized manifest excludes only upstream example declarations
and their four unused dev-tool dependencies; the root lock governs builds.

The existing native CI vendor step selects exact synthetic CPAL library cases
for each runner, checking each case exists before running it. Linux verifies
close-on-exec wake pipes, Windows verifies owned command-event lifetimes, and
Macs verify callback admission and retained owner/TLS joins. These cases do not
open devices and do not establish real-device or complete backend acceptance.
Windows COM/notification retirement and checked ALSA PCM disposal remain open;
Mac checked API completion does not prove every internal Apple thread ended.

The corresponding-source exporter includes all committed CPAL bytes and records
its local manifest, license candidate hash and build interface through its
existing workspace/path traversal. Linux AppImage notices copy the authenticated
Apache license and patch provenance. This addition does not complete broader
Rust/native licensing, SDK source or relinking requirements. Mac/Windows fixed
installer-resource ledgers still need their own complete notice integration.
No source-only review or synthetic pass authorizes distribution or installation.

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

## Additional bundled registry-library notices

Native audio attachment playback uses unmodified `symphonia-core` and
`symphonia-bundle-mp3` 0.6.1 (MPL-2.0), with only MP3 enabled and default
features off. These are registry packages, not local patches. Their exact
archive checksums remain in `Cargo.lock`; corresponding upstream source is
`pdeljanov/Symphonia` commit `ee35874b571a35a9a6e15d3bc9a3aaf8f11fbeee`.
The complete [MPL-2.0 license](../licenses/Symphonia-MPL-2.0.txt) and
[source notice](../licenses/Symphonia-NOTICE.txt) are included as package
resources and referenced in About. The small WAV reader is Morpheus code;
no Symphonia generic metadata/artwork or RIFF package is enabled.


## alsa 0.11.0: checked Linux PCM close

`alsa/` preserves the complete checksum-pinned upstream 0.11.0 source archive and dual license. A two-method `src/pcm.rs` amendment exposes consuming checked PCM close and the native type query used by the Linux CPAL owner. Original `Cargo.toml.orig`, source and attribution remain available. The root patch uses the existing version and dependency identities; upstream dev tooling is outside the application workspace. See `cpal/OWNED_ALSA.md` for exact archive checksums, native-status/consumed-pointer distinction, worker/TLS ownership, selected-device failure boundaries and unverified target acceptance. Generic committed-source export includes all vendored files; this does not complete broader native/system/compiler/relink or distribution gates.


## Windows native ownership amendment

The same-version CPAL 0.18.2 patch also implements retained native STA owners,
checked default-input UI activation and standard Global Interface Table transfer.
Native services, events and notification registration stay with their actual
apartment owner. Typed close observations distinguish Pending, Released and
Retained; callback flags, Stream Drop and a completed application body alone are
not native-release receipts. Native uncertainty retains the same owner and blocks
replacement. The returned receipt also requires actual worker/TLS and supervisor
joins. Notification unregistration permits the documented final COM release; it
does not claim to join every internal Windows callback thread.

This amendment adds only the normalized manifest's WindowsAndMessaging feature
on the already locked Windows bindings. Original upstream source, manifests,
lockfile and Apache license remain preserved. It adds no installed DLL, sidecar
or resource beyond the application binary. The seven-file signed Windows update
payload and its initial-installer resource contract remain fixed. Source-bound
notice files record provenance and unresolved obligations; they do not authorize
native shutdown, restart, installation or distribution.

The implementation remains subject to independent source review and fresh
platform, COM/activation/buffer/error, ordinary-shutdown, installed-update and
real-device acceptance. Production installation gates remain false. Existing Mac
and ALSA ownership notes and their explicit native/library limitations still apply.

## Native window and notification dependency graph

These four same-version crates.io patches are explicitly excluded from workspace
membership. Their original source, normalized manifests, Cargo.toml.orig, upstream
locks, tests, examples and any supplied license files remain preserved. The
mac-notification-sys archive omitted its license texts; unmodified LICENSE-MIT and
LICENSE-Apache are restored from its recorded upstream revision, as bound in that
crate's MORPHEUS_PATCH.md. This closes that text-collection gap; legal distribution
approval remains separate. Winit uses the existing
locked Mac backend through Slint's target-specific interface. No new package
version or registry source is selected.

The managed normalized notification manifests omit development-tool tables so
selected nonmember library tests can use the application lock. Full published
normalized manifests are retained as Cargo.toml.upstream. One notify-rust unit
expected-map initializer uses standard-library HashMap with the same two pairs
instead of maplit; src/hints.rs.upstream preserves the original source. No test
or assertion is removed or skipped. The original examples, integration tests and
doctests retain their source and original toolchain declarations; these selected
application checks do not claim their verification.

The unchanged corresponding-source catalogue follows explicit root path patches.
Portable native notices discover their original license texts and these patch
notes from committed source. SDK, system/compiler, relinking, installation and
legal distribution requirements remain separate gates.

The actual presenter, finite native owner book, original native worker/TLS and
supervisor joins, and all-platform shutdown must be integrated together. Linux
daemon activation and exact reply-identity corrections remain mandatory inputs.
A closed gate is not a native withdrawal or completion receipt.

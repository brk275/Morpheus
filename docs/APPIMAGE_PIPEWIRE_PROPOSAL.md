# AppImage-local PipeWire client and installed capture QA

Status: reviewed-source candidate, **not runtime-verified**. The Linux wrapper's
`INSTALLED_CAPTURE_READY = True` records completed source review of the adapter
and ownership/cleanup contract. It does not claim a passing installed capture or
distribution readiness. No native build, executable download, service, package,
installed app, or Rust test was run locally for this change. The workflow is manual, selects
Linux/AppImage alone, and uploads no installer or raw log.

## Evidence and scope

Historical comparison run 38147202698, job 114492955850, revision
4352c8829c64178dbb0cde857328ad8d4a8150da: system S failed, unmodified client A
failed, patched client B passed the same static-first-frame capture assertion.
Private receipt SHA256:
`0109f87679ccdf0ac45d617a8c82539c2fedd31311e1afb854260b1d66f763a1`.
The controlled result supports upstream commit
[800a25a01f99156d7d954f4d7fe1b0a6cf8ae7e3](https://github.com/PipeWire/pipewire/commit/800a25a01f99156d7d954f4d7fe1b0a6cf8ae7e3),
which completes exported-node state changes before the server completion check.
It does not prove a unique cause or universal reliability. Later ordinary system
capture has also passed; the historical comparison is not a claim that every
system-library run fails.

This candidate packages one fixed ABI-compatible PipeWire **client** in a Linux
x64 AppImage, keeping Ubuntu 24.04 as the baseline. The desktop still supplies
its server, portal, session manager, SPA modules and configuration. No system
file is replaced. Direct binaries continue to use the system client. The
existing system-library acceptance and its all-33-symbol runtime checks remain
unchanged and separate. Debian packaging is deferred and untouched.

## Entry point and tools

`packaging/linux/package_appimage.py` is a Linux-only private-QA wrapper. The
existing `package-qa.yml` manual workflow gains an explicit `appimage_capture`
option. Selection is rejected unless platform is Linux, format is AppImage and
update QA is disabled. Existing ordinary packaging and signed-update QA keep
their own paths. Readiness is checked before dependency installation/builds.
No shared `scripts/package.py` change or additional workflow trigger is needed.

The wrapper requires the public project's main branch on a standard GitHub
Ubuntu 24.04 x64 runner. It creates a private invocation root outside the
checkout, uses a fresh exclusive tool cache, and preserves HOME. Cargo's normal
target cache may be reused; package/profile/log roots cannot be reused.
Inherited loader, compiler, signing and plugin overrides are excluded.

Cargo Packager is exactly 0.11.8; Rust is exactly 1.99.0. Assets are downloaded
with bounded HTTPS-only curl, no inherited curlrc/proxy/authentication, and
verified against exact size and SHA256 before execution. There is no fallback
to mutable latest URLs or a user-owned populated cache. Official release/source
metadata is provenance, not a reproducible-build attestation.

| Component | Asset ID | Immutable source target | SHA256 |
| --- | ---: | --- | --- |
| Official linuxdeploy x64 | 602438599 | 07333c6e942c7d71782b66be924c5d867f9dfdfc | 8aea8da0f7f7039d2a2cecb14657d752a222a5e1d3825caeef186c82f751cdd1 |
| Official AppImage output plugin | 602435573 | 536b068787179ea901964bd7dabc7bf61e4941c3 | 49d6a17160675a6bd1781699aae6bdf7692d98552e02a3671d2183d10547842e |
| Official type2 runtime x64 | 596078161 | 8f39b89e2ac31e1640b3d3f7e9a5108e6ce805fa | 156f4bdbde9c52d01814600013e0a273f0118dc2de98975f3c8c63427ec79074 |

The original `packaging/linux/AppRun` is a small AGPL shell launcher. It derives
an absolute AppDir, rejects a missing/nonexecutable/symlinked target, and `exec`s
the normal binary with unchanged arguments, exit status and signals. It does
not set `LD_LIBRARY_PATH`. Linuxdeploy's ordinary relative ELF RUNPATHs supply
library lookup. The previous opaque AppRun asset is not shipped or trusted.

The pinned output plugin receives the pinned runtime through `LDAI_RUNTIME_FILE`.
After deployment all three tools plus the source launcher are checked again.
Only Cargo Packager's documented bytes 8–10 zeroing of the linuxdeploy invocation
copy is permitted; other bytes must match. The private cache is discarded.

## Fixed-client build and inventory

The client recipe verifies Ubuntu source 1.0.5-1ubuntu3.3, its exact Debian patch
series and upstream patch before building only
`src/pipewire/libpipewire-0.3.so.0.1005.0`. It verifies original/patched
`impl-node.c`, preserves the 33-object inventory, and records build ID,
DT_NEEDED, source/patch/config/tool-output hashes and source notices. The recipe
has a ten-minute command deadline. This is a pinned source recipe; bit-for-bit
reproduction on a second clean build has not been demonstrated.

Only `usr/lib/libpipewire-0.3.so.0` is staged; the host client is excluded from
automatic copying. The final deployed client must retain expected build ID,
SONAME and dependency inventory and only the explicitly allowed `$ORIGIN`
RUNPATH. Pre/post-deployment hashes are separate because patchelf changes bytes.
Build-ID agreement is not claimed as a cryptographic proof of semantic identity.

`appimage_notices.py` uses owned readelf and bounded dpkg queries, never `ldd`, to
inventory ELF dependencies and package/source versions. It copies exact package
copyright and referenced common-license text plus project/client source notices.
Final unknown ELF components, unsafe symlinks and unexpected dependency/RPATH
changes are rejected. The exact original AppRun and staged notices must survive.

**Distribution readiness remains false.** The adapter explicitly reports these
unresolved public-distribution inputs:

- Exact source/license provenance for all statically embedded AppImage runtime dependencies.
- Complete Rust and vendored static dependency notices and corresponding sources.
- Corresponding sources for bundled system packages.

The runtime digest denotes the pinned input, not a claim to have recovered or
attested every final embedded runtime byte. A private candidate may be exercised
without falsely declaring those release obligations complete. No release,
upload, signing key, public update feed or paid service is enabled.

The compatibility footer is appended before the final AppImage hash. If official
signing is later provisioned, it must cover these final bytes.

## Actual installed-app acceptance

`appimage_capture.py` builds the receiving fixture peer once and executes two
fresh account/profile cases: ordinary loading, then per-child
`LD_DEBUG=bindings` plus `LD_BIND_NOW=1`. The latter supplies binding evidence;
it cannot substitute for the first ordinary scheduling/capture case.

Each case executes in a fresh Python worker before importing AT-SPI, so its
process-wide accessibility client cannot retain the previous case's bus. The
worker enables `IsEnabled` and `ScreenReaderEnabled` on its owned private bus
before discovering the app. Private input/result files bind the case to exact
artifact, executable, client, peer, backend and fixture hashes. The worker must
be a direct child of the validated namespace PID1 with the same unprivileged
identity, private PID namespace, zero capabilities and `NoNewPrivs`; the normal
command runner retains its PID1 requirement. Fixed progress and cleanup facts
are validated only after the worker has stopped.

Each case starts an owned private DBus, SecretService, headless Sway, system
PipeWire/WirePlumber and the existing pinned SHM portal fixture. The normal
installed AppImage is launched normally. AT-SPI enters its disposable account,
selects the real encrypted room and uses the actual Join, Share, Stop and Leave
controls. No diagnostic application command, UI state setter, frame injection,
key override or sender test implementation is used.

A normal TOML media preference selects two nonce device IDs. A bounded peer
catalog lookup first proves neither ID exists; no default microphone/speaker or
camera is opened. This intentionally tests screen sharing with unavailable
local audio, not audio-device routing. The peer joins the real MatrixRTC room
and receives only the exact GUI sender's encrypted membership/device-associated
ScreenShare track. This is encrypted sender/device attribution, not an assertion
of cross-user trust verification.

The peer requires all of:

1. An unchanged initial static red output frame, sampled across a grid.
2. A subsequent real green desktop change.
3. Remote screen-track removal while the call remains present after Stop.
4. Reopening with a real yellow desktop transition and fresh yellow pixels.
5. A separate magenta change after reopening.
6. Remote track removal and ordinary GUI recovery after source output removal.
7. GUI Leave, remote membership departure, peer cleanup and exact named test pass.

Untouched-static-source reopening is **not** proven; the reopened source has an
explicit fresh pixel transition. No received pixels, account IDs or source
names are published. The app's own window must be on the other output so it
cannot cover the colored capture source.

The Rust peer lives in `packaging/linux/appimage_peer.rs`, registered as a nested
module of the existing Linux-only `wayland_capture` target. Its ignored exact
case is `appimage_peer::receives_actual_installed_appimage_screen`. Its two pure
color guards have not been executed locally. Ordinary system capture selects
its existing exact named test; this nested peer cannot satisfy that gate.

### Binding and process proof

The existing WebRTC static isolation inventory still covers all 33 collision
symbols, and the separate test ELF still proves all 33 system bindings. The
installed release proof derives its **actual imports** from shipped ELF dynsym.
Unused members of the 33 are reported as not imported, never runtime-verified.

The harness retains the unreaped launcher, private nonce, PID start time and
pidfd, then verifies whole executable/client/AppRun bytes against the inspected
AppImage. `/proc` must show the exact held executable and client inodes, with no
second PipeWire provider. Checks repeat during the live case.

For the binding case every actual `pw_*` app import must resolve to that exact
shipped client in the completed private glibc trace. A normal relative RUNPATH
may print `usr/bin/../lib`; each exact observed spelling is accepted only after
live canonical-path, inode and whole-file hash verification against the already
held client. The final trace may not introduce an unattested alias. Library-local
bindings cannot substitute for application-owned bindings.

### Ownership, bounds and diagnostics

Every subprocess is bounded and stays in the existing mandatory PID namespace.
Owned group leaders are observed without reaping; final cleanup kills owned
groups before reaping. Both stdout and stderr are drained into capped 0600 logs.
A second owned namespace performs Docker teardown even if the first times out.
Checked container/volume/network absence is required before taking fixture
ownership. Both service teardowns and checked final container/network absence
run on failure. Fixture data volumes are intentionally retained, not claimed
wiped. Temporary profiles, packages and logs are removed with the private root.

Build limits are 60 minutes for the app, 30 minutes for the receiving test ELF,
15 minutes for packaging, and 10 minutes across the client recipe. Each fresh
case worker has a 15-minute deadline. Each peer has
a 420-second main phase plus bounded cleanup; the outer namespace has a hard
120-minute deadline and separate bounded teardown. The manual job remains
150 minutes, including tool setup. These are caps, not runtime estimates.

Before raw logs are removed, Gitleaks scans them privately with 100% redaction.
Only a strict path-free schema can be printed: wrapper/command phase, capture
mode/phase, fixed completed peer facts, the first failure category and separate
cleanup outcomes. Later cleanup cannot erase the original failure position.
Absent/malformed progress or a failed scan emits only a fixed withheld marker.
Raw error strings, server bodies, paths, process IDs, tokens and pixels never
enter this schema. No raw log or installer artifact is uploaded.

## Source verification and remaining gates

The worker, containment, cleanup and diagnostic suite passed 53 focused Python
checks using mocks/pure filesystem fixtures; CI selection passed 21 checks.
Source review caught and corrected the descendant PID guard and supervisor
cleanup-reporting issues before hosted execution. Actionlint and Python syntax
checks pass. This manual fixed-client package/capture path has not run; source
enablement is not evidence of installed compatibility.

At `a38fe43`, native CI compiled the Linux `wayland_capture` target containing
the receiving peer. Its separate system-library capture failed. That compiled
target does not establish execution of the installed-AppImage peer or either
fresh worker case.

Required next evidence: build the release receiving test and fixed-client
AppImage on a standard free Linux runner, then run this manual installed case
only after the candidate's native
jobs and cache posts finish. Record exact source, tool/client/package hashes and
both ordinary/binding outcomes. Real desktop portals, hardware/device behavior,
signed packages, installation/update/rollback and the full release requirements
remain separate gates. Do not replace or relabel the system-library result.

Integration also includes the reviewed shared UI label change: the space rail
retains `Open call stage`; the conversation toolbar uses
`Open current room call stage`. This permits an unambiguous accessible action.

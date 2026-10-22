# AppImage-local PipeWire client and installed capture QA

Status: source-build proposal pending independent review, **not runtime-verified**. The Linux wrapper's
`INSTALLED_CAPTURE_READY = True` records completed source review of the adapter
and ownership/cleanup contract. It does not claim a passing installed capture or
distribution readiness. No native build, executable execution, service, package,
installed app, or Rust test was run locally for this change. One runtime asset
was downloaded as inert bytes for source/provenance review; that prebuilt runtime is not an input to this proposal. The workflow is manual, selects
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
file is replaced. The separate native source-build proposal uses its distinct
`libmorpheus-pipewire-0.3.so.0` SONAME and private prefix. This AppImage path keeps
the original `libpipewire-0.3.so.0` and ships only its verified deployed copy.
The old unmodified-system results remain separate evidence. Debian packaging is
deferred and untouched.

## Entry point and tools

`packaging/linux/package_appimage.py` is a Linux-only private-QA wrapper. The
existing `package-qa.yml` manual workflow gains an explicit `appimage_capture`
option. Selection is rejected unless platform is Linux, format is AppImage and
update QA is disabled. Readiness is checked before dependency installation/builds.
The explicit generic AppImage QA command now delegates to this wrapper rather
than building an unshipped native-prefix binary. It retains one private QA image
only after both installed capture cases, owned cleanup and scan pass, preserving
the subsequent original package smoke gate. Signed-update QA uses the same
source-built runtime/original-SONAME build stage but retains its separate genuine
old/new package, official signer, installed update and rollback assertions. No
workflow trigger, artifact upload or release publication is added. The
accompanying permanent Linux workflow proposal adds the scanner argument and
exact host provisioning; it remains held for independent source/dependency review
and fresh hosted acceptance before any package run.

The wrapper requires the public project's main branch on a standard GitHub
Ubuntu 24.04 x64 runner. It creates a private invocation root outside the
checkout, uses a fresh exclusive tool cache, and preserves HOME. Cargo's normal
target cache may be reused; package/profile/log roots cannot be reused.
Inherited loader, compiler, signing and plugin overrides are excluded.

Cargo Packager is exactly 0.11.8; Rust is exactly 1.99.0. Assets are downloaded
with bounded HTTPS-only curl, no inherited curlrc/proxy/authentication, and
verified against exact size and SHA256 before execution. There is no fallback
to mutable latest URLs or a user-owned populated cache. The two deployment-tool
assets use exact public GitHub API asset IDs and constant
`Accept: application/octet-stream`; only the expected repository paths are
accepted, without an API token. Deletion fails closed. Their original payloads,
digests and source targets remain unchanged:

| Component | Asset ID | Immutable source target | SHA256 |
| --- | ---: | --- | --- |
| Official linuxdeploy x64 | 602438599 | 07333c6e942c7d71782b66be924c5d867f9dfdfc | 8aea8da0f7f7039d2a2cecb14657d752a222a5e1d3825caeef186c82f751cdd1 |
| Official AppImage output plugin | 602435573 | 536b068787179ea901964bd7dabc7bf61e4941c3 | 49d6a17160675a6bd1781699aae6bdf7692d98552e02a3671d2183d10547842e |

The previously pinned standalone runtime asset was deleted. Its later signed
replacement contained Squashfuse 0.5.2 without the known malformed-image fixes.
Neither that replacement nor its signature is used as a claim of repaired code.
`appimage_runtime_build.py` instead builds a new static-musl runtime from seven
size/SHA256-pinned source archives in `runtime-build.json`: type2-runtime
fedfad51, Squashfuse 0.5.2, musl 1.2.4, libfuse 3.15.0, zlib 1.3.2, zstd 1.5.6 and
mimalloc 2.1.7. The original upstream Docker/Alpine recipe is not executed.

The private prefix is fresh and outside the checkout. Exact packaged GCC13,
binutils, patch, Autotools, Meson, Ninja and pkgconf versions from 18 fixed Ubuntu
DEBs and 634 tool/header/library hashes are checked against
`runtime-build-host.json` before source commands. The original 15 package pins
and 632 file rows are unchanged; matching `gcc-13-base`, `binutils-common` and
`libbinutils` close the exact sibling dependencies. The two additional hashes
belong to libbfd and libopcodes, which are host build tools, not shipped runtime
components.
Thirteen pinned Linux UAPI declaration headers are copied into the musl prefix;
host glibc headers are not added. Drift fails closed, with no installation or
fallback inside the builder. These package identities and their maintainer hooks require separate fresh
hosted prerequisite admission. The workflow verifies downloaded DEB sizes/hashes,
uses a root-owned private cache and bounded `dpkg --install` without dependency
resolution or force flags, then requires `apt-get --no-download check` and all
634 hashes before building. The new libbinutils package activates the existing
host ldconfig trigger only on that fresh runner. Base interpreters/coreutils
and other dynamic compiler dependencies remain the standard Ubuntu24 host
environment; 18/634 is not a hermetic or closed dynamic-loader graph claim.

The recipe has a 1200-second total budget and two build jobs. Every command uses
the existing owned, bounded process runner, private HOME/TMPDIR, no credentials,
no loader override and no writes under `/usr`. Squashfuse is rebuilt with all
three upstream validation fixes 67c9bb8/e1f8f3c/58330d6; its actual malformed-image
target must run and return 0. musl receives the official iconv, qsort and regex
security backports recorded in the manifest, plus Ubuntu's static-PIE wrapper
fix. zlib receives upstream df84af2 for CVE-2026-85091. Those source corrections
are not proof that the produced binary or installed behavior passed.

The builder requires a bounded x86-64 static PIE with no interpreter or dynamic
DT_NEEDED, non-executable stack, no writable/executable load segment and the
reserved 16-byte MD5 slot. A final link map binds the actual freshly built
libraries and pinned GCC objects. Before executing extraction, the packaged
runtime prefix must match every source-built byte except that non-code MD5 slot.
The unchanged deployment tools have their own pre-existing embedded runtimes;
this proposal does not repair or reclassify those tooling payloads.

The original `packaging/linux/AppRun` is a small AGPL shell launcher. It derives
an absolute AppDir, rejects a missing/nonexecutable/symlinked target, and `exec`s
the normal binary with unchanged arguments, exit status and signals. It does
not set `LD_LIBRARY_PATH`. Linuxdeploy's ordinary relative ELF RUNPATHs supply
library lookup. The previous opaque AppRun asset is not shipped or trusted.

The pinned output plugin receives the freshly source-built runtime through
`LDAI_RUNTIME_FILE`. After deployment both tool assets, that exact runtime and
the source launcher are checked again.
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

- Final compiler-runtime corresponding-source and exception compliance.
- Complete Rust and vendored static dependency notices and corresponding sources.
- Corresponding sources for bundled system packages.

All seven original source archives, their license notices, local security
patches, complete recipe/host identities and actual normalized build commands
are staged inside the candidate. Static libfuse is LGPL-2.1: its prominent
notice, complete source and patch, and the actual unstripped runtime work object
(`runtime-relink.o.gz`) permit rebuilding and relinking with a modified library.
The build receipt binds the shipped notices, object and resulting runtime; it
does not certify legal release compliance. musl and mimalloc are MIT, Squashfuse
is BSD-2-Clause, zlib uses its permissive license, and zstd offers BSD/GPLv2
terms (the recipe records its BSD notice and both license files). Compiler
runtime, broader product sources, signing and distribution remain release gates.
The runtime finding stays open until the patched build and actual installed
Linux checks pass. No release,
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


## Generic and update integration proposal

The generic route requires exactly `--formats appimage` plus the reviewed
`--gitleaks` path; default, all, mixed and DEB formats are not silently remapped.
It replaces its controller with the canonical isolated Python entry so the
existing signal/cleanup handling remains installed. The QA image handoff uses a
same-directory, no-overwrite atomic link into an owned empty 0700 `dist`, verifies
the final bytes/size/digest, and is removed on a later reporting failure. It is
not a release artifact. Normal direct capture invocation keeps its installer
inside the discarded private root.

The update builder runs only in the existing checked PID namespace on the public
owner-dispatched standard Ubuntu24/x64 context. It authenticates the exact clean
reviewed checkout and compares the complete bounded Git archive to its private
export, allowing only the two existing app-version substitutions. Public compile
inputs are limited to the exact `updateqa.1/.2` versions, HTTPS
`127.0.0.1:<port>/feed.json`, and the official signer's bounded minisign public-key
box. No caller flag alone grants export authority; no private key, token, loader
or compiler override enters the build environment. Source identity does not
protect against an owner replacing its filesystem.

Every C child starts with private umask077 while preserving the existing owned
process groups and deadlines. This makes generated Autoconf logs satisfy the
unchanged private-log scanner policy; it does not chmod or suppress findings.
The 3300-second per-image and 6900-second update whole-run bounds remain exact.
All source/build/runtime/capture/update results for this composed proposal remain
unexecuted; the old runtime vulnerability, unchanged deployment-tool runtime
exposure, complete release licensing and installed compatibility gates are not
waived.

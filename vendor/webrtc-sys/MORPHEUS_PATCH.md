# Native SDK admission and Linux PipeWire symbol isolation

This directory vendors the original `webrtc-sys` 0.3.35 crate, Apache-2.0,
from crates.io. Original source copyrights and `NOTICE.md` are retained; a
complete Apache-2.0 license is included as `LICENSE-APACHE`. Registry metadata
and the nested generated lockfile are omitted. Upstream crate SHA-256:

```text
070ad07ed7f1f6a296d8b4bd7fdf14d6ab69c356aa4670c74e25abc459b3e7a1
```

Morpheus changes `build.rs`, adds the `sha2` build dependency, and adds
`pipewire_isolation.rs` plus `pipewire-symbols.txt`. The wrapper also changes
native logging in `src/webrtc.cpp`, `include/livekit/webrtc.h`, `src/webrtc.rs`
and the start of the `KeyProvider` constructor in `src/frame_cryptor.cpp`.
Audio lifetime and enumeration changes are in `src/adm_proxy.cpp`,
`include/livekit/adm_proxy.h` and the controller header, C++ and Rust bridge.
The sections below describe these changes and their verification limits.

## Prebuilt cache admission

Whenever the native build script runs, it calls the pinned `webrtc-sys-build`
helper on prebuilt cache hits as well as misses before using the SDK. An
existing cache must pass that helper's receipt and directory-metadata checks.
A missing cache uses the helper's existing bounded, checksum-verified download
and extraction.
The receipt records download provenance; it does not authenticate the mutable
cached library or header contents, and a cache hit is not a fresh download.

Existing explicit `LK_CUSTOM_WEBRTC` paths retain their upstream opt-in behavior
on platforms that support them. These custom builds are outside the archive
pins and release acceptance. Linux's existing isolation policy still rejects
custom SDK paths and independently verifies the linked static library below.

## Why this patch exists

The pinned `webrtc-sys-build` 0.3.18 downloads LiveKit's
[`webrtc-51ef663` Linux x64 release archive](https://github.com/livekit/rust-sdks/releases/download/webrtc-51ef663/webrtc-linux-x64-release.zip).
Its generated PipeWire dlopen stubs export 33 weak public `pw_*` functions.
They jump through pointers populated by WebRTC's own stub initialization.
They can interpose on the ordinary system PipeWire references in
`pipewire-sys` 0.10.1; a direct Rust `pw_init` call can then reach an
uninitialized WebRTC stub. Renaming only `pw_init`, or only the definitions,
would leave other collisions or incorrectly reconnect WebRTC's callers.

Source inspection and read-only archive disassembly establish this collision
mechanism. The earlier hosted capture test reported SIGSEGV during
`opening_display_1`, after portal startup, but did not identify the final
executable's binding. This patch is not evidence that that crash is fixed.
The hosted binding-origin and actual capture tests must establish that.

## Exact reviewed input and transformation

Official release ZIP SHA-256 (also reported by GitHub's release asset API):

```text
dce9d9414cd8e2cc9a471c054481851b257ad2a879112ce5be56369b4fefdaed
```

Extracted `lib/libwebrtc.a` SHA-256, enforced at every build:

```text
9e377235c75d5f8992202187216187644b00a1bc71aa05f93841f8559b09281a
```

Read-only inspection found a normal, non-thin ar archive with 3,578 object
members, all ELF64 little-endian x86_64 `ET_REL`. No member has GNU LTO,
LLVM bitcode or LLVM LTO sections. The two additional ar records are its
symbol index and long-name table. The exact hash binds this reviewed format;
unknown future archives fail rather than partially rewriting unknown members.
There are 3,272 `.llvm_addrsig` sections (type `0x6fff4c03`). It is the only
LLVM/OS extension section type in the reviewed archive: there is no
`SHT_LLVM_CALL_GRAPH_PROFILE` or other LLVM table containing symbol indexes.

`pipewire-symbols.txt` records all 33 weak definitions and all 46 undefined
references, including their five archive member names. Its SHA-256 is
`cde3c3dfe5d685a66f53ab9aee63fc1047e18279e2406360ab559889870bb6f3`.

The build script:

1. Accepts only native `x86_64-unknown-linux-gnu` with the pinned release
   archive. Upstream selects release WebRTC for both Cargo dev/test and release
   profiles. `LK_DEBUG_WEBRTC=true`, `LK_CUSTOM_WEBRTC`, other Linux targets,
   future tags or unknown archive bytes fail closed. macOS/Windows use the
   upstream native build path with the shared cache admission above; the
   isolation module and its calls are compiled only on a Linux build host.
   A Linux target on another host is explicitly refused.
2. Copies the archive into this build's `OUT_DIR`, verifies its hash and exact
   PipeWire inventory with GNU `nm`, and writes a fixed mapping from every
   `pw_*` stub to `morpheus_webrtc_pw_*`.
3. Runs GNU `objcopy --redefine-syms` on the **whole archive**, with distinct
   input/output paths, so every internal reference changes with its definition.
   It does not change the `dlsym` string literals used to locate the real
   system functions. No private ABI calls, alternate loader flags or preload
   hooks are added.
   The same command removes exactly `.llvm_addrsig`, and GNU `readelf`
   verifies its absence. This optional table contains symbol indexes which
   objcopy may reorder. The [LLVM format specification](https://llvm.org/docs/Extensions.html#sht-llvm-addrsig-section-address-significance-table)
   defines absence as treating every symbol as address-significant; it can
   reduce safe identical-code folding but preserves address uniqueness.
   [LLD's source](https://github.com/llvm/llvm-project/blob/llvmorg-18.1.8/lld/ELF/InputFiles.cpp#L707)
   explicitly describes the objcopy index-invalidation risk. No code/data or
   relocation sections are intentionally removed. Global-symbol equality is
   not a substitute for the required actual linked RTC/media tests.
4. Compares the complete before/after external symbol inventories by member,
   name, type, address and size. Only the 79 expected definition/reference
   names may differ; all unrelated symbols must match. Rechecks the input hash
   and publishes the isolated archive only after validation.
5. Emits **only the isolated directory** as Linux's WebRTC link search path.
   The upstream download cache is never modified or offered as a link fallback.

Linux builds need `/usr/bin/nm`, `/usr/bin/objcopy` and `/usr/bin/readelf`
from GNU binutils.
The transformation is transparent source-controlled build logic; no patched
binary archive is checked into the repository. Upstream's bundled third-party
license requirements still apply to distributed binaries.

## Verification and limits

The Linux RTC unit-test module imports the same pure inventory/parser
contract tests used by the build script. Tests reject a definition-only
rewrite, a missing reference and an unknown future symbol. Build-time checks
also inspect the real transformed archive before linking.

Before its first PipeWire initializer, the actual native Wayland test uses
`dladdr` on all 33 linked Rust function addresses. It requires every address
to belong to the ordinary root-owned system `libpipewire-0.3.so.*` under
Ubuntu's `/usr/lib/x86_64-linux-gnu`; executable stubs, other libraries and
unavailable origins fail. Only fixed origin categories/counts leave the
fixture, never loader paths or addresses. The actual stream/cancellation
checks then remain mandatory. This fixture-specific path policy does not
change the application's runtime loader behavior.

At patch preparation, no local object rewriting, linking, native build or
capture run was performed. Hosted build/link/origin/capture evidence remains
required, as do the separate physical-device and compositor release gates.
Re-review the archive and remove or adapt this patch when upstream eliminates
the public stub collision; never broaden the digest check as a fallback.

## Factory audio lifetime across calls

`src/adm_proxy.cpp` keeps its platform and synthetic audio devices initialized
for the lifetime of the factory-owned proxy. A WebRTC voice engine borrows that
proxy: it stops recording and playout and detaches its callback before invoking
`Terminate()`. The proxy's matching `Init()` leaves the already initialized
devices in place. Final sub-device termination remains in `~AdmProxy()`.

Previously the engine's `Terminate()` destroyed the synthetic audio pump and
terminated the platform device while `Init()` remained a no-op. Releasing the
last peer connection could consequently leave the next call silent or return
negative device counts. The fix preserves audio device selection and mode state
across calls; it does not retain an idle peer connection or keep capture running.
See the [upstream lifecycle report](https://github.com/livekit/rust-sdks/issues/1468).

Verification must exercise repeated last-peer destruction with the same factory,
then actual call recovery and native playback. A successful first call is not
sufficient evidence for this patch. Physical device and package release checks
remain separate prerequisites.

The existing controller bridge test checks synthetic initialization directly
before and after two last-peer releases; the general initialized query retains
its original synthetic-or-platform meaning. These flags do not prove decoded
audio or native-thread quiescence. Device enumeration clamps negative native
errors to zero before higher Rust layers convert counts to unsigned values.

The normalized Cargo manifest omits the unused `env_logger` dev dependency so
this existing bridge test uses the already locked dependency graph. No source
references it; the original `Cargo.toml.orig` is preserved for provenance.

## Native log severity boundary

The pinned native archive retains key-derivation log strings, and the observed
upstream implementation emits such records at INFO. Full source-to-binary
attribution remains open. The wrapper disables its independent debugger/stderr
logging route once, before
any wrapper KeyProvider, runtime or log sink is constructed. Every wrapper log
sink registers at WARNING and also forwards only WARNING/ERROR records. No
message-content filter is used, and the existing Rust callback mapping is
unchanged. This does not remove the underlying native logging instructions or
prove that a debug-enabled application previously disclosed key material.

The fixed synthetic regression in `src/webrtc.rs` constructs a KeyProvider
and derives a disposable key before its sink, checks the actual debug setting
and stream threshold, and exercises both RTC_LOG and the forwarding guard.
CXX 1.0.203 cannot condition its bridge on cfg(test), so the fixed-message C++
test shims and private Rust bridge are compiled in ordinary builds; only the
Rust cfg(test) case calls them. They accept no message/key input, expose no
external Rust API, and have no production callers. The test does not reset
global logger state; source ordering covers provider-first construction, while
a fresh exact test process can additionally observe that order.

Upstream libwebrtc 0.3.38 maps every forwarded native severity to Rust DEBUG;
this patch preserves that mapping rather than promoting warnings to WARN.
Other external C++ users could still reset global logger settings or install
independent sinks; no current Morpheus wrapper/caller exposes such a reset.
A no-logging native build is not validated by this regression, which requires
actual WARNING/ERROR callbacks. Existing key algorithms, encryption, TLS trust
and SIF payload handling are unchanged. Their separate native-source and
release acceptance requirements remain open. No native code was executed
during preparation of this source proposal.

## Notices from the pinned native archives

The four `LICENSE.libwebrtc-*.md` files preserve the unmodified `LICENSE.md`
member of each supported pinned native release archive. They add the native
component notices to the existing wrapper attribution. Source-based packaging can include these committed license files without
executing the archives. Their presence does not prove that a particular
installer carries the notices.

Each source archive is from the `webrtc-51ef663` release, authenticated against
`vendor/webrtc-sys-build/src/pins.rs` before reading its notice. The exact inputs
are recorded below.

- `LICENSE.libwebrtc-linux-x64-release.md`

  Source: [linux-x64-release/LICENSE.md](https://github.com/livekit/rust-sdks/releases/download/webrtc-51ef663/webrtc-linux-x64-release.zip)

  Archive: `149683670` bytes, SHA-256
  `dce9d9414cd8e2cc9a471c054481851b257ad2a879112ce5be56369b4fefdaed`.

  Unmodified notice: `176356` bytes, SHA-256
  `babbfff10dadd0c657bde5e5e2b83eece9ef8cee4e3b35d1c2db514a84587284`.

- `LICENSE.libwebrtc-mac-arm64-release.md`

  Source: [mac-arm64-release/LICENSE.md](https://github.com/livekit/rust-sdks/releases/download/webrtc-51ef663/webrtc-mac-arm64-release.zip)

  Archive: `265849959` bytes, SHA-256
  `f9be49b9fee9cd1588da8a40c85bda21f5ea49c4ed82662dc889b1c919569a08`.

  Unmodified notice: `178927` bytes, SHA-256
  `f85e1d2d1706111522d29b3cdfc2c180bd1afd3d6735901b1e6b9e420a731163`.

- `LICENSE.libwebrtc-mac-x64-release.md`

  Source: [mac-x64-release/LICENSE.md](https://github.com/livekit/rust-sdks/releases/download/webrtc-51ef663/webrtc-mac-x64-release.zip)

  Archive: `223690324` bytes, SHA-256
  `171f1ba0866e5947e0d420ecc1c33814476e1919131b81bb87c8729d45d72127`.

  Unmodified notice: `212865` bytes, SHA-256
  `1ded950864279f62b36a98de2b7b15f9ef38c0555dd1d1c3484d3b9f724a873f`.

- `LICENSE.libwebrtc-win-x64-release.md`

  Source: [win-x64-release/LICENSE.md](https://github.com/livekit/rust-sdks/releases/download/webrtc-51ef663/webrtc-win-x64-release.zip)

  Archive: `110167699` bytes, SHA-256
  `0a56a5c91b3b7b4222082b8a09388d16a802cde22d67295beb24c55700755b30`.

  Unmodified notice: `197167` bytes, SHA-256
  `7b039d5e54336caa5bbb4d228ded62db5f686d9d3258def87c2146eeaa3cf5ef`.

These notices identify the pinned archive texts. They do not establish which
archive or custom build supplied a particular package, complete matching native
preferred source, or permission to distribute a release. Custom SDK builds need
their own notice and source evidence. The existing linked-input, source,
relinking, attribution and distribution checks remain required.

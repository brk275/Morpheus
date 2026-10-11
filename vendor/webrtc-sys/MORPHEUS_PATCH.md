# Linux PipeWire symbol isolation

This directory vendors the original `webrtc-sys` 0.3.35 crate, Apache-2.0,
from crates.io. Original source copyrights and `NOTICE.md` are retained; a
complete Apache-2.0 license is included as `LICENSE-APACHE`. Registry metadata
and the nested generated lockfile are omitted. Upstream crate SHA-256:

```text
070ad07ed7f1f6a296d8b4bd7fdf14d6ab69c356aa4670c74e25abc459b3e7a1
```

Morpheus changes `build.rs`, adds the `sha2` build dependency, and adds
`pipewire_isolation.rs` plus `pipewire-symbols.txt`. The Rust/C++ transport,
cryptography, audio, video and capture source remain upstream code.

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
   future tags or unknown archive bytes fail closed. macOS/Windows keep the
   original build path; the isolation module and its calls are compiled only
   on a Linux build host. A Linux target on another host is explicitly refused.
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

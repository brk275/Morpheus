# Verified Windows packaging tools

The ordinary Windows package job uses Cargo Packager **0.11.8** and its NSIS
backend. Before packaging, `scripts/windows_package_tools.py` prepares a fresh
NSIS tool directory with the SHA-256 pins in `scripts/windows_package_tools.json`.
The helper is limited to the standard Windows hosted `brk275/Morpheus` main job.
It never runs downloaded tools, modifies an existing NSIS tree, or uses signing
credentials. The next packaging step runs the tools after this verification.

## Why a preseed is required

Packager's `src/package/nsis/mod.rs` verifies the NSIS 3.09 archive and
`nsis_tauri_utils` 0.2.1 DLL using fixed SHA-1 hashes. Its ApplicationID archive
uses an unhashed download, and its cache validation only hashes the Tauri DLL.
The preseed verifies all three inputs with SHA-256 before opening any archive.
Only `ReleaseUnicode/ApplicationID.dll` is selected from the ApplicationID ZIP;
the full pinned NSIS layout is retained. Required cached-path files are checked
against individual hashes, preventing Packager from entering its download path.

The source excerpt in `nsis-cached-path-contract.txt` records Packager's exact
Windows cache requirements. Source/archive hashes are recorded in the pins.
Tests tie those requirements and the workflow install version/order together.
A Packager upgrade requires a new review of this contract and its downloads.

## Cache ownership and limits

Packager's `src/package/context.rs` uses `dirs::cache_dir()/.cargo-packager/NSIS`.
On Windows, `dirs` 5.0.1 resolves the Local AppData known folder. The helper uses
the same Windows API, not an environment-variable override. It refuses an
existing NSIS path and rejects symlink/junction parents. Files are staged in a
new owned directory, checked, and renamed into place. No cache migration or
recursive deletion of an existing tool tree is performed.

Downloads have exact size/hash pins, a 16 MiB ceiling, bounded reads/deadline,
and HTTPS-only redirects to GitHub asset hosts. ZIPs are limited to 512 entries,
16 MiB per file and 32 MiB total. Traversal, absolute paths, Windows aliases,
case collisions, links, special files and file/directory conflicts are rejected.
Only fixed result metadata is logged; download errors cannot reflect signed
redirect URLs. The ordinary package workflow still uploads no installers.

## Source and release provenance

Inputs were reviewed as inert data from the URLs used by the pinned Packager:

| Input | Upstream release / asset | Bytes | SHA-256 |
| --- | --- | ---: | --- |
| NSIS archive | `tauri-apps/binary-releases`, `nsis-3.9`, asset `124641645` | 2354828 | `f5dc52eef1f3884230520199bac6f36b82d643d86b003ce51bd24b05c6ba7c91` |
| ApplicationID archive | `tauri-apps/binary-releases`, `nsis-plugins-v0`, asset `87497806` | 2802408 | `1c2772b0edfb0f96a7524734d6c8fac1fc011f26221faf88f3ed2c950f0c06c0` |
| Tauri NSIS plugin | `tauri-apps/nsis-tauri-utils`, `nsis_tauri_utils-v0.2.1`, asset `121974030` | 1188352 | `0eed48313a7f904d7cc1977b70000ab3f11f18cadc8e6a69b807d288ca71f9db` |

The NSIS archive and Tauri DLL also matched Packager's existing SHA-1 pins.
The ApplicationID Unicode DLL SHA-256 is
`f6851dcbf0a39edecd8a46564bc455e5273736c3dbcb02b954c201c79ccdf117`.
The upstream asset API provided no digest for these older assets; the recorded
SHA-256 values identify the exact reviewed downloads. They establish repeatable
content identity, not a new upstream signature or a claim of reproducible builds.

These are build-tool checks. Installed NSIS QA, code signing, real devices,
Narrator, account-vault behavior, update recovery and release approval remain
separate gates. No local execution is implied by the source review.

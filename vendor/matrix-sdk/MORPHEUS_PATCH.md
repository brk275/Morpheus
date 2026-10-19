# Morpheus patch: typed search entry points for matrix-sdk

STATUS: selected through the root
`[patch."https://github.com/matrix-org/matrix-rust-sdk"]` table.
Locked metadata and 14 low-level search-index tests passed for source
`3cb8cb2af7be64905fd5cae51aaf3957ab60ab08` in hosted run `39275386796`.
The high-level SDK, application and platform checks remain incomplete.
The diagnostic-formatting change below still needs its focused regression.

## Upstream

- Repository: https://github.com/matrix-org/matrix-rust-sdk
- Revision: c6ad52ecc09eaa36601f55effb0a48a533eb87dd, the revision
  `Cargo.lock` pins for `matrix-sdk 0.19.1`.
- Source: `crates/matrix-sdk` at that revision, copied read-only from the
  local cargo git checkout. The cargo cache was not modified.
- License: Apache-2.0. `LICENSE` is the repository-root license at the same
  revision; upstream headers are kept.
- `MORPHEUS_PROVENANCE.json` lists all 227 upstream regular files with their
  sha256, git blob ID and size, recorded before patching.
  `Cargo.toml.orig` is the upstream manifest.

## Omitted upstream entry (one, explicit)

The upstream crate has 228 entries: 227 regular files and one symlink.
The symlink is omitted from this copy:

- path: `.cargo/mermaid.html` (crate-relative)
- mode: 120000 (symlink)
- git blob: 3d84ee221773b90e809c1841e654b3ed4164ba69
- stored target: `../../../.cargo/mermaid.html`

It is an unused, docs-only link to the upstream repository's rustdoc
mermaid include. The first vendoring copy carried it by mistake; it was
then removed with `git rm` and was never followed or opened. Nothing is
excluded by pattern or scanner suppression. The manifest's upstream
`include` list still names `.cargo/mermaid.html`, which only affects
`cargo package` and not a path dependency's build. The 227 regular files
match their recorded sha256 except the declared patches (`Cargo.toml` is
the manual manifest; `Cargo.toml.orig` matches upstream).

## Manifest (hand-normalized, resolution checked)

`Cargo.toml` was written out by hand from `Cargo.toml.orig` and the
upstream workspace `Cargo.toml` at c6ad52e:

- `rust-version` is 1.96.
- Each `workspace = true` dependency takes its workspace version,
  default-features and features, plus the member's own features and its
  `optional` flag.
- Internal crates (matrix-sdk-base, -common, -ffi-macros, -indexeddb,
  -search, -sqlite, -test, -test-utils) become git dependencies at the
  pinned revision, so the root `[patch]` table redirects base, ui and
  search to `vendor/`.
- `[features]` is copied verbatim.
- Lints are copied from the normalized vendor/matrix-sdk-ui manifest
  (same workspace lints).
- `build.rs`, `tests/` and `uniffi.toml` are kept as upstream.

The effective dependency versions, defaults and features were compared with
the pinned upstream workspace. Hosted run `39272950150` produced a lock
proposal; its new package archives, checksums, hooks and selected test
dependencies were independently reviewed before adoption. Locked resolution
then passed in run `39275386796`. This does not establish runtime or platform
compatibility.

## Changes (narrow)

- `src/search_index/mod.rs`: explicit diagnostic formatting for the search
  store kind, with its credential field redacted. A focused regression also
  checks formatting through the parent index and acquired guard.
- `src/search_index/mod.rs`: `SearchIndexGuard::search_filtered(query:
  Option<&str>, &SearchFilter, max, offset, room_id)`. It mirrors the
  existing `pub(crate) search`, creating the room index when missing, and
  calls `RoomIndex::search_filtered` from vendor/matrix-sdk-search. It is
  `pub`, with an import line added.
- `src/message_search.rs`:
  * `pub use matrix_sdk_search::index::SearchFilter` (next to the existing
    `SearchResult` re-export).
  * `Room::search_filtered(query, &filter, max, offset)` next to
    `Room::search`.
- Since the third staging commit, newest-first dated windows for the
  all-room filters-only merge: `SearchIndexGuard::search_newest(filter,
  max, offset, room_id)` and `Room::search_newest(&filter, max, offset)`.
  They return `Vec<(MilliSecondsSinceUnixEpoch, OwnedEventId)>` from
  `RoomIndex::search_newest`. Morpheus merges the per-room windows
  (bounded, one room at a time) in morpheus-matrix search.rs, so the SDK
  delta stays narrow, and the plain word builder (`search_messages`) is
  unchanged.


## Cargo.lock

The `[[package]]` entries `matrix-sdk 0.19.1` and `matrix-sdk-search 0.19.1`
lost their
`source = "git+https://github.com/matrix-org/matrix-rust-sdk?rev=c6ad52e...#c6ad52e..."`
line and are now path packages, like the vendored matrix-sdk-ui entry.
Their SDK test dependencies were subsequently resolved by the hosted metadata
proposal and reviewed before adoption. Existing package versions, source
identities and registry checksums stayed pinned. No unlocked compilation was
used.

## PERSISTENCE COHORT

`crates/morpheus-app/build.rs` hashes `vendor/`, so this crate and
vendor/matrix-sdk-search change `MORPHEUS_PERSISTENCE_COHORT`.

## Original-date repair (high-level regression not yet accepted)

The resolved latest replacement carries the original cached message's normalized
timestamp through adds, edits and edit redactions. Replacement indexing reads
`m.new_content`, not the potentially different fallback body. This corrects
index-side lower/upper date bounds and newest-first ordering. Regression source
covers older/later edits, edit-before-original delivery, fallback disagreement
and edit redaction. The app also uses normalized loaded timestamps.

The existing persistence cohort must reject older edit-date stores; an actual
upgrade/reindex path remains a separate release gate. Do not reuse the previous
cohort or claim migration coverage. Source and dependency review, focused hosted
execution, and actual encrypted-store upgrade acceptance are distinct gates.
The original contributor worktree remains preserved.

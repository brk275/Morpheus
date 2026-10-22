# Morpheus search patch

## Upstream and provenance

- Repository: https://github.com/matrix-org/matrix-rust-sdk
- Revision: `c6ad52ecc09eaa36601f55effb0a48a533eb87dd`.
- Crate: `matrix-sdk-search 0.19.1`, from `crates/matrix-sdk-search`.
- License: Apache-2.0; `LICENSE` and source copyright headers are retained.
- `MORPHEUS_PROVENANCE.json` records all 14 upstream regular files, with
  original sizes, SHA-256 hashes and Git blob identities. No upstream entry
  is omitted; there are no symlinks or generated executable files.
- `Cargo.toml.orig` is the unchanged upstream manifest. `Cargo.toml` writes
  out workspace-inherited versions, features and lints from the same pinned
  upstream workspace. Its effective dependencies were independently compared
  with upstream before hosted resolution. The Cargo cache was not modified.

Only `Cargo.toml`, `src/index/mod.rs` and `src/schema.rs` differ from the
recorded upstream files. The matching SDK wrapper is `vendor/matrix-sdk`.
Both are selected by the root Cargo patch table.

## Search behavior

- `SearchFilter` carries typed sender, lower-date and upper-date bounds.
  Diagnostic formatting redacts filter values.
- `RoomIndex::search_filtered` combines a typed sender `TermQuery` and date
  `RangeQuery` with optional parsed search words. Filter values never become
  query-parser text. Word results retain relevance ordering; filter-only
  results are ordered newest first.
- `search_newest` returns a bounded newest-first window with indexed dates.
  The application merges these windows across every joined room using one
  heap head per room and cooperative yields.
- Dates retain second precision in the index. Loaded results recheck exact
  millisecond bounds. Oversized dates are capped before nanosecond conversion.
- The matching SDK patch uses the original message's normalized date and a
  replacement's `m.new_content`, excluding invalid or redacted replacements.

## Redaction recovery and index compatibility

The `original_event_id` field is now both indexed and stored. The bounded,
exact-term `RoomIndex::original_event_id` lookup recovers a replacement's
original ID after cache redaction has removed `m.relates_to`. It also observes
pending additions/removals. Missing or malformed stored mappings report a
fixed repair error rather than guessing an original ID.

The SDK uses that mapping to restore the newest surviving valid version.
Redacting the original still removes its replacement; a redacted message is
never restored from its old indexed body.

This changes the stored index schema. Tantivy's normal `open_or_create`
refuses an existing mismatched schema rather than overwriting it. Existing
indexes require an explicit encrypted-generation rebuild, retaining the old
generation until the replacement is complete and approved. The application's
persistence cohort must remain enforced. No automatic profile upgrade,
backward compatibility or lossless migration is claimed by this patch alone.

## Verification status

Source `3cb8cb2af7be64905fd5cae51aaf3957ab60ab08` passed locked resolution and
14 low-level index tests in hosted run `39275386796`. Its high-level SDK
redaction regression failed in run `39276398829`, exposing the stripped
replacement relation. That historical result does not verify the new stored
mapping or redaction repair. Those changed inputs require fresh focused
checks, followed by the affected native platform and encrypted upgrade gates.

## Formatter inheritance

The regular `.rustfmt.toml` is copied byte-for-byte from the pinned upstream
repository root; it restores settings these crates inherited before vendoring.
No formatting checks are disabled. Stable formatting normalizes local patch
layout; the SDK event-cache redecryptor doc comment also loses one trailing
space. Original source inventories remain unchanged in the provenance record.

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
- Redaction handling resolves an indexed replacement's original ID before
  parsing the already-redacted cache entry, restores the latest remaining valid
  edit, and excludes the redacted event explicitly. Redacting the original
  removes its replacement too. The original message date remains unchanged.
  Valid replacements are selected by normalized timestamp and then event ID,
  so out-of-order arrival cannot select an older edit.
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

The redaction repair also requires the search crate's stored original-ID field.
Older search schemas must be rebuilt through the explicit encrypted-generation
upgrade path. Normal opening refuses a schema mismatch; it must not delete the
old generation or bypass the profile's persistence cohort.

## Optional per-request response-body limit (bounded SDK responses)

STATUS: source only. Nothing has been built, formatted by tool, tested or run;
root reviews the change and runs the affected SDK checks on fresh hosted runners.

Why: upstream `http_client::response_to_http_response` reads every response
with `reqwest::Response::bytes()`, so a homeserver can return an arbitrarily
large body (for example a device list or a user-interactive-auth response)
before any application cap applies after deserialization.

What changed, narrowly:

- `RequestConfig::max_response_bytes(impl Into<Option<u64>>)` sets an optional
  limit (field `max_response_bytes: Option<u64>`, default `None`, shown in the
  config's `Debug`). `None` keeps upstream behavior exactly: the body is read
  whole, so sync and media requests are unaffected unless a caller sets a
  limit. There is no blanket limit.
- `HttpError::ResponseTooLarge { limit: u64 }`: a fixed message naming only the
  limit (no body, URL or token). `HttpError::retry_kind` classifies it as
  `Permanent`, so the native backoff never retries it, and it is not an API
  error, so the unknown-token refresh path never re-sends it.
- `response_to_http_response(response, max_response_bytes)` now returns
  `HttpError`. With a limit, `BoundedBody` refuses a declared length over the
  limit before reading, then counts each decoded chunk (after gzip
  decompression) with checked arithmetic before the buffer grows. The buffer is
  requested to grow geometrically without requesting capacity past the limit;
  allocator overhead, transport/decoder buffers and the current chunk remain
  additional memory. This bounds accumulated body bytes, not process memory.
  A declared length is trusted for at most a 64 KiB first allocation. Returning
  the error drops the response, which stops the transfer.
- Native `execute_request` takes the limit from the request's `RequestConfig`
  on every attempt; the wasm `send_request` passes it the same way.
- Wasm: the pinned reqwest has no chunked reader without its `stream` feature,
  which the wasm target does not enable. A declared length over the limit is
  refused before reading, but a body without one is checked after the browser
  read it (same error and no-retry, not a memory bound). Morpheus runs native
  only. Enabling `stream` for wasm is a manifest/feature change left to root.
- Token refresh, authentication, timeouts, the concurrency semaphore, the send
  progress observable, cancellation by dropping the future and the app's
  redirect policy are unchanged. No request or response body is logged; the
  existing span still records only the response size.

Public seam for callers (root's device/session manager applies it): send the
Ruma request through `Client::send(request).with_request_config(
client.request_config().max_response_bytes(LIMIT))`, for example
`get_devices::v3::Request`, `update_device::v3::Request` and
`delete_devices::v3::Request` (including its user-interactive-auth retry with
`auth` set). The convenience methods `Client::devices`, `Client::rename_device`
and `Client::delete_devices` keep the client's default config, so a capped
caller uses `Client::send` directly. A refused response is
`HttpError::ResponseTooLarge { limit }` (or `matrix_sdk::Error::Http` wrapping
it).

Regression source (in `src/http_client/mod.rs` tests and
`src/config/request.rs` tests): under, at and over the bound; declared
oversize; understated or unknown length counted as bytes arrive; checked
overflow and no reservation past the bound or for a refused chunk; permanent
classification and redacted message; per-request limit through `Client::send`
with a mock that would see any retry; a dropped request that leaves nothing
running; and the unlimited default. The integration case uses a mock with a
declared length (wiremock sends one); the streamed path is covered directly
through `BoundedBody`; a root-added native loopback HTTP fixture additionally
exercises a chunked response with no Content-Length through reqwest.

Additionally modified vendored files (upstream c6ad52e blob -> current blob):

| File | Upstream git blob | Current git blob |
| --- | --- | --- |
| src/config/request.rs | c2a8e3067413203b49221fd1b2d685aa19cb5340 | 33b48c41bfc8cc2d2d5485a03029becb6f379342 |
| src/error.rs | 41b387d55ad3184cbbc0daee68f4ef3d5c4b0ba4 | 1e5d866fcf045096893095a1d4ce671f8a64d810 |
| src/http_client/mod.rs | 9224c8826604f4e8b8e670806efef111ba93edd6 | 75970fc6ff29dd0ac480fe9da736459a3182c6b6 |
| src/http_client/native.rs | 58321de8f1ba7435a6dd22c93bef77437c22fad7 | ee5d9d819e909b7354c77e0b1fedc58e676d3ede |
| src/http_client/wasm.rs | f08346b3093a9dda63b30fcde81d2525cfdf0dc6 | 651e1ad551e97e805105249a586f4279d128a5fa |

The upstream blobs are the `git_blob` values recorded in
`MORPHEUS_PROVENANCE.json` before any patch; those records are unchanged.
`LICENSE` and the upstream headers are kept. No manifest, feature, lock,
build script or dependency changed. `crates/morpheus-app/build.rs` hashes
`vendor/`, so the persistence cohort changes with this patch.

Root integration adds an 8 MiB per-request cap to device roster reads and
rename/sign-out responses (including UIAA). Existing selection, operation,
password and cancellation fences remain intact. Oversized responses map to
the fixed unavailable result; ambiguous destructive requests are not resent.
Source only; no hosted or installed-device acceptance is claimed yet.


## Push-rule write responses

The pinned notification-settings command sender uses the existing per-request
response cap at 64 KiB, a 30-second timeout and no automatic mutation retry.
Its error messages use fixed operation categories; the shared HTTP error log
also omits the typed error body. Existing HTTP request spans are unchanged. Existing authenticated-client and redirect policies are
unchanged. Focused mock-server regressions cover oversized valid JSON and a
single failed request. A failed multi-command update may have partly reached
the server; callers must reconcile through sync and must not claim rollback.

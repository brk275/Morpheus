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

## Opt-in authentication refresh and replay suppression

STATUS: source only; no local build or execution. Focused SDK tests and the
affected native targets still require independently reviewed hosted runs.

`RequestConfig::disable_auth_refresh()` opts a request out of the SDK's
unknown-token handler. This is independent of HTTP backoff: `retry_limit(0)`
does not prevent the upstream `SendRequest` path from refreshing credentials
after a 401 and sending the mutation again. The new boolean defaults to
`false`, preserving all existing request behavior, and its diagnostic output
contains only the boolean. `SendRequest` checks the effective per-request
configuration, or the client default when none was supplied, before calling
the handler. An opted-out request returns its original 401 without refreshing,
replaying, or broadcasting the handler's unknown-token session change. The
original request is still authenticated; `send_inner` still marks its rejected
access token as expired. Another request may refresh the session normally.
The path builder's version discovery already uses its upstream failsafe mode
without token refresh; this patch does not change it.

Only the notification-settings mutating command helper opts out in this
patch, alongside its existing zero HTTP retries, 30-second timeout and 64 KiB
response cap. Callers implementing other uncertain mutations may explicitly
choose the same per-request setting after their own source review; ordinary
reads, sync, media, OAuth and authentication defaults are unchanged. The common
gate covers Matrix and OAuth unknown-token handling without altering either
refresh implementation. No dependency, manifest, lockfile, feature or unsafe
code changed. The SDK patch changes the existing persistence cohort hash.

Changed source files are `src/config/request.rs`, `src/client/futures.rs`,
`src/notification_settings/mod.rs` and
`tests/integration/refresh_token.rs`; their original upstream provenance remains
the pinned revision recorded above. The notification helper's response limit
and fixed operation logs are retained.

Regression source adds actual loopback mock-server cases for a create-room
request with automatic refresh enabled: opted out returns its original
unknown-token 401 after one request, emits no session-change signal, preserves
the fixture credentials, and logs neither token nor reflected reply text;
default behavior still performs one refresh and one successful replay even
with zero HTTP retries. A push-rule mutation has the same opted-out 401
regression, ensuring the production helper is wired to the setting. Existing
oversized-valid-JSON, 500 single-attempt and fixed-log push-rule regressions
are retained unchanged.

## Opt-in stored-members identity status stream

STATUS: source only, composed onto the current root with the IW07 and IW08
repairs below. Nothing has been built, formatted by tool, tested or run.
Root reviews the SDK change and every caller independently before any
hosted execution.

Why: `Room::subscribe_to_identity_status_changes` builds its state through
`Room`'s `RoomIdentityProvider`. Its `member_identities` and `is_member` call
`members()` and `get_member()`, which run `sync_members()` and send
GET `/rooms/{roomId}/members` whenever this device's member list is marked
incomplete. A caller's own `are_members_synced()` check cannot exclude that
request: the flag can change on another worker thread, or while the
subscription awaits the client's OlmMachine read lock, before the SDK's own
check. Its `is_member` also accepts any stored member, including one who
left or was banned, so a later identity update of a departed member is
reported again.

What changed, narrowly (opt-in; existing behaviour preserved):

- `src/room/identity_status_changes.rs`: a private `MemberSource` provider
  and a shared private `create_stream_from`, which holds the unchanged
  stream lifecycle (identity stream, member event handler, initial state,
  default per-change processing). `MemberSource::Room` delegates every
  method to `Room`'s existing `RoomIdentityProvider` implementation, which is not
  modified, so `create_stream` and `Room::subscribe_to_identity_status_changes`
  behave as before for every existing caller. `MemberSource::Stored`, used by
  the new `pub(crate) create_stream_no_sync`, reads membership only from this
  device's state store: `member_identities` uses
  `members_no_sync(JOIN | INVITE)`, and `is_member` uses
  `get_member_no_sync` and accepts only a Join or Invite membership.
  `user_identity` delegates to the existing local lookup
  (`Encryption::get_user_identity`, i.e. `OlmMachine::get_identity(user,
  None)`: crypto store only, no wait for a key query) for both sources;
  `state_of` keeps the trait default.
- `src/room/mod.rs`: `Room::subscribe_to_identity_status_changes_no_sync()`
  (`pub`), next to the existing method.
- A local read that fails is never taken for an empty answer (IW07).
  `RoomIdentityProvider` has no error path (it returns a list, `bool` or
  `Option`), so `MemberSource::Stored` carries a private `StoredReads`
  record of its first failed read. `member_identities` records a failed
  `members_no_sync`; `is_member` a failed `get_member_no_sync`; the stored
  source's `user_identity` calls the same `Encryption::get_user_identity` and
  records a failed read instead of answering `None`. After building the
  initial state, `create_stream_from` returns the recorded error for the
  stored source, so `subscribe_to_identity_status_changes_no_sync` fails
  rather than yielding a stream that reports nobody; after each later change
  it ends the stream if a read failed while processing it. There is no
  network fallback and no sync-flag, trust or key change.
  `MemberSource::Room` never records anything, so `create_stream` and
  `Room::subscribe_to_identity_status_changes` keep their upstream
  behaviour, including its error suppression.
- No public crypto `UserIdentity` exposure, no duplicated crypto logic, and
  no dependency, feature, lock, unsafe, manifest or lockfile change. No sync
  flag, trust, pin, send policy or key material is changed.

Network: every await of the no-sync path reads this device's stores only:
`Encryption::user_identities_stream` (OlmMachine read lock and the crypto
store's broadcast), the room member event handler registration,
`members_no_sync` and `get_member_no_sync` (state store) and
`get_user_identity` (crypto store). None calls `sync_members`,
`request_members`, a profile or keys-query API, or changes `members_synced`.
If the stored member list is incomplete, members missing from it are not
reported; the only caller keeps `are_members_synced()` for that truthfulness.

Caller: Morpheus `crates/morpheus-matrix/src/room_identity_warnings.rs`
only.

Regression source (tests in `src/room/identity_status_changes.rs`):

- With the member list not marked complete and the test's own non-pooled,
  recording mock server: the initial state, a later identity update and one
  after the list went from complete to incomplete are all reported, with
  zero member-list requests and the flag still unset.
- Contrast on the same setup: the syncing stream does request the member
  list, so the request count is not vacuous.
- A warned member who left (stored as Leave) is cleared, later identity
  updates while gone are not reported, and rejoining reports again.
- IW07: with the stored member list marked complete and the recording
  server, a known empty state (Bob pinned) starts and reports nobody; once
  Bob's stored member event cannot be read, the stored-members subscription
  returns an error. No member-list request is made and the flag stays set.
- IW07: on a running stored-members stream, an identity change of a member
  whose stored event cannot be read ends the stream instead of being dropped
  silently; no member-list request is made.
- IW08: pin and verification updates queued before a persisted Leave or Ban
  are reconciled when the stream is next polled. The old warning is removed,
  identity changes and stale Join events while departed do not revive it,
  and a valid active rejoin reports a new violation. A stale Leave before
  that final Join does not remove the current active warning. Each case
  keeps the member-list flag incomplete and makes zero member-list requests.
- IW08: a readable departure followed by an unreadable active member makes
  the rebuild end the stream before publishing a removal/all-clear, even
  with the member-list flag marked complete; no member list is requested.
- IW08 controls: the default syncing stream retains its queued pin/removal
  and historical-member behavior. A direct publication-filter test excludes
  our own ID from initial and incremental batches; it is not an end-to-end
  own-identity stream fixture.
- Test scaffolding only: `init` gains an `init_client` half and
  `create_room_with_other_member` an explicit `members_synced` flag; the
  existing tests' setups are unchanged in behaviour. IW07 adds
  `make_stored_bob_member_unreadable`, which writes one unreadable copy of
  Bob's member event into the test client's in-memory state store (whose
  membership index keeps Bob's Join), with no sync or request. IW08 reuses
  that mechanism for the own member to exercise a failed departure rebuild.

IW08 processing is private to the stored-members opt-in. Before processing
an event, it reads current local membership for affected retained identities.
An actual departure triggers one rebuild of the opt-in `RoomIdentityState`
from active stored members, and a before/after diff; a single rebuild also
coalesces other departures already persisted. Both membership checks and
the rebuild check the IW07 failure record before any result is published.
Stale queued membership events cannot add an inactive member or remove an
active rejoin. The default provider and its per-change state-machine call
are unchanged. No new crypto crate boundary, classification override, trust
write or network request is introduced. The diff uses the existing API's
documented `Pinned` removal signal for a departed user; that signal does not
assert or mutate the departed user's actual crypto state or trust.

Cost and cancellation: each opt-in event snapshots K retained identities:
Verified and both violation states, never Pinned (implicit/absent internally).
The snapshot takes O(K) space/time; ordered-map construction and diffs add
worst-case O(K log K) comparison work. A detected retained departure rebuilds
once with O(room-size) local membership/crypto reads and before/after maps.
Pending departures already reflected in the rebuilt state do not trigger repeated
rebuilds. The underlying SDK member vector and identity state remain uncapped;
the app's visible/retained warning caps are not an SDK memory bound. No new
task is spawned: dropping the stream drops the rebuild future and the owned
member-handler guard. Concurrent store updates are not an atomic snapshot;
subsequent queued events reconcile them. No runtime resource or cancellation
measurement is claimed here.

Additionally modified vendored files (upstream c6ad52e blob -> current blob):

| File | Upstream git blob | Current git blob |
| --- | --- | --- |
| src/room/identity_status_changes.rs | ab830b7fa437da3ef24e2b56474d81a6c2920744 | 8a32f7931a0e79fd26ebff767155a91c53889799 |
| src/room/mod.rs | f979dc97d1667d661c91c0fac34a26206fc15572 | cc6d1b828a3db334814f6bc2ea06ced35360af5a |

Current blobs are git blob IDs computed from the final file bytes (sha256
c1d9991c76dd98074cdb94ed3fcdc7bd9ef6ff6b36e37af2dff0b24f52a424a8, 66510
bytes, and 506306b392b09828d8b22a303424b8d8652b60d218899b0eda0b1dac4d051c6e,
227283 bytes); the same computation reproduces both recorded upstream blobs.
The upstream blobs are the `git_blob` values in `MORPHEUS_PROVENANCE.json`;
those records are unchanged. `LICENSE` and the upstream headers are kept.
`crates/morpheus-app/build.rs` hashes `vendor/`, so the persistence cohort
changes with this patch.

## Formatter inheritance

The regular `.rustfmt.toml` is copied byte-for-byte from the pinned upstream
repository root; it restores settings these crates inherited before vendoring.
No formatting checks are disabled. Stable formatting normalizes local patch
layout; the SDK event-cache redecryptor doc comment also loses one trailing
space. Original source inventories remain unchanged in the provenance record.

## Generated secret-storage key handoff (recovery setup)

Validation uses the focused SDK regression below and native application
checks. Source review, formatting and hosted results are recorded separately;
this note does not certify release readiness.

Why: `Recovery::enable()` creates secret storage through `CreateStore`. That
future generates a new secret-storage key, uploads its description
(`m.secret_storage.key.<id>`), exports the cross-signing and backup secrets
encrypted with it, then makes it the default key
(`m.secret_storage.default_key`). Each of those requests can take effect on
the server and still return an error (for example, the server stores the
default key and the answer is lost). On any such error `CreateStore` fails and
drops the key, `Enable` returns before it sends `EnableProgress::Done`, and
the caller never sees the key, although the server may already point at it.
Morpheus cannot rebuild this flow from outside the crate: `SecretStore`'s
fields and `Recovery`'s backup and state steps are private.

What changed, narrowly:

- `secret_storage::GeneratedKeyHandoff` (new, public): a cloneable handle to
  one `Zeroizing<String>` slot. `take()` returns the key once. Its `Debug`
  output never contains the key. The lock is a std mutex; a poisoned lock is
  recovered, never a panic.
- `CreateStore::hand_key_to(handoff)` (new builder, public) and a private
  `key_handoff` field (`None` by default). When set, the key is put into the
  handoff right after it is generated, encoded as
  `SecretStore::secret_storage_key()` encodes it (base58), and before the
  first request. Without a handoff nothing differs from upstream.
- `Enable::hand_key_to(handoff)` (new builder, public) and a private
  `key_handoff` field (`None` by default), passed to the `CreateStore` it
  creates. `Reset` and `RecoverAndReset` are unchanged and take no handoff.
- `SecretStorage::create_secret_store()` initializes the new field to `None`;
  `secret_storage::mod` re-exports `GeneratedKeyHandoff`.

Unchanged: key generation (`SecretStorageKey::new` and
`new_from_passphrase`), encryption, the key description, the order and
content of every request, serialization, the store lock, the client's
request configuration and retries, progress updates, errors, and the
returned key. The handoff adds no request, no log line and no error path.
`zeroize` was already a dependency of this crate. No manifest, feature, lock,
build script, dependency or unsafe code changed.

Callers: only `crates/morpheus-matrix/src/account_recovery.rs`
(`enable_recovery_selected`). It keeps the handed-over key as unconfirmed
when setup fails before `Done`, and never retries setup.

Regression source (`tests/integration/encryption/recovery.rs`,
`test_recovery_setup_hands_over_the_key_when_its_last_request_fails`), on the
crate's existing wiremock harness: the backup and every secret are stored,
the server takes the default key, then that request answers 500. Setup fails
without `Done`; the handoff yields the key once and its `Debug` output does
not contain it; the stored default key names the uploaded key description
and the stored master key is encrypted for it; every request was sent
exactly once (no retry); and the handed-over key opens the server's secret
storage and decrypts the stored master key.

Formatting follows the upstream `rustfmt.toml` at the pinned revision
(100 columns, comments wrapped at 80, `use_small_heuristics = "Max"`), like
the surrounding upstream code. The root integration retains the regular
`.rustfmt.toml` described in Formatter inheritance above.

Additionally modified vendored files (upstream c6ad52e blob -> current blob):

| File | Upstream git blob | Current git blob |
| --- | --- | --- |
| src/encryption/secret_storage/futures.rs | b206b7c49824ecc7519f9c87c4df61d2d54441ad | 3eaa3b345d3cf08d474003094c6bf5a822cf43da |
| src/encryption/secret_storage/mod.rs | 213970804af065e76f7f3de9fc1d5c2d3603e875 | d262704881742032d3f5219dfee3bca5ebec387c |
| src/encryption/recovery/futures.rs | 8ac3f2b6e32b6b747f14545e74eec8fbde97948f | 9e8ee76b3a5b21ff67d12b6ea728d842b68d32f7 |
| tests/integration/encryption/recovery.rs | 3ebbd24cda63a2a89d8daf6b7e6c1baeb96b45bc | b0a25be4a7f779584e88ff28edcd830e65ba9e5d |

The upstream blobs are the `git_blob` values in `MORPHEUS_PROVENANCE.json`;
those records are unchanged. `LICENSE` and the upstream headers are kept.
`crates/morpheus-app/build.rs` hashes `vendor/`, so the persistence cohort
changes with this patch.

# matrix-sdk-crypto 0.19.1 bounded room-key file preparation

This is the exact package from Matrix Rust SDK revision
`c6ad52ecc09eaa36601f55effb0a48a533eb87dd`, with its upstream source,
tests and fixtures retained. `UPSTREAM_SOURCE.json` records original modes,
Git blobs and SHA256 values. `Cargo.toml.orig` preserves the original manifest;
`LICENSE` is the exact Apache-2.0 text from that SDK revision. The normalized
manifest expands pinned workspace inheritance and changes internal workspace
paths to the same pinned Git revision. All package versions, dependencies,
features, target-specific dependencies and original dev tooling remain present.
The root excludes this path-patched library from workspace membership; the root
lock changes only this existing package's Git source identity to a local path.

## Source adaptation

The original `.cargo/mermaid.html` symlink escaped the package to the SDK root.
The managed copy is a regular file with the exact pinned target bytes. Two
original benchmark JSON includes now point to identical preserved bytes under
`tests/fixtures/`; the original paths, blobs and hashes remain in the source
record. No upstream fixture/test is deleted or silently edited. Existing
public test-vector secret-scanner matches are held for independent classification;
there is no scanner exclusion, execution or publication approval in this draft.

## Opt-in byte API

The new bounded reader and writer perform no file IO or task spawning. They
reuse the canonical armor selection, PBKDF2, AES-CTR and constant-time HMAC
verification. The complete decoded layout is checked before slicing or KDF;
MAC verification precedes decryption and JSON parsing. The legacy public APIs
remain, with short-packet rejection and RAII plaintext protection added. Invalid
UTF-8 from canonical decryption now yields a fixed `InvalidPlaintext` category,
rather than a `FromUtf8Error` retaining decrypted bytes. The old public error
variant remains for source compatibility; new opt-in errors contain fixed
categories only. No crypto/parser details or plaintext are formatted into them.

The bounded contract is 32 MiB total armored bytes, 20,000 records and import
PBKDF2 rounds 1 through 1,000,000. Export always uses the existing SDK factor
500,000 and requires 12–1024 UTF-8 passphrase bytes; import accepts 1–1024 bytes.
The count visitor refuses record max+1 before invoking its deserializer. Room
IDs, session IDs, algorithm/signing-key names, forwarding count and signing-key
count have additional metadata limits before acceptance. A single record's
transient canonical deserialization allocation is bounded by input bytes; this
is not a constant-memory or hardware timing guarantee. Default serde recursion
limits remain active. Plaintext serialization has a checked byte ceiling and
zeroizing storage; oversize is an error, never a truncated recovery export.

Authenticated plaintext and serialization buffers use existing Zeroizing RAII
through UTF-8/serde failures and unwinding. The existing canonical cipher buffer
also stays zeroizing during native randomness and keystream transformation.
Key/cipher algorithms and parameters are unchanged; there is no custom crypto.
Returned parsed session keys retain their pinned vodozemac zeroizing Drop. This
does not promise erasure of all OS/runtime/compiler copies or allocator history.

## SDK and application ownership

The opt-in SDK seam wraps prepared keys in private-field, non-Clone,
non-Debug/non-Serialize types. Its explicit target-preparation await retains the
actual OlmMachine read guard; consuming import calls the existing store import
without a second client/store lookup await. The application must recheck its
captured account/security/work authority after ALL preparation waits and await
an admitted store operation even if presentation is cancelled. The SDK contains
no application callbacks or account-selection policy. Store import errors are
reported as uncertain completion, not confirmed unchanged data. Only counts are
returned, and SDK backup scheduling is not remote-backup completion evidence.
The existing store export first reads its whole session snapshot. The new seam
caps selected export conversion and rejects overflow; it never calls a limited
selection a complete backup and does not claim a constant-memory store read.

This source draft has not been typechecked or executed. Native chooser/IO,
Windows ACL/stable-handle/no-clobber proof, exact application authority hooks,
retained worker integration and fresh hosted interoperability/security checks
remain required. It does not enable update installation, distribution or beta
acceptance. Source and lock changes invalidate affected build/cohort evidence.

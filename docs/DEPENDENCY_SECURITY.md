# Dependency security review

The historical publication review on 2026-10-10 covered
all 1,052 registry package versions and reported seven maintenance notices,
with no OpenSSL/OpenSSL-sys advisory matches. The patched local `lru` version
match is reviewed separately below. Version-based results do not verify
unpublished Git revisions, local source patches, or every native library
bundled by a dependency; source review and platform acceptance remain required.

## Candidate refresh, 2026-10-18

A fresh [OSV query](https://google.github.io/osv.dev/post-v1-querybatch/) covered
all 1,126 locked registry versions, three pinned Git revisions and nine local
crates' upstream version references. Version queries do not establish the safety
of local patches or unreported issues; Git results depend on database coverage.

The candidate updates **Rustls 0.23.45 to 0.23.46** for
[session-secret debug redaction](https://rustsec.org/advisories/RUSTSEC-2026-0346.html),
[ECH rejection handling](https://rustsec.org/advisories/RUSTSEC-2026-0347.html),
[Brotli decompression bounds](https://rustsec.org/advisories/RUSTSEC-2026-0348.html)
and [authenticated key-exchange hints](https://rustsec.org/advisories/RUSTSEC-2026-0349.html).
These version matches are not evidence that Morpheus exercised every affected
configuration. Both release archives were checked against registry checksums;
the complete source diff was reviewed. Features, build script, licenses and
dependency requirements are unchanged. Only the Rustls version and checksum
change in Morpheus's lockfile. OSV returned no matches for 0.23.46 at review time.
**Native and integration verification of this update is still pending.**

The refresh also reports the existing seven maintenance notices below and new
maintenance notices for `zbus_names`, `zvariant` and `zvariant_derive`, whose
standalone crates are being consolidated into `zbus`. These require upstream
migration planning, not a claim of confirmed exploitation.

The locked `zip` build dependency has additional published advisory matches.
The candidate's local [native SDK build-helper patch](../vendor/webrtc-sys-build/MORPHEUS_PATCH.md)
replaces its extraction call and verifies pinned archive bytes before unpacking.
Source review and guarded tests do not replace actual four-platform download,
extraction and integration acceptance, which remains pending. Version-only ZIP
matches remain visible; no blanket advisory suppression is added.

## Applied panic-safety backport

`lru` 0.16.4 is pulled in by Tantivy 0.26.2. [RUSTSEC-2026-0253](https://rustsec.org/advisories/RUSTSEC-2026-0253.html)
describes a dangling-list-pointer bug when a popped key's destructor panics.
The local patch backports [upstream PR 238](https://github.com/jeromefroe/lru-rs/pull/238)
and retains its regression test. This preserves the pinned Matrix SDK and
Tantivy dependency interfaces. Current Tantivy code uses integer keys, whose
destructors cannot trigger this failure, but the dependency is hardened anyway.

The local crate retains version 0.16.4. Version-only scanners may still flag it;
review the patch provenance in `vendor/lru/MORPHEUS_PATCH.md` and rerun its panic
regression instead of suppressing all advisories. Remove the patch after an
upstream dependency update supplies a verified fixed release.

## Five existing dependency maintenance notices

These notices identify unmaintained transitive dependencies, not five confirmed
exploitable vulnerabilities in Morpheus. Track replacements through the owning
dependency and reassess on each lockfile update; do not silently change the
serialization or SDK interfaces to remove a warning.

| Dependency | Advisory | Introduced by | Follow-up |
| --- | --- | --- | --- |
| anymap2 0.13.0 | RUSTSEC-2026-0319 | matrix-sdk 0.19.1 | Follow the SDK's migration to a maintained type map |
| bincode 2.0.1 | RUSTSEC-2025-0141 | typed-index-collections 3.5.0 | Review enabled features and upstream serialization migration |
| derivative 2.2.0 | RUSTSEC-2024-0388 | wasm_evt_listener 0.1.0 | Recheck target reachability and upstream replacement |
| paste 1.0.15 | RUSTSEC-2024-0436 | nokhwa 0.10.11 | Follow a maintained camera backend or macro replacement |
| ttf-parser 0.25.1 | RUSTSEC-2026-0192 | owned_ttf_parser 0.25.1 | Follow the font/rendering stack's maintained parser migration |

## Optional backend entries introduced by vendored test workspaces

Making the patched crates workspace members allows their own regression tests
to run. Cargo also locks their optional backends, even though Morpheus enables
Tokio and Rustls. The resulting lockfile adds two further maintenance notices:

| Dependency | Advisory | Scope |
| --- | --- | --- |
| async-std 1.13.2 | RUSTSEC-2025-0052 | Discontinued; disabled async-std transport backend |
| instant 0.1.13 | RUSTSEC-2024-0384 | Unmaintained; disabled async-std transport backend |

The optional native-TLS backend initially resolved OpenSSL 0.10.78. Its three
advisories concern the OCSP URL accessor and AES key-wrap-with-padding APIs.
The publication lockfile uses 0.10.80, which fixes all three:
[OCSP URL](https://github.com/rust-openssl/rust-openssl/security/advisories/GHSA-xp3w-r5p5-63rr),
[key-wrap update](https://github.com/rust-openssl/rust-openssl/security/advisories/GHSA-xv59-967r-8726),
and [in-place update](https://github.com/rust-openssl/rust-openssl/security/advisories/GHSA-phqj-4mhp-q6mq).
OpenSSL is absent from Morpheus's `native-calls` dependency graph across targets.
Enabling either optional backend requires its own security and platform review.

New advisories require fresh review. A previous clean scan does not establish
the safety of changed dependencies or a release candidate.

## Pinned SDK receipt regression dependencies, 2026-10-11

The local `matrix-sdk-base` receipt patch retains version 0.19.1 and upstream
revision `c6ad52ecc09eaa36601f55effb0a48a533eb87dd`. Making its own regressions
selectable adds 66 registry package identities and four SDK Git test/optional
identities, without upgrading an existing package version. All 66 added
registry versions returned no advisory matches from the
[OSV query API](https://google.github.io/osv.dev/api/) on 2026-10-11. The exact
added identities and upstream file hashes are recorded in the
[provenance manifest](../vendor/matrix-sdk-base/MORPHEUS_PROVENANCE.json).

This incremental query does not replace the existing maintenance notices or
source review of the SDK patch. The receipt cache's trust, redaction, bounds,
and lifecycle tests still require execution on the hosted candidate.

## Pinned RTC membership renewal, 2026-10-11

The local `matrix-rtc-core` patch retains version `0.5.0-rc.2` and revision
`53880ef1ef66b62df555ce71ccd1d6c85b813596`. Resolution replaces the single Git
core package with the local source and adds its existing `futures` test
dependency to that package's lock entry. There are no new registry/Git package
identities or existing version upgrades. The original source inventory,
workspace values and licenses are recorded in its
[provenance manifest](../vendor/matrix-rtc-core/MORPHEUS_PROVENANCE.json).

The [patch note](../vendor/matrix-rtc-core/MORPHEUS_PATCH.md) records the two
changed source files, bounded maintenance hint, unchanged encryption/trust
policies and required core/live tests. This source patch requires hosted
verification; an unchanged dependency version does not establish its safety.

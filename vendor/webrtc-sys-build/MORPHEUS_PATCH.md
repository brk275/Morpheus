# Native SDK archive staging

This directory starts from crates.io `webrtc-sys-build` **0.3.18**, upstream
commit `a4f41cdcae5214986ab4ac5a8cb8507e5cc7ee6e`. Its Apache-2.0 copyright
headers are retained; `LICENSE-APACHE` contains the license text. The upstream
crate checksum, original file inventory and reviewed release asset identities
are recorded in [the provenance manifest](MORPHEUS_PROVENANCE.json).

The native SDK remains **`webrtc-51ef663`**. This patch does not select the newer
helper release or change the native SDK ABI. The existing Linux PipeWire
isolation patch still verifies the extracted static library independently.

## Changes

- Stream downloads over HTTPS with a bounded redirect count and timeout.
  Require the exact reviewed compressed size and SHA-256 before parsing ZIP
  contents. Pins cover the eleven release assets; unsupported profiles fail
  before downloading. Updating the SDK tag requires reviewing its pins too.
- Replace `ZipArchive::extract` with an extractor that accepts regular files
  and directories only. Reject traversal, links, Windows device/stream names,
  unsafe directory components and overwrites. Use fresh staging directories
  and create files exclusively. Limit entry count, path length/depth, individual
  expanded size and total expanded size; verify each copied length.
- Give extracted Unix directories owner access and files owner read/write,
  retaining owner execution where requested. Do not apply archived privileged,
  group or other permission bits. Windows directory reparse points are rejected.
- Publish the expected SDK directory by rename only after verification and
  extraction. Remove temporary files and staging directories on success/error.
- Require a bounded matching download receipt on cache reuse. Older caches
  without a receipt are rejected; remove the old SDK cache explicitly before
  rebuilding. A receipt records download provenance; it does **not** authenticate
  a mutable cache tree against another process running as the same OS user.
  Explicit `LK_CUSTOM_WEBRTC` builds remain a developer-controlled opt-in and
  are not covered by the archive pins or Morpheus release acceptance.

The only added dependency is the already-locked `sha2` 0.10.9. The upstream
generated lockfile, changelog and registry metadata are omitted from this local
workspace package; their original hashes remain in the provenance inventory.

CI jobs that build the native SDK use the `verified-sdk-archives-v1` cache
key. The pinned cache action includes this key in both exact and fallback
prefixes, so pre-patch target caches cannot restore an unverified SDK extraction.
The configuration-only and account-vault jobs keep their existing keys.
Cache writes remain restricted to trusted `main` runs.

## Verification status

Guarded tests cover download identity/length rejection, cache receipts, unsafe
paths and link entries, oversized entries, non-overwrite behavior, Unix special
permission removal and refusal to traverse an existing directory link. They
exercise the new extractor rather than an unpatched reproducer.

The six archive guard tests passed on Linux in
[run 39103257117](https://github.com/brk275/Morpheus/actions/runs/39103257117),
at source revision `519feec284500b20dfd2909ab4a0ed9e77f0ebdc`.
Actual SDK downloads/extraction and platform-specific guard behavior on Apple
Silicon, Intel Mac, Windows and Linux are **not yet verified**. The ordinary CI vendor gate
includes this crate's unit tests. No source review or checksum alone establishes
beta readiness, and version-only ZIP advisory matches must still be assessed
against actual callers; do not suppress ZIP advisories globally.

# Original Windows rc_win build-tool DATA

The selected WebRTC `aaeeee8077eb0a4cad1c9494e9c6433c751ef663` DEPS
`rc_win` hook uses `build/toolchain/win/rc/win/rc.exe.sha1` from the pinned
Chromium build `be1a8f6dcd7df7e46320192c5e2f364e50d79bbf` checkout.
That original pointer is `dea7da0d9bfcb8dad6cf53ca608dddac3459faaa`.
The existing toolchain calls `tool_wrapper.py` -> `rc/rc.py`; on Windows this
selects that bundled executable. Microsoft's host SDK resource compiler does
not supply that file or replace the selected reimplementation.

`native-tools.json` binds exactly one object from the original GCS bucket
`chromium-browser-clang`, object `rc/dea7da0d9bfcb8dad6cf53ca608dddac3459faaa`,
generation `1729017425353926`, 603648 bytes, SHA256
`903769d98df8a005c79d52d7089fe34f23c0b7ada4cd0c42b6063526a5593e6a`.
`reference/rc-win-gcs.json` preserves original public object metadata. The
producer preserves the original SHA1 as an additional full-byte check, uses
its existing bounded authenticated HTTPS/GCS downloader, and copies only that
one file into the exact original source path. Existing source files, aliases,
gitlinks and case collisions may not be overwritten. The full typed source
overlay and actual private inventories include the new file.

This is DATA staging. It executes no downloaded tool or hook, provides no host
MSVC/SDK observation, and leaves `build_inputs` null and every build/release
admission unchanged. Runtime35 archives remain separate and unchanged.

## Optional staging and build applicability

The default pipeline leaves `native_tools_profile` null. The producer then
skips this supplemental download and emits explicit null tool-profile/file
fields; it creates neither an RC overlay row nor a supplemental receipt.
The exact `native-tools.json` opt-in DATA route remains available with every
original object, metadata, SHA1, SHA256, pointer and no-overwrite check intact.

Windows inputs v5 accepts the original exact `source_provision` references
without `native_tools_receipt`, or those same references plus that receipt.
The receipt must be present if and only if the overlay contains an RC path
matching the fixed path under case folding. When present, the receipt, original
pointer and exact typed overlay row are all required; a case alias still fails
the exact row comparison. Package provenance includes `NATIVE_TOOLS.json` only
when declared and validated, and otherwise records explicit null supplemental
fields. RC bytes are not added to the package payload by this contract.

The original DEPS hook makes this download available; it is not automatic
authorization to execute the tool or proof that the selected targets require it.
The inspected first-party resource source belongs to the separately selected
`desktop_capture_unittests`, not the fixed `rtc_unittests` target; the ordinary
Windows executable manifest uses linker manifest inputs. This source inspection
is not a fully resolved GN dependency graph. Production `:default`, the separate
`rtc_unittests` build and all twelve exact tests remain unchanged. A selected
target that actually needs an absent RC tool must fail; no target or check is
skipped to accommodate its absence.

## Unresolved source and license evidence

The pinned `build/toolchain/win/rc/README.md` links to the source project
https://github.com/nico/hack/tree/master/res. Chromium's original publisher
record for CL 5935354 explicitly links the pointer update to
https://github.com/nico/hack/commit/e0d5f9bbfb399dbab065635531bec34afa5ba718.
That identifies a source change, not a complete immutable upload/build recipe.
The inspected source has scoped Unicode notices but no established overall
license grant for the program. The original `rc.py` wrapper's own license
header does not supply that missing grant. Neither optional staging nor omission
claims this tool's preferred-source/license closure, grants its execution or
distribution, or waives any gate applicable to tools actually used. All twelve
independent native build gates and the null build admission remain unchanged.

## Complete hosted-toolchain inventory time policy

The read-only Windows host-toolchain preflight uses fixed policy
`complete-host-inventory-900s-v1`. One 900-second watch starts in
`Initialize-HostReader` before native and managed setup and spans role hashes,
the complete selected VS2022 edition root, the complete Windows Kits10 root,
and final retained-handle identity checks. It never resets per file or root.
Native cancellation and cleanup retain their separate aggregate 5000ms bound;
uncertain cleanup still refuses reuse. No dispatch or environment time override
is accepted.

Inventory receipts use `morpheus-native-windows-host-toolchain-inventory-v3`
and the explicit `inventory_policy` above. Closed cost suffixes use
`owned-exclusive-wall-900s-v2`, with each component and their sum bounded to
905000ms including cleanup. Consumers must match these exact identities and
limits; old v2 receipts and v1 cost records remain evidence of the earlier
180-second policy, not successful validation under this policy.

The host-toolchain workflow alone has a 17-minute outer limit. Basic preflight
remains five minutes, stage/build remains 240 minutes, and synthetic fixture
case, child, metadata and job deadlines remain unchanged. These are offline
validation limits, not application or media runtime limits. Entry, handle,
byte, depth, metadata, JSON, buffer, native provenance, full hashing and final
identity requirements are unchanged. This policy does not authorize toolchain
admission or distribution. It is not a speed improvement or evidence that 900s
is sufficient; a further failure requires diagnosis, not another time increase.

### Receipt row representation budget

The unchanged 30MiB metadata ledger bounds receipt JSON rows, not total managed
heap memory. Each file row retains the 160-byte fixed-field allowance, charging
its path by a conservative JSON escaping upper bound: six bytes per control,
non-ASCII, quote or HTML-sensitive UTF-16 unit; two per slash/backslash; one per
remaining printable ASCII unit. Both surrogate halves receive their own six-byte
allowance. Managed traversal reserves this exact bound before starting payload
reads and commits it once after hashing, as before. Original fallback commits
the same bound in its existing position. No file is omitted or truncated.

This avoids treating every ordinary ASCII path character as a Unicode escape.
The full selected VS2022 root and Windows Kits10 root, every digest and final
retained-handle identity check remain required. All metadata/JSON/entry/handle/
byte/depth/memo/buffer/time/cleanup caps remain unchanged; final actual UTF-8 JSON
still must fit 32MiB. A source-reviewed fixture compares the production bound to
the pinned serializer with mixed escape classes and checks actual managed and
fallback ledger commits. No hosted completion or speedup is implied.

### Cancellation ownership of small and text payloads

All admitted managed payload reads, including small files, empty-file EOF and
bounded UTF-8 version text, use the same persistent physical read worker. The
coordinator keeps the original 900-second watch and targeted cancellation joins
that worker under the existing aggregate 5000ms cleanup bound. Handle exhaustion
refuses admission instead of selecting an uncancellable synchronous fallback.
Text is copied only by the coordinator from completed ranges, then decoded with
the existing strict UTF-8 rule. No worker accesses the text stream. Borrowed
file wrappers remain retained until reads complete or worker join is proven;
failed cleanup keeps the original sticky ownership refusal. Construction failure
before worker startup also releases any borrowed wrappers deterministically.

The existing synthetic fixture covers blocked small/text reads and retained
ownership through failed cleanup, plus a real positive version-text digest.
This corrects cancellation scope; it does not attribute or promise to cure the
separate full-inventory read-wait failure or change any resource/time cap.

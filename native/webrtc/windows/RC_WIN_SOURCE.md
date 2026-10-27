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

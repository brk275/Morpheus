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

The original pinned `build/toolchain/win/rc/README.md` identifies the binary's
source project as https://github.com/nico/hack/tree/master/res. That mutable
link does not prove the preferred-source revision used for this particular
object. The original `rc.py` source header refers to a BSD-style license in a
`LICENSE` file; this alone does not establish the binary's source/license mapping.
Exact corresponding-source/license closure remains required before native
execution or distribution approval; no preferred-source revision or old native
asset correspondence is asserted by this DATA record.

The future builder contract is explicitly Windows inputs v5. Its required
`source_provision.native_tools_receipt` names the actual staged
`materials/windows-native-tools.json` file. The builder binds the exact object,
profile, original pointer bytes/hash and typed overlay row, and carries that
receipt into package provenance. This does not execute or redistribute `rc.exe`
by itself and does not satisfy the twelve independent native build gates.

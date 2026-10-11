# Wayland capture fixture

Run `python3 scripts/run_wayland_acceptance.py` on the standard Ubuntu 24.04
CI runner. This is a real, isolated Sway/PipeWire/portal integration check with
two software-rendered displays. It does not capture a user's desktop or ask for
device permission. It must pass; a failure is not converted into a skip.

## Why the fixture builds its own portal backend

Ubuntu 24.04 ships xdg-desktop-portal-wlr 0.7.1. Its
[`start_screencast` check](https://github.com/emersion/xdg-desktop-portal-wlr/blob/74428f2a8fa7f252e2a46fdf5b697536c66c8a1c/src/screencast/screencast.c#L199)
requires both SHM and DMA-BUF formats for the advertised portal version. Sway's
Pixman software allocator exposes SHM; wlroots only sends the DMA-BUF format
when the output allocator supports it. This combination can reject `Start`
before any PipeWire frames are produced. The first hosted attempt failed at
portal startup; its generic error did not preserve enough detail to prove the
exact failing backend phase.

The helper downloads upstream v0.7.1 at immutable commit
`74428f2a8fa7f252e2a46fdf5b697536c66c8a1c` and verifies its archive SHA-256:

```text
10050be0b2675ffe02f9d09102a23e972d170628049b91fff6b5433f5eab7a7e
```

It also verifies the complete original `screencast.c` SHA-256 before applying
one exact replacement:

```diff
- if (SHM format is invalid || (protocol version >= 3 && DMA-BUF format is invalid))
+ if (SHM format is invalid)
```

The real patch retains the SHM format validation and its failure path. It does
not change the chooser, authorization, requested source type, number of streams,
source identity, or capture code. It is a CI compatibility adaptation informed
by upstream's later removal of this check during
[`21288533ce0ad39ceed055ed0af457062515a392`](https://github.com/emersion/xdg-desktop-portal-wlr/commit/21288533ce0ad39ceed055ed0af457062515a392),
not a verbatim upstream backport. Newer 0.8.0 also changes compositor/protocol
requirements, so the fixture does not silently substitute that release.

Meson configures and builds only inside the fixture's mode-0700 temporary
directory. There is no `meson install`, system replacement, distributed backend
binary, or change to Morpheus packaging. Downloads, source, compiler output,
backend binary and service logs are deleted at fixture exit. The source's MIT
license remains in that temporary source tree. Each build command has a
180-second deadline and participates in the fixture's owned-process cleanup.
Meson subproject downloads are disabled. The helper checks that its backend
process owns the D-Bus service before starting the portal frontend.
The overall session deadline is 840 seconds, leaving cleanup time inside the
15-minute workflow step; test discovery has a 300-second deadline after the
preceding native build has already populated dependencies.

The fixture requires a disposable GitHub-hosted Linux job with passwordless
`sudo`, `unshare` and `setpriv`. It reuses the reviewed PID namespace wrapper
from the Element fixture. Before any desktop services start, a real preflight
kills the same non-TTY wrapper and requires both namespace PID1 and a detached
child to disappear. Repository Python, compiler, desktop and native test code
run only after UID/GID, supplementary groups and capabilities have been
restricted by the wrapper. The outer parent keeps its direct child unreaped
until group cleanup. If normal inside-session cleanup cannot run, namespace
termination also stops separately sessioned descendants; no stored PID or
start-time snapshot is used as a signal target. Runtime capture stage, exit and
signal facts remain independent of this containment check.

Additional build dependencies, beyond the existing fixture/runtime packages:

```text
meson ninja-build pkg-config libinih-dev libsystemd-dev libgbm-dev libdrm-dev
wayland-protocols libwayland-dev libpipewire-0.3-dev
```

## What a pass establishes

Before the first PipeWire initializer, the native test checks the resolved
addresses of all 33 functions that collide with the pinned WebRTC dlopen stub
exports. Every one must resolve to the root-owned Ubuntu system PipeWire
library; executable stubs or other/unknown libraries fail before initialization.
The helper requires this evidence in addition to the named capture test and
prints only fixed category counts. Separate `pipewire_binding_origin` and
`pipewire_initializer` stages survive a native abort. The
[vendored build patch](../vendor/webrtc-sys/MORPHEUS_PATCH.md) verifies and
isolates all 33 WebRTC definitions and 46 internal references in `OUT_DIR`.
The prior SIGSEGV at `opening_display_1` did not record binding origins, so the
symbol-collision fix still requires hosted confirmation; no capture pass is
inferred from source inspection.

The existing acceptance test still verifies the selected second display,
changing frame contents, repeated start/stop, source departure, refusal to
switch silently to another output, denial, and bounded cancellation. X11 is
disabled. A window request must not fall back to a display.
The helper requires the exact named test to report success and one executed,
passed test in libtest's summary. Test discovery or a zero-test exit is not a
pass, including if the test accidentally loses its `#[ignore]` attribute.

A pass establishes real portal and PipeWire SHM capture against this adapted
backend and the headless Sway fixture. It does **not** establish compatibility
with unmodified distribution backends, GNOME, KDE, native-window sharing,
physical displays, hardware buffers, or packaged permission behavior. Those
remain separate release gates. Morpheus itself continues to use the user's
system portal; this CI backend is never shipped or used as an app fallback.

On failure, the app reports the portal phase using fixed, local messages. The
helper emits only allowlisted boolean facts and fixed service-state values
from private logs. It never prints raw service lines, pixels, output names,
application identifiers, session paths, or node IDs. Bounded log scans mark
truncation explicitly; absence of a matched flag is not proof that a phase did
not occur outside the scanned region.
Captured libtest output also stays private on failure. Only fixed result/phase
facts, bounded command/child exit codes or signal numbers, fixed library-panic
categories, and the known fixture source's numeric panic line may be reported.
A private stage file preserves the last fixed test phase even when a native
abort prevents libtest from flushing captured output. Neither a missing panic
marker nor an exit code alone establishes the underlying cause.

The capture adapter drops PipeWire chunks marked `CORRUPTED` before reading
their pixels, even when the backend retains a previous nonzero chunk size.
This does not end an otherwise healthy stream: xdpw retries after an individual
failed frame. Existing portal closure and stream state checks handle source
loss. The metadata regression covers valid pixel decoding, no access to corrupt
nonempty or empty chunks, and a subsequent valid frame. This correctness fix and the
fixture cleanup change are not evidence of the cause of an earlier native
capture failure; the actual hosted stream check still has to pass.

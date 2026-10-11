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
The earlier SIGSEGV at `opening_display_1` did not record binding origins.
Candidate `16fbd5b`, run `38110401901`, subsequently confirmed all 33 bindings
resolve to system PipeWire, with zero executable/other/unavailable resolutions.
That run passed opening and reached `first_frame_1`, where capture failed;
the wlr backend had exited while the other four services remained running.
Its retained facts do not include the backend exit status or raw error body.
This establishes binding-origin progress, not a working capture stream.

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
Repeated capture currently supplies changing fixture pixels; reopening an
untouched static source remains a separate open gate, as explained below.

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
Service diagnostics now also retain bounded exit codes (0–255) or signal
numbers (1–64), plus literal backend fatal, format-negotiation, buffer-allocation
and assertion markers. The assertion expression and all other matched text
remain private. Fixed native error markers distinguish source termination
from the first-frame delivery deadline.

## SPA format enumeration correction

Source inspection after the `16fbd5b` failure found an independent protocol
defect: the capture adapter encoded BGRx as the SPA Enum default, but omitted
it from the accepted alternatives. Pinned libspa 0.10.1 encodes those fields
separately. PipeWire's
[`spa_pod_filter_prop`](https://github.com/PipeWire/pipewire/blob/1.0.5/spa/include/spa/pod/filter.h)
skips the default slot while matching alternatives. Consequently, a BGRx-only
source cannot negotiate with the old offer, despite the adapter's BGRx decoder.

The corrected offer includes BGRx in both slots. A regression serializes and
deserializes the actual offer and checks that its accepted alternatives include
BGRx and the other supported formats, while excluding unsupported NV12. The
fixture's pinned xdpw source and its SHM adaptation are unchanged. xdpw 0.7.1
can offer BGRx alone for an XRGB8888 source and exits on a PipeWire core error,
so this defect is consistent with the observed failure. The deleted raw child
logs cannot establish that it caused this run's backend exit; the corrected
candidate still requires actual hosted frame acceptance.

The capture adapter drops PipeWire chunks marked `CORRUPTED` before reading
their pixels, even when the backend retains a previous nonzero chunk size.
This does not end an otherwise healthy stream: xdpw retries after an individual
failed frame. Existing portal closure and stream state checks handle source
loss. The metadata regression covers valid pixel decoding, no access to corrupt
nonempty or empty chunks, and a subsequent valid frame. This correctness fix and the
fixture cleanup change are not evidence of the cause of an earlier native
capture failure; the actual hosted stream check still has to pass.

## Consumer buffer negotiation correction

Candidate `db25e42`, run `38112448483`, confirmed all 33 system-library
bindings and reached `first_frame_1`. The portal backend stayed running,
negotiated a format and added buffers, but Morpheus reached its ten-second
first-frame deadline. These backend facts describe the producer; they do not
establish consumer allocation, streaming, or delivery. No source-selection
or permission check failed in the retained facts.

Inspection found that the consumer parsed `SPA_PARAM_Format` without completing
the required `pw_stream_update_params` step. PipeWire's
[stream contract](https://github.com/PipeWire/pipewire/blob/1.0.5/src/pipewire/stream.h#L75-L102)
requires this step after the format callback; its
[video consumer example](https://github.com/PipeWire/pipewire/blob/1.0.5/src/examples/video-play.c#L335-L365)
supplies buffer requirements there. Morpheus now validates the negotiated packed
RGB format and dimensions, then advertises 2–16 single-plane buffers using
CPU-addressable `MemPtr`/`MemFd` memory. DMA-BUF is not accepted by this decoder.
Size and stride remain producer-specified to preserve padded SHM rows; received
offsets, strides and extents are still checked before pixel access. PipeWire
copies the serialized parameters during the update call.

A regression deserializes the actual buffer offer and checks count, plane count,
memory flags, rejection of unsupported formats/dimensions, and absence of fixed
stride/size constraints. Fixed timeout categories now distinguish consumer
format acceptance, allocation, streaming, processing, dequeue without data,
and a dequeued buffer without a usable frame. The helper also extracts literal
producer streaming, buffer shortage/enqueue-attempt, and screencopy
submission/readiness facts without publishing the matched log text.
These facts improve any later failure diagnosis; they do not
replace the existing selected-source pixel, change, stop, denial and cancellation
assertions.

This corrects an omitted public API step. The retained `db25e42` evidence cannot
prove it was the sole cause of the deadline, and the corrected candidate still
requires hosted compilation and actual frame acceptance. The pinned fixture
backend, its SHM adaptation, and all other platform capture paths are unchanged.

## Damage-driven reopen fixture

Candidate `3f1a3df`, run `38115477988`, passed the first selected red frame and
its stop budget, then timed out at `first_frame_2`. The second consumer had
accepted its format, allocated buffers and reached streaming, but had no process
callback. All five services stayed running and all 33 library bindings passed.
Producer readiness/enqueue facts aggregate all sessions, so they do not establish
that the second producer delivered a frame.

The pinned xdpw backend calls
[`copy_with_damage`](https://github.com/emersion/xdg-desktop-portal-wlr/blob/74428f2a8fa7f252e2a46fdf5b697536c66c8a1c/src/screencast/wlr_screencast.c#L178)
through its persistent screencopy manager. This protocol request waits for
changed output pixels. The hosted job installed Sway `1.9-1build2` and wlroots
`0.17.1-2.1build1`. Its
[Ubuntu source archive](https://archive.ubuntu.com/ubuntu/pool/universe/w/wlroots/wlroots_0.17.1-2.1build1.dsc)
identifies the reviewed `types/wlr_screencopy_v1.c`, whose SHA-256 is
`1b862fab88ec313ba9e60edc6e2188046051e13d3e4335a66bbeb350b20bfe6d`.
That implementation keeps damage per manager client/output, clears it after
delivery, and waits if a later `copy_with_damage` has no new damage. Ubuntu's
single downstream patch changes layer-shell, not screencopy. Reopening the
same unchanged red output therefore need not immediately produce another frame.
This is a source-backed explanation consistent with the retained facts, not a
claim that deleted per-session raw logs proved the precise timeout cause.

The fixture retains the first static red frame assertion. On the second open it
alternates two green brightnesses; on the third it alternates two yellow
brightnesses. Each newly opened capture must produce its distinct expected
chroma. A prior red/green frame, or the unselected blue display, cannot satisfy
those assertions. Repeated changes during negotiation avoid one change being
consumed by the preceding producer as it shuts down. Each animator is bounded
to 20 seconds, each quiet `swaymsg` child to two seconds, and the owning worker
is stopped and joined before accepting success or measuring native stop time.
Child errors remain fixed categories; pixels and IPC payloads are not recorded.
Panic cleanup also stops/joins the worker and kills/reaps its owned command.

After the third capture, animation stops and a separate magenta change must
arrive before the existing source-loss, no-silent-switch, denial and cancellation
checks. Production capture code and the pinned backend adaptation are unchanged.
This is stronger evidence of fresh moving pixels across independent sessions;
it does **not** resolve or validate an untouched static source on reopen.
That backend/product behavior and actual hosted success remain open checks.

## First-frame delivery diagnostics

Candidate `e22d685`, run `38117794649`, failed again at `first_frame_1`.
Format, buffers and streaming were observed on the consumer, but its process
callback did not run. All 33 PipeWire bindings resolved to the system library,
all five services were running, and producer submission/readiness/enqueue-attempt
markers were present. This is a first-frame failure; second/third-open pixel
animation does not explain or resolve it. The static first-frame assertion
remains unchanged.

The next diagnostic candidate instruments the pinned backend's original
`src/screencast/pipewire_screencast.c`, whose full SHA-256 must match before
exact, anchor-checked diagnostic source replacements:

```text
e5dd5063d50494efaf82295807b43b20a8c5e68a389e89dbcfca63a724d60d4f
```

For at most the first 16 enqueue attempts per backend process, fixed literals
record buffer presence, valid/corrupt frame status, the read-only
`pw_stream_is_driving` result, and accepted/rejected `pw_stream_queue_buffer`
result. Correlated queue samples retain only those fixed categories. The
original queue call still executes exactly once. No frame, chooser, scheduling,
return-value or source-selection behavior is changed; this instrumentation is
part of the temporary fixture backend only.

The distinction matters because
[xdpw's enqueue marker](https://github.com/emersion/xdg-desktop-portal-wlr/blob/74428f2a8fa7f252e2a46fdf5b697536c66c8a1c/src/screencast/pipewire_screencast.c#L429-L525)
precedes its missing-buffer check and ignored queue result. In
[PipeWire 1.0.5](https://github.com/PipeWire/pipewire/blob/1.0.5/src/pipewire/stream.c#L2452-L2474),
queueing invokes the legacy graph trigger only if the output stream is already
driving and has not switched to explicit trigger mode. An accepted queue while
not driving can therefore be distinguished from a rejected queue or a queue
that entered that trigger branch. Neither acceptance nor the driving getter
alone proves that the consumer processed the frame. Source inspection also
found explicit follower-before-driver startup ordering, so a startup race has
not been established and no speculative production correction is applied.

The backend and capture test also receive fixture-only `pw.stream`/`pw.node`
debug logging. Source-verified function/message patterns yield boolean facts
for Start, non-null Clock/Position IO setup, driver selection, the legacy
trigger branch, and non-active/incomplete graph reports. PipeWire's
[build flags](https://github.com/PipeWire/pipewire/blob/1.0.5/meson.build#L99-L102)
define `FASTPATH`; its
[logging header](https://github.com/PipeWire/pipewire/blob/1.0.5/src/pipewire/log.h#L138-L142)
compiles process-input/output and callback fastpath tracing out. Those absent
records are deliberately not presented as observations. The initial trace
captured backend/client processes only; the ordered diagnostic extension below
also captures the daemon. These diagnostics do not prove exact graph delivery
or consumer dispatch.

Each of the three traced child outputs is drained into a private mode-0600 file
with an 8 MiB storage cap. Excess bytes are discarded while the pipe continues
to drain; partial writes are completed and errors/truncation are explicit.
No file-size limit is imposed on Cargo artifacts. Systemd logging and inherited
PipeWire log destinations are disabled for these children. Normal owned
process cleanup precedes joining the bounded readers to EOF and reporting the
trace; a missing EOF explicitly marks it incomplete. Files remain
under the mode-0700 temporary fixture and are deleted on exit, never uploaded.
Only fixed boolean/category samples leave the fixture. Missing, truncated,
changed-format or unrecognized log records cannot establish that an event did
not occur. All selected-source, static-frame, moving-frame, source-loss,
shutdown, denial and cancellation gates remain required.

## Ordered graph diagnostics

Candidate `d155e25`, run `38119789400`, recorded one valid producer queue that
was accepted while driving, followed by a corrupt queue during the failed
attempt. The consumer had Start and Clock/Position IO but no process callback.
Aggregate `graph_not_finished` and `ready_nonactive_node` flags do not identify
the cause: PipeWire's latter branch explicitly handles nodes removed during
shutdown, and the former continues scheduling. No producer scheduling change,
new damage, retry, or production correction is justified by those facts alone.

The next diagnostic input includes the daemon's private `pw.node`/`pw.link`
logs and a bounded ordered report. Each backend queue retains the original
single queue operation and adds timestamps immediately before and after it,
using `CLOCK_MONOTONIC_RAW`. This mirrors PipeWire 1.0.5's
[support logger](https://github.com/PipeWire/pipewire/blob/1.0.5/spa/plugins/support/logger.c#L94-L99):
microsecond precision with folded seconds. Its
[initialization](https://github.com/PipeWire/pipewire/blob/1.0.5/src/pipewire/pipewire.c#L606)
enables that logger's timestamps. The PID namespace does not create a separate
time namespace. xdpw's ordinary wall-clock prefix is ignored; it is never
correlated as a monotonic timestamp.

For each traced client only, the harness sets `PIPEWIRE_PROPS` to a synthetic
producer or consumer `node.name`. PipeWire's
[stream property override](https://github.com/PipeWire/pipewire/blob/1.0.5/src/pipewire/stream.c#L2052)
applies this diagnostic input; no target, format, driver, or permission property
is supplied. The daemon gets no override and the parent environment is untouched.
The isolated fixture has no custom WirePlumber rules, and its data/state roots
are now private too. The reviewed WirePlumber 0.4.17 stock monitor rules match
ALSA, BlueZ, V4L2 and libcamera prefixes, not these synthetic labels. General
policy still runs normally; this is a documented fixture property change,
not evidence about an unmodified desktop configuration.

The parser recognizes exact source/function/message forms and classifies names,
node IDs and link identities internally. Only fixed process roles and event
categories leave the fixture, in at most 96 events. Equal-microsecond events
form an **unordered group**; list order within that group has no causal meaning.
Repeated role identities, including a destroyed/recreated identity, flag
cross-session ambiguity. Link identity must be observed before its activation
and is forgotten at destruction. No private name, ID, pointer, raw line or
timestamp is published. Missing sources, event/storage limits, unsupported
logger records and incomplete pipe drains are explicit. A clock wrap or a span
longer than the fixture budget invalidates the order rather than guessing an
epoch. Partial source coverage is labeled partial.

Harness `test_finished`/`test_timeout` and `cleanup_begin` observations share
the same clock. The first is observed **after** the child exits; Rust panic
unwinding and `PortalCapture::drop` can already have run. Events before that
boundary can therefore still be teardown. The report orders logging
observations, not completion of asynchronous work, and cannot prove absence
of a callback that upstream compiled out. All static first-frame, selected
pixels, source-loss, shutdown, denial and cancellation assertions remain intact.

## Startup ordering investigation, 2026-10-11

At `954b2a5`, the complete ordered trace recorded a valid producer queue and
legacy driver trigger before the consumer's local node became Running. No
consumer process callback followed; the first static frame failed. The
nonactive-node warning occurred during teardown and is not startup evidence.

Source review found an applicable upstream race description:
[PipeWire commit 800a25a](https://github.com/PipeWire/pipewire/commit/800a25a01f99156d7d954f4d7fe1b0a6cf8ae7e3),
included in 1.2.6, completes exported-node state changes synchronously before
acknowledging the server. In the runner's 1.0.5 path, completion is queued and
`do_node_add` clears a pending eventfd before activating the node. The observed
order is consistent with the described startup race, but the retained trace
does not show whether that read consumed the producer's wake-up. It therefore
does not establish the root cause of this fixture failure.

The consumer's Streaming callback runs before this activation and cannot act
as its completion barrier. Real-time callbacks would require a different thread
ownership model and do not fix this core ordering. The source review did not
identify a supported application workaround. The system PipeWire library and
the mandatory first static-frame assertion remain unchanged; this gate is open.

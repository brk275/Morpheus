# Native frame timing probe

`crates/morpheus-app/examples/frame_probe.rs` answers one prerequisite question
for the real 10,000-message timeline gate: can a standard hosted Linux runner,
under Xvfb with Mesa software OpenGL, run Slint's real winit FemtoVG renderer
and deliver its per-frame rendering callbacks? It is a Cargo example, outside
normal app builds and packages, and does not open Morpheus itself.

**This probe is not performance acceptance.** Its numbers are CPU timings of
Slint's rendering callbacks for a tiny animated window. They are not display
FPS, GPU time, presentation latency or Morpheus timeline performance, and a
passing result does not satisfy any [release gate](RELEASE_CHECKLIST.md). The
eventual real 10k check must still cover a server-backed room, including
encrypted history, through both sync adapters.

## What it checks

- The environment explicitly requests `SLINT_BACKEND=winit-femtovg` and Mesa
  software OpenGL (`LIBGL_ALWAYS_SOFTWARE=1`), on Linux with an X11 `DISPLAY`
  and no `WAYLAND_DISPLAY`. The probe cannot prove which OpenGL driver ran; the
  variable records intent.
- `BackendSelector` names both the winit backend and the FemtoVG renderer.
  Winit can still fall back to another renderer if FemtoVG cannot be created.
  The only other renderer compiled into this package is the software renderer,
  which has no rendering notifier, so the probe then fails with
  `renderer_not_opengl` and never reports software-renderer frame counts.
- `Window::set_rendering_notifier` must report `RenderingSetup` with
  `GraphicsAPI::NativeOpenGL` whose loader resolves `glGetString` (the pointer
  is checked, never called), and every `BeforeRendering` must be matched by one
  `AfterRendering` on the same OpenGL API.
- After 10 warm-up frames it stores at most 120 samples, then hides the window
  and leaves the event loop. An event-loop deadline (10 s) closes the window if
  samples are incomplete; an independent watchdog thread (20 s) ends the
  process if rendering or closing hangs.

In Slint 1.18.1's FemtoVG renderer, the `BeforeRendering` to `AfterRendering`
interval covers item rendering and the OpenGL command submission, but not the
buffer swap (`present_surface`) or driver work deferred past it. Registering a
notifier also adds one early canvas flush per frame. The interval between frame
starts additionally includes event-loop scheduling and the previous swap.

## Running it

Only run it on a fresh standard hosted Linux runner after review. It needs the
app's normal Linux build dependencies plus Xvfb and Mesa:

```text
cargo build --locked -p morpheus-app --example frame_probe
SLINT_BACKEND=winit-femtovg LIBGL_ALWAYS_SOFTWARE=1 \
  xvfb-run -a -s '-screen 0 1280x800x24' target/debug/examples/frame_probe
```

A debug build gives debug-profile timings; record the profile with any result.
The probe reads only environment variables. It opens no files, network
connections, accounts, profiles or credential stores, and never prints backend
error text. Mesa, winit or Slint may still write their own messages to stderr,
so keep that stream private like other native logs.

## Output

Standard output is exactly one JSON line. A pass has this shape; timings are
microseconds, shown here as placeholder zeros:

```json
{"probe":"morpheus_frame_timing","result":"pass","backend":"winit-femtovg","graphics_api":"native_opengl","software_gl_requested":true,"measure":"cpu_render_callback_before_presentation","warmup_frames":10,"samples":120,"render_callback_us":{"p50":0,"p95":0,"max":0},"frame_start_interval_us":{"p50":0,"p95":0,"max":0},"teardown_observed":true}
```

Percentiles use the nearest rank of the stored samples. `teardown_observed`
is informational: on X11, hiding a window keeps its OpenGL context until the
event loop exits.

A failure prints `{"probe":"morpheus_frame_timing","result":"error","reason":…}`
with one fixed reason and exits with status 1:

| Reason | Meaning |
| --- | --- |
| `unsupported_platform` | Not Linux |
| `backend_not_winit_femtovg` | `SLINT_BACKEND` is not exactly `winit-femtovg` |
| `software_gl_not_requested` | `LIBGL_ALWAYS_SOFTWARE` is not `1` |
| `x11_display_missing`, `wayland_display_present` | Not an X11-only session |
| `backend_unavailable`, `window_unavailable`, `window_show_failed`, `event_loop_failed` | Slint could not start, show or run the window |
| `renderer_not_opengl`, `notifier_unavailable` | No rendering notifier, such as a software-renderer fallback |
| `graphics_api_not_native_opengl`, `setup_repeated`, `unknown_rendering_state` | Callbacks did not come from one OpenGL context |
| `unpaired_frame` | A `BeforeRendering`/`AfterRendering` pair was broken |
| `insufficient_frames` | Fewer than 130 complete frames before the deadline |
| `close_failed`, `probe_panicked` | The window could not be closed, or the probe panicked |

If the watchdog fires it prints `watchdog_expired` and exits with status 3.

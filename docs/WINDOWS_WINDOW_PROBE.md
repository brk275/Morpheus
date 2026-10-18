# Windows window probe

The installed Windows keyboard check failed at `step=window`,
`failure=window_count`: the release process had **more than one** visible
top-level window of its own, while
`scripts/windows_package_ui.py` requires exactly one. This probe gathers cheap
evidence about which windows those are, without a release build, an installer,
a profile or Morpheus application code. The probe sends no input; installed
keyboard window selection is a separate check.

## Hypothesis from the pinned sources

winit 0.30.13 creates a per-thread "Winit Thread Event Target" window when its
event loop starts (`platform_impl/windows/event_loop.rs`,
`create_event_target_window`). It requests a top-level, unowned, zero-size popup with
`WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW`, and
winit then sets `WS_VISIBLE | WS_POPUP` so it receives `WM_PAINT` during
resizes. `IsWindowVisible` therefore reports it as visible, so
`package_smoke.visible_windows`, which counts every same-process top-level
window with that flag, would see it next to the real window. Slint 1.18.1 sets
no Win32 class name, so its window keeps winit's default `Window Class`. Its
Windows OpenGL probe window is created hidden and destroyed. The app UI uses
no native menu bar, tray or popup window.

The standard Windows runner confirmed both families in all eight snapshots in
[run 39145646856](https://github.com/brk275/Morpheus/actions/runs/39145646856),
using source `0677707285c50967b71e8155c77d5bbae7800f5a`. Both windows had
nonzero area: the requested creation size does not establish runtime geometry.
The probe exited normally and its job cleanup passed. The installed app is
also built with native calls, whose libraries this probe does not load.

## Files

- `crates/morpheus-app/examples/windows_window_probe.rs`: one ordinary Slint
  window using the default backend selection, built as a GUI-subsystem
  program. It uses no project crate and no Win32 calls of its own, closes
  itself after 30 seconds, and an independent watchdog thread exits with
  code 3 at 45 seconds.
- `scripts/windows_window_probe.py`: the driver. It launches the example in
  the reviewed kill-on-close `WindowsJob` with a minimal machine environment
  and a private temporary directory, then makes read-only `user32`/`dwmapi`
  queries.
- `scripts/tests/test_windows_window_probe.py`: mocked contracts. No Win32
  call or process is used.

## Guards and limits

- Runs only on a GitHub-hosted Windows runner with a reviewed `ImageOS`
  (`win25` or `win25-vs2026`) and `MORPHEUS_WINDOWS_WINDOW_PROBE=1`.
- Accepts only `target\{debug,release}\examples\windows_window_probe.exe`
  under this checkout.
- Waits up to 20 seconds for a visible, nonzero-area Slint subject window (the
  event target alone does not start sampling), then takes 8 snapshots
  0.5 seconds apart, at most 64 windows each. It then waits up to 50 seconds
  for the example's own normal exit, reserves 20 seconds for cleanup, requires
  an empty job, and closes the job before releasing its log or temporary tree.
  The run budget is 100 seconds. Hosted supervision supplies an outer deadline.
- Rechecks each borrowed window handle's process ownership before and after
  metadata queries. Vanished windows are counted; foreign ownership fails.
- Sends no input and no window messages, and starts no profile, account,
  vault, network or capture work.
- Reads class names only to map exact known names to fixed families
  (`winit_window`, `winit_event_target`, `ime`, `text_services`, `ole`,
  `other`). It never reads titles or UI text.

## Command

On a fresh standard hosted Windows runner, after review:

```text
cargo build --locked -p morpheus-app --example windows_window_probe
$env:MORPHEUS_WINDOWS_WINDOW_PROBE = "1"
python scripts\windows_window_probe.py "$PWD\target\debug\examples\windows_window_probe.exe"
```

The build uses the app's default features, so no native calls. Output is one
JSON line, re-validated before printing:

- visible-window counts across snapshots, and `driver_rule`: whether the
  keyboard driver's exactly-one rule would have held;
- deduplicated visible-window kinds: family, named style and extended-style
  flags, owner kind, zero or nonzero area, DWM cloaked state, and how many
  snapshots each kind appeared in;
- hidden-window families, the example's exit token and cleanup.

It never prints handles, process IDs, paths, raw class names or exception
text. `status=passed` means the evidence was collected, not that one window
was found.

## What it cannot prove

- That the installed native-calls release has no further visible windows. A
  result of exactly `winit_window` plus `winit_event_target` supports only the
  winit hypothesis for the default build.
- That the installed keyboard selection and interaction checks pass. They need
  separate source review and installed-package acceptance.

## Installed keyboard selection

The installed driver excludes only the pinned event target's exact class,
complete raw style words (`0x90000000` and `0x080800a0`) and absent owner.
Geometry is not an identity check. It rechecks the borrowed handle's process
ownership before and after queries. A different class, extra style bit or owner
keeps the window as a candidate. Two candidates remain an error; enumeration order
never chooses one. An event target alone keeps the driver waiting for the app.

Diagnostics expose candidate and ignored-target counts, each saturating at
two, and the window inventory below, alongside the existing fixed fields.
Successful interaction requires an exact candidate count of one. Raw names,
titles, coordinates, handles and process IDs are never published. A vanished
window can be retried; a live metadata failure or foreign handle fails the
check.

This selector change still requires installed-package verification. The tiny
default-feature subject does not establish the complete
window set of an installed native-calls build.

## Installed window inventory

[Package run 39150595802](https://github.com/brk275/Morpheus/actions/runs/39150595802)
built `e7031d5bf80595f021feb266288d326d9d110158`. Its installed launch,
NSIS install and registration passed, then the keyboard driver failed at
`step=window` with `window_count`, `window_candidates=2`,
`event_targets_ignored=0` and `owned_window_seen=false`.

### What the sources establish

- The driver picks a window only through `owned_window`. Each poll calls
  `keyboard_window_candidates`, which takes `package_smoke.visible_windows`
  (same process, `IsWindowVisible`) and classifies each handle with
  `keyboard_window_kind`. The first poll with any candidate decides: exactly
  one is selected, otherwise `window_count`. The driver and this probe share
  the read-only `windows_window_probe.Win32` queries, which mask style words
  to 32 bits. The plain launch smoke only waits for a visible window.
- `event_targets_ignored` is the maximum over every poll. A value of 0 means
  that no visible window matched the event target's complete expected
  signature in any poll. Pinned winit 0.30.13 creates that window, visible,
  when the event loop is built, so the expected match never happened.
- Morpheus calls `AppWindow::new()` and `show()` near the start of `main`, but
  Slint 1.18.1 creates the native window only once `run()` starts the event
  loop, after the rest of startup. A mismatched event target visible alone
  for a whole poll would have been selected as the only candidate. The
  failure means the first poll with any candidate already had two.
- The matcher's words `0x90000000` and `0x080800a0` come from winit's
  requested values. The window is created as `WS_OVERLAPPED` and then given
  `WS_VISIBLE | WS_POPUP` through `SetWindowLongPtr`. No hosted run has
  recorded the resulting raw words. This probe reports only named flags, and
  its tables omit bits such as `WS_CLIPSIBLINGS` (`0x04000000`) and
  `WS_EX_WINDOWEDGE` (`0x00000100`). Its nonzero event-target area is
  consistent with the overlapped creation's minimum tracking size.
- The installed build differs from the probe subject by the `native-calls`
  feature. That feature adds no window creation to Morpheus's own startup:
  the `morpheus_rtc::install_crypto_provider()` call only selects a TLS
  provider, and audio device discovery runs only on request. The app defines
  one Slint window and no popup window, menu bar or tray. glutin's WGL dummy
  window is created hidden.

### Hypotheses, not findings

- Windows may keep `WS_CLIPSIBLINGS` when the style word of a top-level window
  is replaced. Wine's reimplementation does so explicitly. If Windows does
  too, the event target reports `0x94000000` and stays a candidate next to
  the real window. This is the leading explanation. It is unverified.
- Alternatively, an extended-style bit differs, or a window not visible in the
  default-feature probe appears in the installed native-calls build.

The selector stays unchanged until raw evidence decides between these.

### Diagnostic

When `owned_window` decides (one candidate, `window_count` or `wait_timeout`),
it records `window_inventory` for the handles that same poll enumerated. The
inventory only reports. It never selects, excludes or retries a window, and
any error inside it becomes `status=failed` without changing the outcome.

For each handle the inventory does the following:

- It requires the task-owned process before and after the queries. A vanished
  or reused handle keeps no facts and only increments `discarded`, which
  saturates at 2.
- It publishes at most eight entries, sorted so enumeration order is not
  reported. More entries set `truncated`.

Each entry has:

- `family`: `winit_window`, `winit_event_target`, `ime`, `text_services`,
  `ole` or `other`. The raw class name is never kept.
- `style` and `exstyle`: the complete 32-bit words as integers.
- `visible`, `cloaked` (DWM), and `owner`: `none`, `same_process` or
  `other_process`.
- `width` and `height`: clamped to 0–32767. Positions are never reported.

A fact whose query failed on a live owned window is `null`. The smoke
validates every key, token and range before printing the record. Titles,
handles, process IDs, positions, paths, environment, application logs and
screenshots are never read into it.

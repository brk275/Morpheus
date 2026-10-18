# Windows window probe

The installed Windows keyboard check failed at `step=window`,
`failure=window_count`: the release process had **more than one** visible
top-level window of its own, while
`scripts/windows_package_ui.py` requires exactly one. This probe gathers cheap
evidence about which windows those are, without a release build, an installer,
a profile or Morpheus application code. It does not change the keyboard driver
or production window selection.

## Hypothesis from the pinned sources

winit 0.30.13 creates a per-thread "Winit Thread Event Target" window when its
event loop starts (`platform_impl/windows/event_loop.rs`,
`create_event_target_window`). It is a top-level, unowned, zero-size popup with
`WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW`, and
winit then sets `WS_VISIBLE | WS_POPUP` so it receives `WM_PAINT` during
resizes. `IsWindowVisible` therefore reports it as visible, so
`package_smoke.visible_windows`, which counts every same-process top-level
window with that flag, would see it next to the real window. Slint 1.18.1 sets
no Win32 class name, so its window keeps winit's default `Window Class`. Its
Windows OpenGL probe window is created hidden and destroyed. The app UI uses
no native menu bar, tray or popup window.

This is unproven on a hosted runner. The installed app is also built with
native calls, whose libraries this probe does not load.

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
- Waits up to 20 seconds for a first visible window, then takes 8 snapshots
  0.5 seconds apart, at most 64 windows each. It then waits up to 50 seconds
  for the example's own exit, requires an empty job, and always closes the job.
  The whole run is bounded at 100 seconds.
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
- Which window the keyboard driver should choose. Any production change still
  needs its own review.

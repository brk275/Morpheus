# Native timeline 10,000-row stress check

This is a measured **offline-preview** check from 2026-10-10. It does not
satisfy the [private beta gate](../docs/RELEASE_CHECKLIST.md) for a real
10,000-message Matrix room.

## Setup and data

- Host: macOS 26.7, Apple Silicon `Mac15,6`, 18 GiB RAM. Four old recovery/SAS
  QA app processes and the notification QA app were closed before this run.
  The sole Morpheus process used an isolated QA profile with no Matrix account.
- App: debug `morpheus-app`, Slint 1.18.1, FemtoVG/OpenGL renderer. The tested
  ad-hoc-signed QA executable has SHA-256
  `59ba64764b0a2e874723092b683f71189d8e5717fa5f272c42ef27283cc16840`
  (its pre-signing build has SHA-256
  `db7d51aa8bb9fd863af6d8afe5a0110d4b5fa140dfaf13a9fbc8487b5524c3ac`).
- In the app, **Explore the interactive preview → Settings → Load 10,000
  preview messages → #general** generated rows `#00001` through `#10000`.
  [`make_stress_messages`](../crates/morpheus-app/src/main.rs) cycles four
  body lengths and adds a reply strip every seventh row, an attachment every
  eleventh, and a reaction every fifth. The Slint timeline uses one repeated
  row in a `ListView`, with wrapped text and conditional row sections.
- `SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` captured rendered-frame
  counts. Computer Use supplied the UI actions and full macOS accessibility
  trees. Each reported interaction time starts before the action and ends
  after its accessibility observation, so it includes automation and
  observation time; it is an upper bound on app response, **not** a frame time.

## Observations

| Check | Result |
| --- | --- |
| First load | “10,000 synthetic messages loaded” appeared within **531 ms** of click plus accessibility observation; a second load took **608 ms**. Opening `#general` and observing the first rows took **730 ms**. |
| Materialized-row proxy | The full accessibility tree exposed **6** numbered message rows at the top (`#00001`–`#00006`), **6** near the middle (`#05199`–`#05204`), and **6** at the end (`#09995`–`#10000`). A ten-page scroll burst exposed **5–7** consecutive numbered rows per sample, never all 10,000. |
| Page scrolling | In a ten-page downward burst, the first scroll plus full accessibility observation took **1,486 ms**; the next nine took **589–753 ms** each. Sampled visible ranges advanced from `#00009`–`#00015` through `#00088`–`#00092`, with no blank or repeated row in each sampled view. |
| Deep seeking | Dragging the scrollbar to the end, then middle, then top reached the expected numbered rows in **1,524 ms**, **1,653 ms**, and **1,320 ms** respectively, including the accessibility observation. The screenshot at the end showed wrapped and conditional content rather than fixed-height placeholders. |
| Heap allocation | `vmmap -summary` reported **18.1 MiB** allocated in malloc zones while loaded, **12.3 MiB** after restoring normal preview, and **18.0 MiB** after loading again: a repeatable approximately **5.7–5.8 MiB** difference. RSS was volatile under macOS compression and is unsuitable as a precise comparison here. |
| Renderer log | In seven active one-second windows during the ten-page burst, lazy mode recorded **17, 35, 15, 34, 15, 30, 20** rendered frames. Idle windows recorded zero by design. These counts do not establish a sustained FPS or per-frame latency. |

The row count is an accessibility proxy, not a direct instrumented
`Repeater::len()` measurement. Slint 1.18.1's pinned `update_visible_instances`
implementation creates rows to fill the viewport and removes off-screen
instances; its `Repeater::visit` walks those instances for the accessibility
tree. The observed 5–7 numbered rows are consistent with that implementation
and strongly exclude 10,000 simultaneously exposed delegates on this view.

## Gate still open

Repeat this on a server-backed, 10,000-event room through both Matrix sync
adapters, then measure frame times and interaction latency under the supported
macOS, Windows, and Linux packages. The offline preview avoids network,
decryption, pagination, and real event formatting costs, so its result cannot
be used as a claim that the full beta performance gate has passed.

## Real-room acceptance harness

The optimized instrumented app compiled in
[run 39155273734](https://github.com/brk275/Morpheus/actions/runs/39155273734)
at source `92fc431ab7bd71e0b7021540ae4a710e68d1b085`, but the live workload
failed. The outer fixture runner withheld the inner failure summary along
with its private log, so this run does not establish the failing stage or any
performance result. The real-room performance gate remains open.

Failure summaries now use explicit vocabulary for every published field.
`failed_summary` accepts one exact failed record after a caller scans its
private log; arbitrary lowercase strings, extra fields, duplicate records,
oversized records and success claims are refused. The outer fixture runner now
forwards this validated record for a failed `timeline_10k` command after its
independent log scan and before raising. Scanner failure withholds diagnostics.
No other command forwards these records, and raw logs remain private. Synthetic
forwarding checks do not prove that the live workload succeeds.

### Instrumented build

```sh
MORPHEUS_TIMELINE_ACCEPTANCE_BUILD=1 cargo build --release --locked \
  -p morpheus-app --features native-calls
```

This is the shipping build command plus one variable. With it,
[`build.rs`](../crates/morpheus-app/build.rs) sets the registered
`cfg(morpheus_timeline_acceptance)` and refuses any profile except release.
It also gives the build its own persistence cohort, so it never shares one
with a shipping build. Ordinary builds compile none of the acceptance code,
and the instrumented binary refuses to start without a valid run file.

Its only differences from shipping are:

- the acceptance driver;
- a rendering notifier on the main window;
- a generated profile root instead of the standard app directories;
- an explicitly requested sync policy: the shipping `Auto` for sliding sync,
  or `ClassicOnly`, which forces the same classic `/sync` loop that `Auto`
  falls back to.

The renderer is still the one Slint's default backend selects, with no
backend or renderer override. The `AppWindow` and the `LiveUi` controller are
the real ones. Every report states `shipping_acceptance: false`. It is
instrumented-build evidence, never installed-package acceptance.

### Fixture and steps

[`scripts/run_timeline_acceptance.py`](../scripts/run_timeline_acceptance.py)
runs only on a fresh standard GitHub-hosted Ubuntu 24.04 runner. It must run
inside `dbus-run-session` with a throwaway unlocked keyring and an X display,
after `scripts/local_matrix_env.py up`. The app and the seeder accept only
loopback homeservers and generated `.test` users from that fixture. Bob is
measured and Carol is the account-switch target. Profiles, run files, reports
and raw logs stay in a private directory outside the checkout. They are never
printed or uploaded.

1. **Enroll** one fresh generated profile per adapter. Carol signs in first,
   then Bob, so startup restores Bob. Each enrollment writes Bob's own device
   ID for that profile to a new owner-only file the run file names, separate
   from its closed report. The runner accepts one plausible, distinct ID per
   profile and hands both to the seeder in another private file. The Unix
   seeder opens it without following final symlinks or blocking on special
   files, and requires a private regular file of at most 4 KiB outside the
   checkout. Directory aliases into the checkout are rejected.
2. **Seed once.**
   [`timeline_10k_local`](../crates/morpheus-matrix/tests/timeline_10k_local.rs)
   uses the pinned SDK:
   - Alice creates a new private encrypted room and invites Bob.
   - Bob joins from a disposable device.
   - Before the first message, Alice must see Bob's join in his member state
     and hold identity and Olm keys for exactly the two enrolled devices and
     the seeder device. No device count is used: other devices of Bob's could
     meet one.
   - Alice then sends exactly 10,000 messages, paced below the fixture's
     unchanged limit of 10 messages per second. That takes about 18 minutes.
   - The bodies cycle through seven deterministic kinds: short, two and nine
     wrapped sentences, four extra lines, a Markdown list, a code block and a
     quote. Each carries `tl10k-NNNNN`.
   - The fixture has no replies, reactions, edits, attachments or images.
3. **Measure** each profile through production paths only:
   - the startup restore;
   - the `open-room` and `load-older-messages` callbacks, until the start of
     history and all 10,000 events have decrypted;
   - wheel events dispatched in process over the timeline, 120 steps of
     480 logical pixels in each direction;
   - the `switch-account` callback to Carol and back, then reopening the room.

Each run must observe its own adapter running: `sliding_sticky` or `classic`.
Every validated report is closed, with fixed keys, tokens and numbers and
never IDs or text. The runner prints only a summary built from those reports.

### Metric labels

| Report field | What it is | What it is not |
| --- | --- | --- |
| `startup_ms.window_shown` | Process start until `AppWindow::show()` returned | A painted or presented window |
| `startup_ms.first_render_hook` | Process start until the first `AfterRendering` callback | First presented frame |
| `startup_ms.sync_running` | Process start until the account manager first reported its adapter running | Room list or timeline readiness |
| `render_hook_span_us` | CPU time between Slint's `BeforeRendering` and `AfterRendering` callbacks; `AfterRendering` fires before the back buffer is presented | GPU time, presentation or a displayed frame |
| `render_hook_interval_us` | Time between consecutive `AfterRendering` callbacks during the scroll steps, including waits for the next dispatched event | Display refresh interval or FPS |
| `dispatch_to_render_hook_us` | In-process wheel-event dispatch to the next `AfterRendering` | OS input latency, presentation or automation timing |
| `steps_accepted` | Wheel events an element, such as the timeline list, consumed | Proof of a particular scroll distance |
| `*_observed*` | First seen by a 5 ms event-loop poll; an upper bound | A precise event time |
| `request_until_loading_cleared_ms` | Production load-older callback until its loading flag clears; rows may publish slightly later | Time to render the new rows |
| `fixture_distinct_decrypted` | Direct count of distinct fixture events in the window's message model; undecryptable events never enter it | A delegate count |
| `rss_kib` | `/proc/self/status` on Linux; `unavailable` elsewhere | Heap size, or RSS on macOS or Windows |
| `delegates` | `unmeasured`: Slint exposes no public count of instantiated rows | Evidence for or against full instantiation |

The accessibility row counts in the offline record above remain a proxy.

### Still needed before claiming the gate

- **Hosted Linux runs.** Run both adapters on a fresh hosted Ubuntu runner and
  retain the validated reports.
- **macOS and Windows.** Their hosted runners have no loopback homeserver for
  this fixture. Those runs need a reviewed way to reach a synthetic server,
  plus external RSS sampling.
- **Installed packages.** The instrumented binary is not the installed package.
  Installed-package performance needs its own path.
- **Owner thresholds.** No pass or fail threshold is defined. The owner must
  set responsiveness thresholds from measured evidence.
- **Unmeasured quantities.** Delegate instantiation, GPU time, presentation and
  display refresh are not measured.
- **Fixture variety.** Replies, reactions, edits, attachments and images are
  not in the fixture.
- **Device key timing.** The exact-device wait only guards key distribution.
  The authoritative check is that all 10,000 events decrypt in each profile.
  An enrolled app that quits before publishing its device keys fails the seed
  at `enrolled_device_absent`; enrollment does not yet wait for that itself.

### Seed run 39165425640: the `devices` stage

The optimized seed failed at the fixed stage `devices`. That stage covered
three causes: a device query error, Bob's join not being seen, and too few
devices. Source review of the pinned SDK and fixture homeserver finds one of
them is certain:

- the old wait required `room.joined_members_count() >= 2`;
- in the pinned SDK, classic `/sync` updates that count only from the
  response's room summary;
- Synapse v1.162.0 sends a room summary only when the sync filter lazy-loads
  members, and the seeder's `sync_once` uses no filter.

The count therefore never changed, and the wait could only time out, whatever
devices existed. The seeder now reads Bob's own member event instead. Each
cause also has its own closed stage: `member_query` and `devices_query` for
query errors, `bob_join`, `seeder_device_absent`, `seeder_device_keys`,
`enrolled_device_absent` and `enrolled_device_keys`. A wait reports the first
unmet one, in that order. Malformed enrolled device input fails at
`bob_device_ids`. No retry was added; the next hosted run shows whether
anything remains.
- **Workflow integration.** No workflow runs this yet.

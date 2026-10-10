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

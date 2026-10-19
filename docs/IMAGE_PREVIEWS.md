# Native image previews

An explicit image viewer is implemented in the room timeline and thread replies. Native builds and its 13 focused guards passed on all four platforms at `4fd8071`; real viewer and installed acceptance remain pending.

## Behavior and privacy

- An image attachment has a native **Preview image** card/button. The image is fetched only after a click. No thumbnail or body URL is loaded automatically.
- The native viewer fits the image to its panel, never enlarging a small one past its own pixels. **Close** and **Escape** release it and return focus to the app navigation/shortcut scope. Programmatic navigation/privacy closes do not move focus.
- Implemented in source, not yet compiled or run: zoom from fit up to 8× with the buttons, the mouse wheel, double-click, or `+`, `-` and `0`, and pan by dragging or with the arrow keys while zoomed. **Previous**/**Next** (or Left/Right at fit) open the neighbouring image among the room's or open thread's already loaded messages, and show "3 of 12". Each step is another explicit request; neighbours are never prefetched and no more history is loaded. **Cancel** stops a load in progress without closing the viewer, and **Try again** reloads after a cancelled or unavailable load. **Save…** uses the existing attachment save. Tab moves between Close, Save and the loading action.
- `media.previews_in_encrypted_rooms` remains `false` by default. Settings exposes the same persisted preference. An encrypted room, encrypted message metadata, or an encrypted attachment each require opt-in. Unknown room encryption fails closed. Disabling the preference clears/revokes any current preview; re-enabling cannot revive the old request.
- Closing, switching rooms/accounts/threads, leaving the chat page, opening sign-in, and shutdown clear the viewer. A revoked account selection stays revoked across A→B→A switching. Old results cannot replace a newer viewer.
- Source URLs and attachment keys stay inside the Matrix adapter. It resolves the actual authenticated room event, verifies that it is an image, and uses the existing bounded Matrix media endpoint/redirect policy. Text/body URLs and arbitrary external URLs are not inputs.
- Downloaded/decrypted preview bytes and pixels stay in memory. No preview file or disk media cache is created. The existing explicit **Save** attachment action is independent. It always asks for the location, starting in the user's Downloads folder, and suggests only a sanitized final name component from the sender's filename. Memory owned by the worker is zeroed where practical; clearing the image is not a guarantee that the OS, allocator or GPU has erased every copy.

## Supported inputs and limits

Static PNG up to 8 bits per channel (including indexed/grayscale expansion), and 8-bit baseline JPEG with one complete interleaved scan and one or three components are supported. Images must be at most **8 MiB**, **4096 pixels per side**, and **4,194,304 pixels**. The body limit is enforced while streaming even if the event and HTTP response omit a size.

PNG framing checks dimensions before constructing the decoder. It strips ancillary metadata without decompressing it, rejects animation, bounds palette/transparency chunks, and configures the pinned PNG decoder's 32 MiB allocation budget before header parsing. The budget applies to allocations tracked by that decoder; it is not a whole-process/RSS guarantee.

JPEG framing checks dimensions, components, sampling and the single-scan structure before invoking the pinned decoder. EXIF, ICC, comments and other large metadata are removed before parsing. Small JFIF/Adobe color hints are retained. The decoder receives width/height and scan limits before header parsing. EXIF orientation and ICC color management are not applied.

There is one application download/decode lease, one dedicated decryption/decoder thread, at most one pending result, and one displayed image; no decoded-image cache. The network phase has a 45-second deadline. Cancellation stops publication immediately and drops its network future. An already entered decryption/codec operation cannot be recalled, so it retains the sole worker permit until it exits. Existing SDK/HTTP resolver internals retain their own implementation behavior; this is not a process-wide thread guarantee. The native image owns an additional display buffer and may allocate GPU resources.

Animated PNG/GIF, progressive or multi-scan JPEG, WebP, SVG, 16-bit PNG, audio and video previews remain unsupported. Attachments can still be saved through the existing action.

## Verification boundary

Thirteen focused regressions passed in hosted Rust suites on Apple Silicon, Intel Mac, Windows and Linux at `4fd8071`. They cover actual PNG and baseline JPEG pixels, metadata stripping, animation/size/dimension rejection, ciphertext tampering and EOF hash verification, streamed body limits, privacy combinations, permanent cancellation, worker permit ownership, queued-pixel clearing and stale view/account selection rejection. No local native build or test was used.

The expanded `media_local` encrypted/plain-image API fixture did not run in that candidate because an earlier formatting check failed. No real image-download/preview API pass follows from the unit results. Real encrypted and unencrypted attachment checks, opt-in/off reload, rapid account/room switching during load, viewer keyboard/accessibility behavior, failed/oversized images and installed-package acceptance remain required.

## Attachment transfer work in progress

The new per-room queue is under source review. Files are staged until an
explicit send. Preparation is limited to two tasks, and cancelled blocking
reads retain their own admission permit until the read exits. A send belongs
to the exact account selection that started it. Cancellation before SDK
admission queues nothing; after admission the app awaits the handle and
reports its actual abort result. An SDK admission error or account departure
ends observation as delivery unconfirmed, which cannot be resent with Retry.
The SDK owns already admitted uploads; changing rooms alone does not stop them.

Save cancellation is rechecked after the account and room commit guards. Once
the local write is admitted, it finishes and reports its actual outcome. The
source includes focused cancellation and selection regression cases; these
changes have not yet passed hosted or installed acceptance.

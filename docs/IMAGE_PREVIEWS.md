# Native image previews

An explicit image viewer is implemented in the room timeline and thread replies. Native builds and its 13 focused guards passed on all four platforms at `4fd8071`; real viewer and installed acceptance remain pending. GIF and WebP previews, animated playback, progressive JPEG, EXIF orientation, 16-bit still PNG, rotation, pointer-centred zoom and the extra viewer keys described below are source only: they have not been built, tested or run.

## Behavior and privacy

- An image attachment has a native **Preview image** card/button. The image is fetched only after a click. No thumbnail or body URL is loaded automatically.
- The native viewer fits the image to its panel, never enlarging a small one past its own pixels. **Close** and **Escape** release it and return focus to the app navigation/shortcut scope. Programmatic navigation/privacy closes do not move focus.
- The viewer supports zoom from fit up to 8× with the buttons, the mouse wheel, double-click, or `+`, `-` and `0`, and pan by dragging or with the arrow keys while zoomed. **Previous**/**Next** (or Left/Right at fit) open the neighbouring image among the room's or open thread's already loaded messages, and show "3 of 12". Each step is another explicit request; neighbours are never prefetched and no more history is loaded. **Cancel** stops a load in progress without closing the viewer, and **Try again** reloads after a cancelled or unavailable load. **Save…** uses the existing attachment save. Tab and Shift+Tab reach every available viewer button, skipping disabled actions.
- In source, not yet built or run: the wheel and double-click keep the point under the pointer in place, and Home/End open the first/last loaded image (still one explicit request each). Tab and Shift+Tab stay inside the viewer in layout order: Close, Save, Previous, Next, Play/Pause, Replay, Cancel or Try again, Zoom out, Fit and Zoom in. If a focused action becomes unavailable (for example Next at the last image, or Play when a picture turns out to be still), focus moves to the next available one. **Rotate** (or `]`, and `[` for counterclockwise) turns the view a quarter turn and fits the turned picture; fit and panning use the turned extent. Rotate is a pointer and assistive-technology button outside the Tab ring, so the existing ring keeps its order; the keys work from anywhere in the dialog. The rotation resets when another image opens, and Save still writes the original file. The dialog's accessible description lists its keys, and the image's label gives its position, zoom, rotation and playback state.
- `media.previews_in_encrypted_rooms` remains `false` by default. Settings exposes the same persisted preference. An encrypted room, encrypted message metadata, or an encrypted attachment each require opt-in. Unknown room encryption fails closed. Disabling the preference clears/revokes any current preview; re-enabling cannot revive the old request.
- Closing, switching rooms/accounts/threads, leaving the chat page, opening sign-in, and shutdown clear the viewer. A revoked account selection stays revoked across A→B→A switching. Old results cannot replace a newer viewer.
- Source URLs and attachment keys stay inside the Matrix adapter. It resolves the actual authenticated room event, verifies that it is an image, and uses the existing bounded Matrix media endpoint/redirect policy. Text/body URLs and arbitrary external URLs are not inputs.
- Downloaded/decrypted preview bytes and pixels stay in memory. No preview file or disk media cache is created. The existing explicit **Save** attachment action is independent. It always asks for the location, starting in the user's Downloads folder, and suggests only a sanitized final name component from the sender's filename. Memory owned by the worker is zeroed where practical; clearing the image is not a guarantee that the OS, allocator or GPU has erased every copy.

## Supported inputs and limits

Static PNG up to 8 bits per channel (including indexed/grayscale expansion), and 8-bit baseline JPEG with one complete interleaved scan and one or three components are supported. Images must be at most **8 MiB**, **4096 pixels per side**, and **4,194,304 pixels**. The body limit is enforced while streaming even if the event and HTTP response omit a size. In source, not yet built or tested: still PNG up to 16 bits per channel (16-bit samples are rounded to 8 bits after decoding; formats of four or more bytes per pixel are compacted in place, and 16-bit gray expands into one new RGBA buffer), 8-bit progressive (Huffman) JPEG, GIF and WebP, under the same limits.

PNG framing checks dimensions before constructing the decoder. It strips ancillary metadata without decompressing it, rejects animation, bounds palette/transparency chunks, and configures the pinned PNG decoder's 32 MiB allocation budget before header parsing. The budget applies to allocations tracked by that decoder; it is not a whole-process/RSS guarantee.

JPEG framing checks dimensions, components, sampling and the single-scan structure before invoking the pinned decoder. EXIF, ICC, comments and other large metadata are removed before parsing. Small JFIF/Adobe color hints are retained. The decoder receives width/height and scan limits before header parsing. ICC color management is not applied.

In source, not yet built or tested: a progressive (SOF2, Huffman) JPEG may have up to 64 scans, and the decoder gets the same cap. Each scan header is validated: a DC scan of up to every component or an AC scan of exactly one, each a component of the frame named once, table selectors 0–3 and approximation bits in range. Only Huffman and restart tables may come between scans, and metadata after the first scan is rejected. The progressive coefficient buffers (up to about 24 MiB at the 4 MP limit) are outside the PNG allocation budget. The orientation tag of a JPEG's first Exif segment, or of a still PNG's first `eXIf` chunk, is read with the image crate's bounded Exif orientation parser before that metadata is discarded, and the decoded pixels are turned or flipped upright into a fresh zeroizing buffer. Later Exif segments and every other EXIF value are not read; animated PNG and WebP orientation is not applied.

There is one application download/decode lease, one dedicated decryption/decoder thread, at most one pending result, and one displayed image; no decoded-image cache. The network phase has a 45-second deadline. Cancellation stops publication immediately and drops its network future. An already entered decryption/codec operation cannot be recalled, so it retains the sole worker permit until it exits. Existing SDK/HTTP resolver internals retain their own implementation behavior; this is not a process-wide thread guarantee. The native image owns an additional display buffer and may allocate GPU resources.

### GIF, WebP and animation (source only)

GIF and WebP decode through the pinned `image` 0.25.10 codecs, whose `gif` and `webp` features this source enables (dependency edges to the already locked `gif` 0.14.2, `color_quant` 1.1.0 and `image-webp` 0.2.4; no new package, version or source). Animated PNG uses the already enabled PNG codec. Only the viewer decodes these formats; avatars, stickers and other previews stay on the still PNG/JPEG path. None of this has been built, tested or run.

- Before any decoder runs, a structural check walks the whole file once and rebuilds it; no pixel work happens until it passes. A GIF must have a logical screen within the limits, every image inside that screen with a colour table and valid code size, at most one graphic control per image, at most 4096 blocks and an exact trailer.
- A WebP must have a RIFF length equal to the admitted input, and every chunk and every frame subchunk must fit within it. An animated file needs its VP8X canvas within the limits, one ANIM chunk before its frames, and every ANMF frame inside the canvas. Each frame holds an optional alpha plane only directly before a lossy bitstream, exactly one VP8 or VP8L bitstream, then at most 64 well-formed unknown chunks. Every embedded VP8 and VP8L width and height must equal its frame's (or a still's canvas) before any decoder sees it, even when the outer sizes look small. The pinned WebP decoder sizes its padded YUV planes from those inner sizes before its own consistency check. A lossy bitstream's first partition must also fit its chunk, and an uncompressed alpha plane must hold exactly one byte per pixel.
- Comments, plain text, application extensions, ICCP, EXIF, XMP and unknown chunks are left out unread, so no decoder holds them and no metadata read can allocate from a declared but missing range. The viewer never asks a codec for orientation or metadata.
- An animated PNG passes the same chunk sanitizer as a still PNG plus these frame checks:
  - one `acTL` before any image data, declaring at most 1000 frames
  - exactly that many `fcTL` frames, each a non-empty rectangle inside the image (checked arithmetic) with valid dispose and blend operations
  - a frame before the image data only when it is the whole image
  - `fdAT` data only after the image data and a frame control

  A still PNG keeps rejecting animation chunks. 16-bit animated PNG is unsupported.
- Frame counts are exact. More than 1000 frames is refused before decoding. A decoder that yields more frames than were counted, or stops before all of them, ends playback with an error and a note, never as the animation's natural end.
- An animation opens paused on its first frame unless `media.autoplay_animated_media` is on (Settings › Image previews › Play animated images automatically) and reduced motion is off. Both are read when playback would start, once the first frames are decoded, not when the image was clicked. Turning on reduced motion requests a pause even while the image is loading. The pause takes effect at the next wait; one frame already being decoded can finish first. Play/Pause (or K) and Replay (or R) control it, and your own Play still works under reduced motion.
- Each frame keeps its own delay; delays under 20 ms play at 100 ms.
- A GIF without a loop setting plays once, a loop count of n plays n + 1 times and 0 loops endlessly. WebP and PNG play their stated count (0 is endless). A finished animation stays on its last frame.
- Playback pauses itself after ten minutes of continuous play, before decoding anything further. This holds even when a single frame's own delay is longer; WebP and PNG allow delays of hours.
- A frame that fails to decode ends playback on the frame already shown, saying why.
- Playback runs on the same single dedicated worker and decode permit as still previews, which it holds until playback stops. Frames are decoded strictly in order from the rebuilt in-memory input, never collected ahead; looping and Replay decode again from the start. Only the newest picture waits for the window and at most one window update is queued.
- Commands reach the worker through a single slot. The newest command replaces one not yet taken, preserving the latest choice while the worker is busy.
- Closing, opening another image, switching room or account, or revoking the selection releases that slot and drops the waiting picture at once. The worker stops between frames, at its next wait; a frame it is already decoding finishes first. Any late picture fails the same token/account/room check as still previews. A new image waits up to three seconds for a stopping worker's permit before reporting busy.
- An animation plays only within these limits; otherwise it shows its first frame still, with a note saying which limit applied. The decision is made from the checked header and frame count before any later frame is decoded.
  - at most 4096 pixels per side and 2 megapixels
  - at most 1000 frames per pass
  - at most 536,870,912 canvas pixels per pass (frames times canvas), for example 256 frames at 2 MP or 1000 frames at half a megapixel
- Each shown picture is copied once into the window's buffer on the worker. With delays as short as 20 ms that is at most 50 pictures a second. Decoding that is slower than the delays can keep one core busy until the ten-minute pause.

These bounds are a conservative, finite complexity policy. They are not a hard memory total and not a realtime guarantee. The GIF and PNG codecs receive a 64 MiB `image::Limits` budget that covers only some of their own allocations, and the pinned WebP codec takes none. The figures below are a conservative peak estimate for one 2 MP animation, from reading the pinned decoders' source; nothing has measured them.

- **Input.** About 24 MiB while the input is checked: the 8 MiB plaintext, the rebuilt copy and one frame being rebuilt. After that, only the 8 MiB rebuilt input stays.
- **Codec, per frame.** The decoder's own 8 MiB canvas (two for animated PNG) and an 8 MiB output frame. The decoded frame is up to 8 MiB, plus a 6 MiB RGB-to-RGBA conversion where it applies.
  - VP8 adds its padded planes (about 3 MiB). It also copies the frame payload, the last partition twice. A token partition whose declared size (up to 16,777,215 bytes) is larger than the remaining data is allocated before its read fails, adding one such temporary.
  - An alpha plane adds up to 10 MiB.
  - VP8L adds its transform data and Huffman tables. That storage is bounded by the decoder's own limit on meta-Huffman groups, not by these checks; at 128 groups it is about 15 MiB plus overhead per stream.
- **Viewer.** The shown, next and waiting pictures, the window's copy and the window's image: about 40 MiB.

Together this is on the order of 120–140 MiB in the worst case, before allocator and GPU overhead. A first frame shown still from a canvas up to 4 MP needs a similar amount.

Arithmetic-coded JPEG, 16-bit animated PNG, SVG, audio and video previews remain unsupported. Attachments can still be saved through the existing action.

## Verification boundary

Thirteen focused regressions passed in hosted Rust suites on Apple Silicon, Intel Mac, Windows and Linux at `4fd8071`. They cover actual PNG and baseline JPEG pixels, metadata stripping, animation/size/dimension rejection, ciphertext tampering and EOF hash verification, streamed body limits, privacy combinations, permanent cancellation, worker permit ownership, queued-pixel clearing and stale view/account selection rejection. No local native build or test was used.

The source-only additions above add unit cases and generated-UI cases.

- **Unit cases:**
  - GIF and WebP framing: embedded bitstream sizes, first partitions, alpha pairing and trailing subchunks
  - animated PNG frame admission
  - exact frame counts, the per-pass budget and loop counts
  - the player's start decision, the forced pause on reduced motion, the command slot, and the ten-minute budget with long frame delays
  - progressive scan validation, orientation and 16-bit PNG
  - the gallery step and player shutdown
- **Generated-UI cases:** the viewer's keys, Tab ring, wheel zoom, playback actions and the autoplay setting. They use small synthetic images only and none of them has been built or run.

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
changes passed focused standalone hosted checks at `c95a668`: 18 core media regressions, six app attachment checks and one generated-UI privacy case. The combined beta candidate and installed behavior still need acceptance.

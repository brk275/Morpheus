# Native Slint video image proof

This isolated proof uses pinned Slint 1.18.1 and Morpheus's pinned RTC
`VideoFrame` type. A worker generates synthetic I420 frames, validates plane
strides and dimensions, converts them to RGB, and passes a one-frame bounded
buffer to a native Slint `Image`. It rotates the bars every three seconds.

Run on a desktop with a graphical session:

```sh
CARGO_TARGET_DIR=/tmp/morpheus-build cargo run --manifest-path experiments/native-video-image/Cargo.toml
```

Run the conversion tests without opening a window:

```sh
CARGO_TARGET_DIR=/tmp/morpheus-build cargo test --manifest-path experiments/native-video-image/Cargo.toml
```

This proof has no Matrix connection, camera, screen capture, or incoming media
track. A passing build proves only the Rust/Slint API boundary. Running and
visually inspecting the window proves local native image updates on that OS.
Neither result proves encrypted video transfer or performance in a call.

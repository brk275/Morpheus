# Owned native notification patch: tauri-winrt-notification

Upstream: tauri-winrt-notification 0.8.1, crates.io archive SHA256
`f37a6c354fd28fc9e322ed9bd47e3959576dad28c9d58ea1cf888cce1c7ccb36`. Original MIT and Apache-2.0 attribution and license texts are retained.

The additive route initializes and retires interfaces on the same native MTA
worker. Exact AUMID/tag/group identities, bounded actions, callback-entry/return
facts, original registration tokens and own-notification withdrawal are separate
observations. No fallback Show, Hide, broad history clear or retry is implied.
The normalized windows dependency adds only Win32_System_WinRT on the already
locked windows 0.62.2 package; no dependency version changes.

The managed normalized manifest omits only development-tool tables. The complete
published normalized manifest is preserved as Cargo.toml.upstream; normal/build
dependencies, features and default features stay unchanged except the explicitly
documented Windows bindings feature. Original tests, examples, licenses,
Cargo.toml.orig and upstream lock remain preserved.

The app must own the real native worker, retain finite identity obligations on
both success and error, and actually join worker/TLS and supervisor. Closed
admission alone is not completion. Whole-platform shutdown, real native API and
installed-update behavior still require verification; no distribution, signing
or complete OS-quiescence acceptance is established by source review.

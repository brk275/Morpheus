# Morpheus attachment event receiver patch

This is `matrix-rtc-call-sdk` from Element's Matrix Rust RTC revision
`53880ef1ef66b62df555ce71ccd1d6c85b813596` (`v0.5.0-rc.2`), path
`crates/matrix-rtc-call-sdk`. Every original package file is retained; the
original manifest is `Cargo.toml.upstream`, and its complete owning workspace
manifest is `Cargo.workspace.toml.upstream`. The managed manifest expands
workspace declarations and sibling paths to the SAME already locked source
selectors. No dependency version, normal feature, target, test, example or build
hook has been removed or added by that normalization.

`attach_media` subscribes before held-key replay and own-focus connection setup.
The native `LiveKitCall::join` previously discarded that original receiver. An
`Ended` emitted before the facade's caller subscribed could therefore be missed
permanently: the engine emits the end once and can retain its channel sender.
The facade now owns that ORIGINAL receiver until one facade subscriber takes
it under a mutex. The fresh-subscription fallback runs outside the mutex;
later facade subscriptions and direct `CallEngine` subscriptions are unchanged.
There is no additional queue, event replay, retry, timer, network request, key,
notification, authorization or media-state change. The original 256-event count
bound and `Lagged` behavior remain; durable or byte-bounded history is not
claimed. Leaving or dropping the facade drops an unclaimed receiver with it.

A tiny private helper is used by the production facade and the ordinary app
integration target `rtc_retained_events`. Its three literal cases exercise real
bounded Tokio broadcast receivers: two competing first consumers plus later and
reentrant fresh subscriptions, preservation of original lag, and separate-call
receiver/drop ownership. Test payloads are synthetic identifiers, not actual
cryptographic keys or native media. The existing vendor step lists and runs each
exact case using the app's existing `native-calls` graph. No upstream SDK unit,
example, e2e or dev-dependency graph is selected by this integration target; all
upstream declarations and tests remain intact. These focused cases prove the
helper boundary, not a real SFU outage, cryptographic rotation or compositor.

The old run `40124579405` established a failed DM case and did not identify its
failure phase. This source-proven race is NOT claimed as that run's cause.
Actual encrypted DM and three-participant SFU restart/media/key/no-extra-ring
acceptance remains a required separate existing fixture gate.

Original Element copyright and dual-license declarations are unchanged. Full
original `LICENSE` and `LICENSE-COMMERCIAL` from the same revision are included.
The latter grants nothing automatically; this patch makes no new commercial
license or legal distribution claim. New helper/test code is a Morpheus
AGPL-3.0-or-later contribution; it does not expand upstream commercial terms.

The original Apple-only `build.rs` still emits `-ObjC` for its declared examples
and tests. No build-hook or native/unsafe implementation is changed. The root
lock changes only this same-version SDK's Git source identity to the local
patch; all 1,174 package versions/dependency references remain unchanged.

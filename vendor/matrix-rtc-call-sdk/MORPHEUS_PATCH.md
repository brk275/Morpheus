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

## Standalone slot wire and admission

Standalone open/close now use the same `DialectBackend` as joined rooms, so the
canonical slot event type no longer depends on a nonexistent locked Ruma alias.
The admitted open entry retains the original account/room predicate through the
SDK backend's authority read and final state write. Existing low-level free APIs
remain available. The same-revision adapter's
[patch notes](../matrix-rtc-matrix-sdk/MORPHEUS_PATCH.md) bind stable-state
precedence, fixed read failures, request limits, single-write behavior and the
remaining stable-only stock-close/distributed-race limitations. No default,
membership/key/transport policy or original held-event ownership is changed.

Additional accepted-key sender adoption correction:

- Let MediaKeyHandler register and adopt the last successfully applied own index
  atomically. Remove the separate recorded-key snapshot/setter: a ring-refused
  key remains in that snapshot, and an independently read snapshot could also
  overwrite a newly activated rotation.
- Preserve key replay, identity mapping, original event receiver, connection
  ownership, delayed activation and all incoming/history/trust policy. Focused
  transport-module regressions exercise this same production handler registration
  with refused and held imports. Original SDK attachment tests remain byte-exact;
  no fixture/scanner policy changes or local test execution.

This source-proven rejected-index bug is not established as the cause of any
captured RTC first-frame failure. No native crypto readiness or success is claimed.

## Bounded media failure observations

Morpheus adds a read-only snapshot through the retained call and service. It
exposes only positive per-entry video counter lower bounds and explicit native
capture flags from a unique local camera. Missing or typed-zero counters remain
unknown. Source admission, encoded frames and sent packets are separate facts;
none establishes successful remote decryption or decoded media. No identifiers,
raw reports, keys or native handles leave this interface.

The existing synthetic outage fixture reads this snapshot only after its original
45-second media deadline failed, within one three-second observation window. Its
original failure, acceptance predicates, call authority and cleanup remain intact.
The app observer is Linux-test-only and fenced to the retained call generation.
Native report parsing is bounded separately in libwebrtc; source flags and stats
are independent readings, not an atomic snapshot across an internal reconnect.

## rc.7 hidden encrypted-video recovery probe

The private call engine backports `probe_undecryptable` from official revision
`5b8e73539878e40fa5bd88d637eb8eb2b6b2f0d0`. If an enabled video is hidden
while the member has a frame-key failure, it requests the lowest layer rather
than pausing all frames that could show recovery. The exact host constraints
are retained and reapplied when failure classification changes; leaving members
lose the retained request. An explicit release, bandwidth-saver choice, audio
request or visible-video request is not promoted. No new task, timer, key
request, crypto-state acceptance, public API or dependency is introduced.

The existing engine fake-transport suite covers failure after a hidden request,
Low-layer probing, exact dimensions/pause restoration, explicit release during
failure and recovery, plus the direct policy bounds. It requires its own
affected hosted execution; the vendor route now selects the SDK library suite
with `cargo test --locked -p matrix-rtc-call-sdk --lib`. Original `engine.rs` is retained byte-exact in
`engine.rs.upstream`. Existing app KeyImported handling remains unchanged, so
this is not a claim that a captured missing-key membership failure follows this
media path, nor a cure for unavailable historical membership keys. Encryption
admission, identity, trust, capture, cancellation, deadlines and release gates
are unchanged.

### Same-ID receiver restoration integration

A focused test in the existing engine module exercises the transport pool's
receiver-owned encryption-state replay after a membership lapse: the same live
receiver is restored, then a new hidden host request probes at the permitted low
layer without requiring a duplicate cryptor failure callback. Explicit release
and low-bandwidth requests retain their original precedence. The replay is a
participant-level observation, not proof of a particular cryptor or recovered
media. This integration adds no production engine API or lifetime beyond the
existing receiver, and has not been executed locally.

### Bandwidth-saver precedence adaptation

Morpheus adapts the rc.7 probe helper to return the complete host request
unchanged when `low_bandwidth` is set, before any hidden-video promotion.
The backported predicate previously retained that flag but changed visibility
and detail to Low, contradicting the helper's bandwidth-saver policy. The
ordinary constraint resolver still paused the video; its detail preference was
incorrectly overwritten. Explicit release, audio, visible-video and ordinary
hidden undecryptable-video behavior are unchanged.

The existing direct policy case now compares the complete limited request for
both video kinds. The combined same-receiver restoration case and all its
assertions remain unchanged: its whole resolved-constraint comparison exposed
this mismatch in hosted run `40522573451` (94 passed, one failed). This change
adds no timer, key request, crypto acceptance, dependency or public interface.
It is not a fix for unavailable historical membership keys or evidence that
any real media or call acceptance gate passed. No candidate was executed locally.

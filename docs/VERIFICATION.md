# Verification log

Morpheus is pre-alpha software with no approved installer release. This log records checks actually completed;
the [release matrix](FEATURE_MATRIX.md) remains the beta gate.

## Native and live acceptance: `51c04ce`

[Run 38725671739](https://github.com/brk275/Morpheus/actions/runs/38725671739)
tested `51c04ceec75538ff0bff3ef43cbfd3531870b3cb`. Linux and both Macs
completed successfully. Windows passed its native builds and unit suites but
failed the synthetic capture fixture. All jobs and cache/checkout post steps
finished; the final CI acceptance job correctly failed. Complete collected
platform logs scanned with zero findings. This candidate has not passed the
complete native matrix.

Each Mac passed 874 substantive unit/vendor/SDK tests; Windows passed 876 and
Linux passed 900. Windows has two additional platform-specific Matrix tests.
These counts exclude no-fixture integration early returns and pure diagnostic
checks. Each target retains one ignored live test. Helpers passed 501 tests
with one skip; the history scan found no secrets in 58 reachable commits.

Linux passed 20 substantive live Rust tests plus three repeated diagnostics,
with no failed or unrun checks. The staged `live_local` test reached `complete`.
Against two disposable homeservers and an isolated real OS credential vault,
it enabled account protection; rejected direct, wrong-password and cancelled
activation; switched A→B→A with cached isolation; reopened locked and unlocked;
removed inactive and active accounts with credential/store deletion; and
allowed password reset only after complete removal. Protected classic login
remained paused until authorization. This verifies the Linux fixture scope,
not Mac/Windows credential-vault or installed-app behavior.

All four Current/Sticky2025 DM/room ringing cases passed. Current/DM reached the
fresh-offer phase and exercised account A→B→A with the old offer rejected.
OAuth login/refresh and restore/refresh, all 13 room-settings facts, recovery,
native late joins and the stock Element Call peer's 11 browser/native facts
also passed. Real provider/browser handoff and physical audio/video devices
remain separate gates.

All nine native Linux preview captures passed visual inspection, including
Manage accounts and password dialogs at 100% and 200%. The real Slint keyboard
and pointer harness passed in these completed jobs, without binding-loop
warnings. Six unchanged PNGs are recorded in the
[screenshot gallery](screenshots/README.md), with exact source, run, artifact
and image identities. This is neither screen-reader nor interactive installed
acceptance. No installer was uploaded.

The next source batch adds exact-intent logout, disabled-keyboard guards,
scrollable sign-in, navigation choices and three more capture views. Those
changed inputs need their own native evidence; they do not inherit this run's
passing results. Signing, real devices, installation, updates, rollback and
VoiceOver/Narrator/Orca remain release gates.

### Windows capture failure

The fixed result was `failure=capture`, `stage=window_ready`, `status=101`.
That stage covers the fixture acknowledgement, owned PID/nonce window lookup,
source-key lookup, single-display check and full-cover geometry assertion.
`window_frames` was never reached, so no successful frame capture is established.
The private native test log was deleted during fixture cleanup; the exact
failed assertion cannot be recovered from the public result. Containment and
cleanup reported no failure. The next candidate adds fixed stage/reason
identifiers, bounded child exit codes and PowerShell preflight phases without
exposing raw logs, window titles, paths or pixels. Its 24 mocked helper tests
pass; hosted execution is pending. Geometry checks, process containment and
deadlines remain unchanged.

## Live acceptance stack overflow diagnosis, 2026-10-15

At `65b4ceb`, the `live_local` acceptance test stopped during `encrypted_restart`
without a classified cause. At `93d1358`, the added diagnostics confirmed a
stack overflow (SIGABRT) at `encrypted_restart_message` in Linux
[run 38246027915](https://github.com/brk275/Morpheus/actions/runs/38246027915).

Claude's contribution reports a local reproduction against two loopback
Synapse 1.162.0 homeservers and a disposable keyring. Its reported stack probe
at the unoptimized test profile measured about 1.1 MiB in the monolithic test's
poll frame before an SDK call, with the restored encrypted timeline exceeding
the default 2 MiB test-thread stack. Those local measurements have not been
independently reproduced in this review.

The test now runs each stage as its own `async fn` behind a slim coordinator,
with the original runtime phase order, assertions, timeouts and account
operations retained. Claude reports the complete test passing locally after
this change with a peak below 1.5 MiB. No stack size is raised. The source guard
checks entry/completion within the coordinator and still requires the exact
global phase allowlist with no duplicates; the runtime `complete` gate and
stack-overflow detection are unchanged. The merged test subsequently reached
`complete` in Linux run 38725671739, including the protected-account sequence
described above.

## Completed native run: `93d1358`

[Run 38246027915](https://github.com/brk275/Morpheus/actions/runs/38246027915)
tested `93d135844847b262679b3d5996fe7863b93ba2f1` on the four standard
supported runners. Every native build, model/controller/device suite, vendor
regression and SDK receipt check passed. Each Mac and Windows passed 855 tests;
Linux passed 881. These totals exclude input-free diagnostic tests and skipped
no-fixture integration paths. Each target retained one ignored live test.
The helper jobs passed 501 tests with one skip. The history scan found no
secrets in 56 commits; complete collected job logs also scanned clean.

The real Slint keyboard/account harness and system generic-font projections
passed on all four targets. Linux Xvfb startup, strict Wayland capture and
Windows synthetic-window/display capture passed. Nine native Linux screenshots
were reviewed: Nocturne, Terminal, Paper, sign-in, incoming call and room settings
passed the inspected visual scope. Manage accounts and both password-dialog
captures failed: stretched buttons squeezed the scrollable content. A subsequent
source fix bounds those buttons and gives the body its own scroll area; it still
needs fresh captures and keyboard/pointer checks. These images are not proof of
VoiceOver, Narrator or Orca acceptance.

Linux passed 18 substantive live Rust checks, plus three repeated diagnostic
checks. OAuth login/refresh and restore/refresh now passed, including vault
persistence and logout. Room settings passed all 13 facts after the disposable
fixture's rate-limit adjustment. The pre-synced stock Element Call peer again
passed all 11 facts. Two live checks still failed:

- `live_local`: confirmed stack overflow/SIGABRT at `encrypted_restart_message`;
  the protected-account sequence was not reached.
- Current/DM incoming calls: `account_fresh_offer` timed out before account
  switching. Discovery observed 20 passes with no verified receipts and 22
  malformed membership results; no admission or queue event occurred during
  that wait. The other three ringing cases passed.

Source review traced that discovery failure to a valid Current leave event
retained in the SDK sticky cache after the previous call. Discovery was passing
it to the join-only admission parser and rejecting the entire reconciliation.
The follow-up ignores only validated departures after the existing sender/device
trust and size checks. Departures remain invalid join evidence, and caller
presence, receipt expiry, replay and membership invalidation checks stay in
place. This explanation matches the observed counters; the repair and its
negative regressions still require the next native/live run.

The final CI acceptance job correctly failed. All job/cache posts and fixture
cleanup completed. Only fictional preview PNGs and provenance were uploaded;
no installer was published. The next candidate combines the reviewed staged
test repair with account durability, modal layout and metadata styling changes;
these new inputs do not inherit this run's passing evidence. Physical devices,
provider handoff, screen readers, installation, signing and updates remain open.

## Account access and style expansion, 2026-10-12

Candidate `cab6c91` implements the local app password, protected activation
and startup, direct account removal, durable incomplete-cleanup states and a
reset allowed only after every account has been removed. It also adds original
Nocturne/Terminal/Paper presets and explicit typography, geometry and message
layout preferences. Independent source review resolved cancellation, partial
logout/removal, profile-lock lifetime, preview-transition and update/recovery
exclusion findings. Local source checks passed: 487 Python helper tests with
six platform-only skips, direct Rust formatting, workflow validation and
four-platform scope selection. No local native build or test ran.

### Hosted candidate stopped at UI compilation: `cab6c91`

[Run 38236579140](https://github.com/brk275/Morpheus/actions/runs/38236579140)
tested `cab6c918b3a5f5709eb63df6ad19897268ce9790`. Scope passed 492 helper
tests with one skip and scanned 54 reachable commits with zero findings.
Linux formatting passed. Each of the four supported targets passed 151
configuration, Matrix and RTC protocol unit tests: 24 configuration, 119 Matrix
(including all 15 new password/removal regressions) and eight RTC protocol tests.

The no-input integration targets separately ran two pure diagnostic tests on
Linux and one on each Mac and Windows. Their ten Linux and nine other-platform
early returns are not real Matrix, OS-vault or encrypted-store acceptance.
The expanded protected-account live fixture has not executed on this candidate.

All four desktop checks stopped at the same Slint source errors: inherited
`enabled` and `has-hover` properties were referenced without `self`, and one
message-body element specified both `width` and `min-width`. The compiler also
reported new layout binding-loop warnings in the configurable side panes.
Native builds, preview captures, device checks and live Matrix/RTC acceptance
did not run. This candidate produced no artifacts and establishes no beta or
installer readiness.

Every job and cache/checkout post step finished. All five complete job logs
were scanned with zero findings. The next source repair corrects the property
references and geometry constraints; its native/UI behavior still requires
hosted verification. Prior-candidate results remain recorded below.

### Follow-up native execution: `65b4ceb`

[Run 38240049732](https://github.com/brk275/Morpheus/actions/runs/38240049732)
tested `65b4cebf703c7aa8eae061c803f0bcc135e81772`. All four native checks and
builds passed, including the Slint repairs. No layout binding-loop warnings
remained. Linux formatting passed. Scope passed 492 helper tests with one skip
and scanned 55 reachable commits with zero findings.

| Suite | Each Mac and Windows | Linux |
| --- | ---: | ---: |
| Configuration | 30 | 30 |
| Matrix | 121 | 121 |
| RTC protocol | 8 | 8 |
| Desktop/controller | 218 | 233 |
| Native RTC/device guards | 78 | 89 |
| Vendor, updater and SDK regressions | 391 | 391 |
| Total passed | 846 | 872 |

Each target had one ignored live-fixture test. Separately, two input-free
diagnostic tests ran on Linux and one on each other target. Ten Linux and nine
other-platform no-input integration early returns are excluded from those
totals and do not establish live acceptance.

The 16 core account-access regressions passed everywhere, together with the
notification-client lifetime test and two account-presentation cancellation/
detach tests. The six added semantic-theme configuration tests and three theme
projection tests passed, including legacy fallbacks and replacement of omitted
roles/sender colors. All five full Unicode emoji-catalog regressions and the
native ringtone guards passed. These are native automated regressions; actual
account-dialog keyboard/screen-reader behavior, picker performance, hot reload
interaction and audible output/device routing remain open.

Linux passed Xvfb startup and the strict Wayland capture fixture; Windows passed
owned synthetic window/display capture. Linux also passed 15 live Rust tests,
including encrypted drafts, formatting/media, search through both sync paths,
recovery, three ringing cases and native RTC/late joins. The pre-synced stock
Element Call `v0.28.0-rc.1` Sticky2025 peer passed all 11 fixture facts.

Four Linux live checks failed:

- `live_local` stopped at `encrypted_restart`. The protected-account live
  sequence follows that point and was not reached.
- Room settings failed `concurrent_topic` with the fixed `rate_limited` category.
- OAuth login/refresh failed at `vault_persist` with `rust_acceptance`; its
  restore/refresh phase did not run.
- Current/DM ringing failed `account_fresh_offer` with `timed_out`, before its
  account-switch/replay sequence. All 13 sender facts and ten earlier actor
  checks passed; the other three ringing cases passed.

These fixed diagnostics narrow the failures but do not establish a repaired
candidate. No unchanged rerun was dispatched. All fixture cleanup checks passed.

The only artifact was five actual native PNG captures plus provenance, artifact
ID `11742924649`. Exact revision/run/attempt, archive digest, image hashes and
fictional fixture names were verified; all five images received visual review.
They show Nocturne, Terminal, sign-in, incoming-call and room-settings surfaces;
they do not establish account-password dialog or Paper-preset visual acceptance.
No installer was uploaded.

Every job and cache/checkout post finished. All five complete job logs were
independently scanned with zero findings. Follow-up chrome, focus-harness and
expanded preview changes require their own candidate verification. Physical
devices, real provider handoff, interactive accessibility, installation, signing,
updates and uninstall gates remain open. Prior-candidate history follows.

### Completed prior candidate: `6abdfb7`

[Run 38171690229](https://github.com/brk275/Morpheus/actions/runs/38171690229)
tested `6abdfb7495a8d9ab9531fd051df52acd6fabe63c`. All four native builds
passed. Windows and both Macs each passed 791 unit/vendor/SDK tests; Linux passed
817. Each target had one ignored test. Scope passed 489 helper tests with one
skip and scanned 53 reachable commits with zero findings. Complete logs were
scanned without secret findings; cache and checkout post steps completed.

Linux's real Matrix/RTC acceptance failed in two places: the room-settings
fixture's first concurrent topic write and the Current DM ringing controller's
account-revocation phase. Twenty live Rust tests passed. The other three ringing
cases passed all ten actor facts, and the stock Element Call peer passed all
11 assertions. Windows synthetic capture and strict Linux Wayland capture
passed. These results do not establish real-device or installed-package
acceptance. The next batch adds fixed, secret-free error categories and finer
phase markers for the two failures; no unchanged rerun was dispatched.

The five actual native preview captures at this revision passed provenance,
image integrity and visual review, including the room-settings view. They do
not verify the new account controls, theme changes, emoji catalog or ringtone.

## Current hosted acceptance, 2026-10-11

### Pending native verification: emoji catalog and incoming-call sound

The next source batch expands the offline emoji picker to the pinned Unicode 17
catalog, including skin-tone combinations and name/shortcode/glyph search. It
preserves the original starter order and guarded literal insertion path. The
emoji catalog dependency change is a direct app edge to the already locked `emojis`
0.9.0 package; no package version or checksum changes. Five new regression
sources await execution; picker responsiveness and platform fonts remain open.

Native incoming-call sound now uses the existing CPAL output path, an original
synthesized tone, exact admitted-offer leases and the selected speaker. Initial
source review found missing failure presentation, insufficient cancellation
between device queries and a pending outgoing-request interval without sound
suppression. The revised implementation adds a subtle fixed status, checks each
native query and reserves an outgoing-request veto before waiting. Independent
source review cleared those findings. Fifteen new regression sources and one
extended existing assertion await native execution and actual device checks.
See [incoming-call behavior and limits](INCOMING_CALLS.md).

Direct formatting and patch/whitespace checks passed for this batch. Its changes
are not part of `6abdfb7` or its current CI run; earlier passing suites provide
no runtime evidence for this expansion. No beta/release gate is closed here.

The `6abdfb7` preview also exposed light system-style controls in the dark live
chat view. Five voice/history/member-invite buttons now use the existing themed
action component through a small text adapter. The independent pinned-Slint
source review checked their unchanged guards, accessible labels and actions,
text sizing and focus behavior. The changed controls still need native capture
and interaction checks; the `6abdfb7` images show the earlier controls.

### Room-settings candidate stopped at compilation: `8a1ca01`

[Run 38170624743](https://github.com/brk275/Morpheus/actions/runs/38170624743)
tested `8a1ca01f2f60776db238e5a98a105a1bf8581c0f`. Scope ran 490 helper
tests (489 passed, one skipped), selected all four native platforms and scanned
52 reachable commits with zero findings.

The candidate has two integration errors: `room_settings.rs` imports Ruma's
`ErrorKind` through the wrong module, and `AppWindow` declares two `changed page`
callbacks. All four native jobs fail for these errors. The model test command
exits before producing any Rust test-result summary; its continue-on-error
workflow step does not establish a passing suite.
The desktop check also fails. Native build, preview capture, device checks and
live Matrix/RTC fixtures do not execute, so this run provides no room-settings
or departure-observation acceptance and produces no new preview images.

Every job and cache/checkout post-step finished. All four native caches restored
and reported “Cache up-to-date”. All five complete logs scanned with zero
findings. No installer was uploaded and no package acceptance was dispatched.

The next source repair uses the pinned Ruma API's `api::error::ErrorKind` path
and moves room-settings cancellation into the existing page-change handler,
preserving its other cleanup. Direct formatting, component-scoped callback
inspection and independent pinned-API source review passed. Native compilation
and the new runtime checks still require hosted verification.

### Real native preview and expanded live fixtures: `c02a1b7`

[Run 38167245637](https://github.com/brk275/Morpheus/actions/runs/38167245637)
tested `c02a1b78fc1ce6a7b28b66c4b22768e6f30594c2`. Scope ran 488 helper
tests (487 passed, one skipped), selected all four native platforms and scanned
50 reachable commits with zero findings. All four platforms completed native
check/build and their unit/vendor/SDK suites: 776 passes on Linux and 750 on
each of Apple Silicon, Intel Mac and Windows, with no native test failures and
one ignored opt-in application test per platform.

The no-input integration targets separately contributed ten early returns and
two pure diagnostic tests on Linux, and nine early returns plus one diagnostic
on each Mac and Windows job. Those are not live fixture acceptance. Linux's 65 composer,
starter-picker, inherited-reply trust and image-preview guards passed, along
with the four rich-span/modern-room-ID regressions and modified actor-setup guard.
The supported native suites include 280 RTC core tests and 17 key-rotation tests.

The Linux live harness reported 20 Rust passes. Real formatting now passed
room/reply/thread/edit mention payloads, decrypted rich-text projection and the
independently verified hidden-reply sender case. Typed room/thread drafts passed
restart/isolation checks. The expanded media fixture passed encrypted opt-out,
opt-in preview byte equality, non-image and foreign-selection rejection, and
cleartext-room image fetches; it does not exercise the Slint decoder or viewer.
Discovery, moderation and both search modes ran and passed after formatting.
The new independent-check orchestration was used, but this run did not exercise
continuation after a formatting failure because formatting succeeded.

Accounts/switching, room actions, recovery and both real HTTPS OAuth phases
passed, including refresh persistence, restart, remote logout and vault removal.
Normal RTC and both Current/Sticky2025 fresh-device cases passed; the latter
decrypted a renewed membership without claiming historical-key recovery.
Stock Element Call `v0.28.0-rc.1` passed all eleven pre-synced Sticky2025 facts;
fresh-device and Current-format Element interoperability remain open.

All four invitation cases passed the thirteen sender/receipt facts. The modern
room-ID repair allowed the actual incoming actor to discover, withdraw, resolve
an exact one-shot key and join the admitted call in every case. Current/room
passed all ten actor checks. Current/DM, Sticky2025/DM and Sticky2025/room failed
`accepted_leave` with the generic `rtc` category; their other nine actor facts,
including cleanup, passed. `accepted_leave_observed` and the subsequent
Current/DM account-ABA assertion are not established by this run. The outer
fixture assertion is `outgoing_ring/live_tests.rs:1234:5`; the discarded observer
subtype prevents attributing these failures to a specific source condition.
Quiet evidence samples cache/media state, and Joined after SDK receipt removal
does not prove the actor processed that withdrawal. No remaining check was
skipped; all fixture service/container/network cleanup gates passed.

Windows's owned synthetic window/display fixture passed. Linux Wayland capture
separately passed the exact named test and one-pass summary, with all 33 system
PipeWire bindings and both stdout/stderr drains complete and untruncated. Its
ordered diagnostic event list reached its cap. This one pass does not resolve
the intermittent first-frame failures on previous unchanged capture inputs or
establish installed AppImage/physical-device behavior.

Four real 1440×900 native preview PNGs were captured as artifact `11712208811`.
The archive SHA-256 is
`c98b6e17ebc865d62a17f0ad67869d36ec3e61fc4c5127fc7398db428d42b915`.
Archive verification checked its revision/provenance, exact file allowlist and
PNG/RGB hashes; the actual rendered images were reviewed for fictional identities. README and
three screenshots were published in documentation-only commit `109e451` with
`[skip ci]`; that publication did not cancel this native run. These images are
static preview evidence, not live UI, keyboard, screen-reader or call acceptance.

Linux failed solely for the three live invitation cases; Apple Silicon, Intel
Mac and Windows completed successfully. Every job and cache/checkout post-step
finished. All five complete logs scanned with zero findings. All four native
caches restored and reported “Cache up-to-date”; no new cache save is inferred.
No installer was uploaded or package acceptance dispatched.

### Unverified source in `8a1ca01`: room settings and departure observation

The reviewed room-settings core adds permission-checked name/topic, avatar and
supported join-rule writes. Each operation retains the exact account selection,
room and cancellable form; writes are not retried. A successful save advances
only its field's baseline, preserving other unsaved baselines until explicit
Refresh. The added live fixture runs after existing moderation assertions and
checks concurrent topic change versus a name save, stale-baseline rejection,
refresh, state readback, avatar privacy and permission/account rejection within
one 180-second bound. Its nine new facts and all new Rust tests are unexecuted.

The native editor adds bounded in-memory PNG/baseline-JPEG preparation, staged
preview and explicit upload/removal/public-access confirmation. One owned file
worker retains its permit until the OS chooser/read exits; closing the form or
revoking its account prevents later publication or mutation steps. Encrypted-room
avatar downloads require the existing preview opt-in. Cancellation cannot recall
already-dispatched server writes, and baseline checks do not provide atomic CAS.
The fifth hosted `room-settings` snapshot is a presentation-only fixture before
account/controller initialization; it has not been captured or reviewed yet.
Native compile, actual form/chooser/keyboard/accessibility and live settings
acceptance remain pending. Existing README images are not replaced automatically.

A separate reviewed observer/fixture change identifies only the absence of a
matching current SDK membership event as `RosterNotCurrent`. The production
observer still returns an error and grants no snapshot. Departure observation
may retry only that category under the original 20-second window and membership
expiry; malformed, duplicate, conflicting or unauthenticated entries remain fatal.
Before the next fresh offer, the fixture also waits for the initiating client's
actual exact own-only roster. It never refreshes membership or retries a send.
Fixed observer/invitation subtypes preserve the next failure's cause. These
changes are source-reviewed, not executed; the prior Current/DM occupied-call
explanation remains a hypothesis and no generic `rtc` failure is declared fixed.

The prior candidate records below retain their historical outcomes. None of
these results closes a beta or release checkbox.

### Native suites pass; live projection and actor setup remain blocked: `4fd8071`

[Run 38160617966](https://github.com/brk275/Morpheus/actions/runs/38160617966)
tested `4fd8071e1cbd895311205a09fe40ca93f1e538ce`. Scope passed 484 Python
checks with one skip and scanned all 48 reachable commits with zero findings.
All four native check/build jobs reached their Rust suites: Apple Silicon,
Intel Mac and Windows each passed 746 unit/vendor/SDK checks; Linux passed 772.
The 65 added guards passed everywhere: 45 composer, six emoji picker, one
inherited-reply trust and 13 image-preview guards. These results supersede the
unexecuted/native-build-pending status recorded for earlier candidates below.

Separate integration targets without live fixture input contributed nine early
returns and one pure diagnostic test on each Mac/Windows job, and ten early
returns plus two diagnostic tests on Linux. Those are not live acceptance.
All platforms passed 280 RTC core tests, including 17 peer-renewal guards, and
17 key-rotation tests. One opt-in application live test remained ignored in the
ordinary unit invocation.

The Linux live harness reported 12 Rust passes across accounts/switching,
encrypted drafts, room actions, recovery, HTTPS OAuth login/refresh/restore/logout,
unsupported transport, and the three native RTC cases. Current and Sticky2025
fresh-device cases decrypted newly renewed membership; historical-key recovery
is not claimed. Stock Element Call `v0.28.0-rc.1` passed all eleven facts in the
pre-synced Sticky2025 fixture, without proving fresh-device or Current-format
Element interoperability.

Five live checks failed:

- The expanded encrypted formatting test reached Bob's message but failed its
  rich-projection assertion at `formatted_local.rs:433`. Source inspection found
  that strict sanitization removes a selected room mention's private attribute
  while retaining its span; the projection rejected that bare span. A reviewed
  repair treats only attribute-free spans as transparent, retaining all child,
  URL, depth and size guards. Its two new regressions and the unchanged live
  assertion await hosted retest.
- All four Current/Sticky2025 DM/room invitation cases passed encrypted
  membership proof, then stopped at `actor_setup` with `unavailable`. No incoming
  discovery/Answer assertion passed. The next source repair aligns call-room
  configuration validation with the pinned SDK's modern room IDs; runtime
  resolution of this setup failure remains unproven.

The harness did not run media, discovery, moderation or either search mode after
the formatting failure. In particular, the expanded image-preview API fixture
has no live pass on this revision. Earlier, narrower successes remain historical
evidence; they do not cover these changed fixtures.

The updated harness continues independent chat checks after a contained test
failure. Moderation still requires the formatting fixture's explicit identity
setup; infrastructure failures, deadlines and final failure enforcement remain
unchanged. This orchestration change also awaits hosted retest.

The Windows owned synthetic window/display capture passed. Linux's exact Wayland
test passed with all 33 bindings resolving to system PipeWire and complete,
untruncated test logs. Its bounded ordered trace reached the event limit; this
pass does not establish that the intermittent startup issue is resolved or that
installed AppImage capture works. Four actual native preview PNGs were captured
in artifact `11710027728` and checked for fictional identities; the README image
files remain unchanged. Static preview capture does not verify live interactions,
keyboard/IME, screen readers or physical devices.

Every job and cache/checkout post-step completed; Apple Silicon, Intel and Windows
succeeded, while Linux's job failed for the live checks above.
All five complete job logs scanned with zero findings. All native caches restored
and reported “Cache up-to-date.” No installer was uploaded. The sections below
retain results and pending work as recorded for each older candidate; no beta or
release checkbox is closed by this run.

### Composer compiler failures and palette follow-up: `03cb9d5`

[Run 38158419315](https://github.com/brk275/Morpheus/actions/runs/38158419315)
tested `03cb9d59247ae6f2326463b5a0f36fa3e8e33098`. Scope passed 484 Python
checks with one skip and scanned all 47 reachable commits with zero findings.
All four native targets passed the previous user-ID comparison site, then stopped
at the same two source issues: a formatting test's local `text` value shadows its
recursive helper (`E0434`/`E0618`), and the composer reads a private generated Slint
`side-panel-tab` getter (`E0624`). No Rust test-result summary, completed native
build, preview capture or live acceptance was produced. Every job and cache/checkout
post-step completed; all four completed native logs scanned with zero findings.
Cache restoration and upload attempts were observed, but explicit cache-service
save confirmation was absent. No installer was uploaded.

The reviewed repair renames the test helper and exposes the existing Slint
property as an output, preserving internal UI control without adding a Rust
setter. A source-only check of 317 root properties found no additional private
AppWindow accessor call. Hosted compilation and the outstanding composer,
picker, image-preview and live-fixture checks remain pending.

The same follow-up corrects built-in dark/light and shipped Aurora color pairs,
including the call button's high-contrast, unavailable and destructive states.
Independent source calculations put the 32 reviewed foreground/background pairs
at 4.538:1 or higher. Custom theme overrides and avatar colors are unchanged;
these calculations are not native rendering, screen-reader or full accessibility
acceptance. No new feature or test source is added by this follow-up.

### ID comparison build failure and image-preview candidate: `7357111`

[Run 38156725671](https://github.com/brk275/Morpheus/actions/runs/38156725671)
tested `73571113b2cd273fe2b9acd1c0d7cc1c0c66fcc2`. Scope passed 484 helper
checks with one skip and scanned all 46 reachable commits with zero findings.
All four native targets completed with the same `E0283` ambiguity at
`crates/morpheus-matrix/src/lib.rs:4141` in the inherited-mention user-ID filter.
Their compilation stopped before Rust tests, native preview capture or live
acceptance. All four complete native logs scanned with zero findings. Every
native job restored its cache and reported “Cache up-to-date”; no new cache save
was established. All cache, checkout and completion post-steps finished successfully.
No native suite passed on this revision, and no installer was uploaded.

The reviewed one-site correction binds explicit `UserId` references. It changes no
mention targeting or trust rule. Its hosted compilation and all previously
unexecuted composer/picker and expanded live fixtures remain pending.

The next candidate also adds [native image previews](IMAGE_PREVIEWS.md) for
explicit attachment clicks in room and thread views. Authenticated Matrix event
resolution supplies a bounded in-memory download; unknown room encryption fails
closed, and encrypted room, message or media-source context independently requires
the default-off TOML/Settings opt-in.
PNG and single-scan baseline JPEG have pre-header dimension/metadata checks,
an 8 MiB input limit, 4096-pixel side limit and 4,194,304-pixel limit. One owned
worker retains its permit through decryption/decoding after cancellation, while
account/view revocation prevents stale publication. No preview disk cache is
created. Close/Escape return keyboard focus; programmatic closes do not steal it.

Thirteen new Rust test sources cover decoder pixels and malformed input,
metadata/allocation guards, ciphertext integrity, streamed size limits, privacy,
worker ownership, cancellation and stale results. They have not executed. Direct
rustfmt, private-baseline patch/whitespace checks and a narrow source scan passed;
independent source review found no remaining concrete blocker. Dependency changes
add three direct edges to already locked packages without new versions. These
checks do not establish native, real attachment or accessibility acceptance.
Animation, progressive JPEG, WebP, SVG, 16-bit PNG, audio/video previews and
EXIF-orientation/ICC handling remain outside this slice. The beta/release gates
remain open.

The existing `media_local` selector also adds source-only assertions using a real
queued original PNG: encrypted/plain room-state checks, encrypted-preview opt-out
and opt-in byte equality, nonimage and foreign-selection rejection, and plain-image
opt-out compatibility, within its existing deadline. These unexecuted API checks
do not verify the app decoder, viewer, keyboard or accessibility behavior.

### Composer build failure and reviewed follow-up: `56688d0`

[Run 38154958727](https://github.com/brk275/Morpheus/actions/runs/38154958727)
tested `56688d019b91b07ffa0f1ebab3641e7138fef243`. Scope passed 484 helper
checks with one skip and scanned all 45 reachable commits with zero findings.
All four native targets failed on the same Matrix SDK/Ruma API mismatches:
`MatrixId` was imported from the wrong module, `Room::is_encrypted` was unavailable,
two calls used private `without_relation`, and a user-ID comparison had ambiguous
`AsRef` types. No native build completed; no Rust tests, preview capture or live acceptance
executed. Every job and post-step completed; all four caches restored and reported
“Cache up-to-date.” All five completed job logs scanned with zero findings.

The reviewed follow-up uses the pinned public APIs and preserves edit metadata.
Inherited encrypted reply mentions require SDK-Verified evidence bound to the
exact sender even when current room state is cleartext; unknown room encryption
and still-encrypted undecryptable events fail closed. One new trust regression
is source-reviewed but unexecuted. The prior 45 composer regressions, two incoming
fixture setup regressions and expanded encrypted fixtures remain unexecuted.

The same follow-up adds a local searchable 44-entry starter emoji picker for main
and thread composers. Insertion checks the account, target, source revision and
current native selection, preserves unaffected mention intent and creates no new
mention metadata. Six new regression sources were reviewed but have not run.
Native compilation, caret/selection, keyboard, IME, font and screen-reader behavior
remain pending. Full Unicode/skin-tone breadth is not implemented by this catalog.

### Incoming controls and membership renewal: `c7ad50c`

[Run 38149404773](https://github.com/brk275/Morpheus/actions/runs/38149404773)
tested `c7ad50cedc2559e3acfded13f135c735c2070be8`. The scope job passed
402 helper/selection checks with one skip and scanned all 44 commits with zero
secret findings. Apple Silicon, Intel Mac and Windows passed native check/build,
679 unit/vendor checks and ten selectors without live fixture inputs. Windows
also passed the owned synthetic window/display capture check. Linux passed
native check/build, 705 unit/vendor checks and twelve selectors without live inputs.
All four platforms passed 280 RTC core tests, including all 17 membership-renewal
regressions, and 17 upstream key-rotation tests. Every job and post-step completed;
Linux's final failure reflects the four incoming fixture setup failures below.

Linux passed 19 live Rust checks covering accounts, encrypted drafts and media,
room actions, formatting, discovery, moderation, both search sync paths,
recovery, OAuth login/refresh/restore/logout, unsupported transport and RTC.
Fresh-device late joins passed for both Current and Sticky2025: each receiving
device decrypted a newly renewed encrypted membership after its creation. The
historical marker only proves absence from the sampled current live map;
historical key recovery is not claimed. The stock Element Call
`v0.28.0-rc.1` pre-synced Sticky2025 case passed all eleven fixture facts. This
does not establish fresh-device Element or Current-format Element acceptance.

All four expanded incoming-call cases failed during fixture actor setup. The
passive observer had already passed and the encrypted membership was confirmed.
Source diagnosis found that `ActorAcceptance::new` called the persistent call
mode setter before creating its fresh `incoming-config.toml`. The old outer
`observer_roster` label therefore identified an earlier stage, not the failing
operation. No invitation was sent and no live incoming worker/Answer assertion
was reached. A focused repair initializes the fixture configuration and gives
actor setup its own diagnostic stage; its hosted execution is still pending.

Linux Wayland capture passed the exact named test with exit zero, all 33 system
PipeWire bindings and complete, untruncated stdout/stderr. Its ordered diagnostic
summary reached its event limit. This pass does not explain earlier intermittent
first-frame failures or establish installed-package capture. The four native
preview PNGs were captured from the actual Slint application; the incoming view
was visually inspected and the three existing README previews remained byte
identical. The incoming capture verifies presentation, not live Answer behavior.
No installers were uploaded. All four complete native logs scanned with zero
findings, and their cache and checkout post-steps completed successfully. Intel
restored no cache; its attempted save could not reserve a cache entry (HTTP 429).
The successful post-step therefore does not establish that Intel saved a cache.

### Composer candidate: source review only

The composer candidate implements caret-based suggestions from cached joined-room
members, explicit user/authorized-room mention selection, and typed Plain/Markdown
source across room, reply, thread and edit composers. Pasted IDs and links do not
create mention intent. Encrypted draft restoration preserves canonical typed
intent; unsupported or legacy rich data restores plain source without inferred
mentions. Effective-edit baselines retain full replacement metadata while notifying
only newly added targets. Unknown enqueue outcomes retain their scope guard and
source instead of inviting a duplicate send.

Forty-five new Rust regression tests and two incoming-fixture setup regressions
were added and source-reviewed but have not executed. The existing encrypted
formatting and draft fixtures were expanded for selected metadata, effective-edit
deltas, authenticated inherited reply mentions and restart persistence; those
changed fixtures have not executed either. No native build or runtime result above
verifies this candidate.

Caret/selection integration uses internal properties of pinned Slint 1.18.1 and
still needs native keyboard, IME, popup and accessibility acceptance. Suggestions
may be unavailable until ordinary room-member loading completes. Unsupported rich
edits fail closed; editing an encrypted reply with an invisible inherited sender
requires that sender's fetched event to be SDK-Verified. Cross-restart queued-send
reconciliation and complete retry/cancel controls remain open. The incoming actor
setup repair also awaits the same four hosted live cases.

### Installed AppImage capture candidate

The Linux-only [AppImage capture adapter](APPIMAGE_PIPEWIRE_PROPOSAL.md) now
passes its source-review gate and 87 focused pure/mocked Python checks. It builds
a private package with the reviewed PipeWire client fix, operates the actual
installed app through accessibility controls, and requires an independent encrypted
receiver plus live process/library identity and binding evidence. Both ordinary
loading and a separate eager-binding run are required. Two native color guards
and the installed flow remain unexecuted. The source gate does not establish
installed capture success or distribution readiness; installer uploads remain
disabled and licensing/source-provenance requirements remain open.

### Incoming call controls implementation

The active-account incoming worker, four-offer native banner, exact-key Answer
and local Dismiss are wired. Answer retains the admitted account, room and
protocol mode under the account-transition lock. It revalidates before joining
and requires the actual local device and membership generation after joining,
before media starts. Successful Answer opens controls for that exact joined
call; delayed UI completions cannot select a different destination. The pinned
RTC join does not await its own participant echo. A new readiness wait observes
both actual rosters within the original invitation deadline, rejects conflicting
identities and closed sources, and keeps cancellation active before the existing
full post-join validation. It does not fabricate a participant or extend expiry.

Forty-three added regression tests were source-reviewed and are included in the
native execution results above.
They cover bounded discovery and commands, single-use actions, cancellation,
account changes, deadlines, owned authorization, capture revocation, preference
changes and identity-label controls. In-app notification toggles apply each
successful change synchronously, including rapid off/on edits; external TOML
changes take effect when observed by live reload. SDK streams remain asynchronous,
and the pinned device-update stream omits deletions; fresh admission and
pre/post-join lookups remain required.

A fourth native preview capture selects the actual incoming-banner layout with
fictional in-memory Casey/Jules data, before any account, network or media
initialization. The five lossless-encoder/input-validation Python tests passed;
Rust formatting and workflow lint passed. The actual native capture passed and
was visually reviewed. It verifies presentation only; live actor Answer,
keyboard/screen-reader interaction and physical audio require their own evidence.
Audible ringing, incoming OS notifications and protocol Decline remain open.

The same four encrypted Current/Sticky2025 DM/room fixtures now select actual
worker discovery, one-use Answer, the production call controller, exact joined
identity, unchanged slot, receive-only device failure, leave and account-switch
revocation. The outer 300-second limits and original thirteen assertions remain.
The updated harness passed 95 Python checks. Native execution failed in fixture
setup as recorded above; the actor path has no live pass yet.
Quiet-state observations are sampled; they do not prove that no transient server
notification existed. Joined status after SDK receipt removal is also sampled;
this fixture does not acknowledge complete actor processing of that withdrawal.
Deterministic authorization tests cover that handoff separately.

### Fresh-device call membership renewal candidate

The pinned RTC core is now vendored at the same `0.5.0-rc.2` revision, with a
bounded renewal request when an existing participant observes a newly joined,
encrypted and device-attributed peer. The existing heartbeat sends fresh
ordinary membership through the unchanged SDK encryption path. It retains the
logical membership, content and fixed lifetime, serializes with leave, limits
optional attempts to one per ten seconds, and preserves the room's recipient
and trust policies. No historical keys are exported. Delayed-leave maintenance
must succeed first, except for the already-selected shorter lifetime on a
permanently unsupported server. The per-join replay ledger disables optional
renewal at capacity instead of evicting old identities.

Seventeen source-reviewed core regressions and upstream key-rotation tests are
selected in CI. The live harness now covers both Current and Sticky2025 fresh
devices without changing the 45-second roster deadline. Each must observe a
new encrypted event for the same logical membership after the original event
preceded the second device's creation. Its historical-event marker means only
that the original event was absent from the sampled current live map.
Both native fresh-device recovery fixtures passed at `c7ad50c`, as recorded
above. Installed UI and production-provider behavior remain unverified.
Dependency resolution found one patched core instance, no existing version
changes and no new registry/git package identities.

### Hosted result of the renewal candidate

[Run 38143560227](https://github.com/brk275/Morpheus/actions/runs/38143560227)
tested `4352c8829c64178dbb0cde857328ad8d4a8150da`. Scope passed 391 helper
checks with one skip. Linux passed native check/build, preview capture and
314 unit/vendor tests, plus 12 fixture selectors without live inputs.
Apple Silicon, Intel Mac and Windows each passed native check/build,
288 unit/vendor tests and ten fixture selectors without live inputs. Windows
owned synthetic window/display capture passed.

On all four platforms, the core test command failed to compile with four
`E0432` imports of
`crate::testing`: the pinned crate exports that module only with its `testing`
feature, which the command omitted. No core unit tests, including the 17 new
renewal regressions, or upstream `key_rotation` tests executed. The later
updater and SDK sticky suites were not reached. Linux
live Matrix, OAuth, RTC and Element acceptance was skipped. The
command now enables the existing feature; the corrected command has not yet run.
Vendored-test failures are also collected so independent SDK/live checks can
continue, while the final job still fails.
Fresh-device membership recovery remains unproven for this candidate.

The separate Linux Wayland test failed at `first_frame_1`: the exact named
test failed with exit 101 and no consumer process callback. All 33 capture
bindings resolved to system PipeWire, all five services remained running,
and portal selection, SHM format negotiation and buffer submission succeeded.
Both private output streams drained without truncation. The bounded ordered
trace records a valid accepted producer queue before the consumer's local
Running transition. It does not establish a lost wake or the cause of the
intermittent failure; earlier system-library passes remain historical evidence.
The unchanged static first-frame gate stays mandatory. The separate diagnostic below ran once on the same revision after all four
jobs and their posts finished.

These results do not establish new live incoming-call or incoming UI acceptance.
All jobs, cache and checkout post-steps completed. All five completed job
logs scanned with zero findings. Cache posts reported successful completion;
no unreported cache artifact identity is inferred from that result.

### Linux PipeWire startup comparison

[Run 38147202698](https://github.com/brk275/Morpheus/actions/runs/38147202698)
used the same `4352c88` source and one hash-recorded test executable for three
isolated, owned desktop sessions. The control rebuilt the exact Ubuntu
`1.0.5-1ubuntu3.3` client sources and distro patches. The patched comparison
changed only upstream commit `800a25a01f99156d7d954f4d7fe1b0a6cf8ae7e3`;
all 32 other object hashes matched the control.

| Client library | Actual capture result |
| --- | --- |
| Installed system library | Failed at `first_frame_1`, exit 101 |
| Matching private control rebuild | Failed at `first_frame_1`, exit 101 |
| Private rebuild with upstream startup fix | Passed the complete named capture test, exit 0 |

Each condition verified all 33 client bindings, kept the desktop services on
system libraries, drained its captured logs and passed owned cleanup. The patched
condition's bounded ordered-event summary reached its limit, so it is not a
complete event chronology. The first two conditions recorded accepted producer
buffers without the consumer process callback. The fixed client completed the unchanged static first-frame,
reopen/changed-frame, source-loss, stop, denial and cancellation checks.

This comparison supports the relevance of that upstream startup change. One
pass does not establish reliability or uniquely prove the eventfd race. It does
not close the production Ubuntu system-library gate: that condition still
failed. No replacement library or installer was distributed. The completed job
log scanned with zero findings; cache and checkout posts finished successfully.
Production integration and its compatibility checks remain open.

### Stable call-slot lookup correction

The passive observer now reads `m.rtc.slot`, matching the pinned SDK writer.
Only a direct HTTP 404 with `M_NOT_FOUND` permits trying the unstable
`org.matrix.msc4143.rtc.slot` alias. A present stable event remains authoritative,
including when it is closed, malformed or fails the encryption/application
checks. Authentication, transport and other server errors do not select an
alias. Both reads share the existing ten-second snapshot deadline.

Five injected-response regressions cover request identity against the pinned
writer, stable-state precedence, exact missing-state fallback and failure paths.
[Run 38140942397](https://github.com/brk275/Morpheus/actions/runs/38140942397)
tested `831cb13ddf19a1e43c2e7660eacc40fd0063c808`. All five passed on all four
native targets. Apple Silicon, Intel Mac and Windows each passed native
check/build, 339 unit/vendor/SDK tests and ten fixture selectors without live
inputs. Windows owned-window/display capture passed. Scope passed 390 helper
checks with one skip.

Linux passed 365 unit/vendor/SDK tests, 12 fixture selectors and 21 live Rust
tests. The live harness passed 43 checks and failed one. All four
Current/Sticky2025 DM/room invitation cases passed the actual passive observer,
encrypted receipt, incoming admission and revocation assertions. Both OAuth
phases, ordinary encrypted RTC and the stock Element RC pre-synced interop case
passed. The exact Wayland test passed with all 33 bindings from system PipeWire;
the earlier intermittent first-frame timeout still has no established cause.
The sole live failure remained fresh-device `initial_bob_roster`, with the
bounded diagnostic reporting `MissingMegolmSession` for Alice's current event
on Bob. The renewal candidate above addresses this separate failure.
All jobs, cache and checkout post-steps completed. Their private final logs
scanned with zero findings. No installer was uploaded.

### Native preview and microphone cancellation results

[Run 38138168629](https://github.com/brk275/Morpheus/actions/runs/38138168629)
tested `95caa416de0a4bd01c4084aa82e96270d40f64be`. Scope passed 390 helper
checks with one skip. Apple Silicon, Intel Mac and Windows each passed native
check/build and 334 unit/vendor/SDK tests, plus ten fixture-selector checks
without live inputs. All seven new microphone cancellation and controller
regressions passed on those platforms; Windows capture also passed.

Linux passed the native preview capture and 360 unit/vendor/SDK tests, plus
12 fixture-selector checks without live inputs. These include 124 desktop tests
and 74 native RTC tests, including the new audio guards. Wayland capture passed on this run:
the exact named test and one-pass summary were present, the fixture reached
`complete`, and both output streams drained without truncation. This pass
does not establish the cause or resolution of the previous first-frame timeout.
The live harness still failed the four invitation cases at
`observer_roster` / `slot_unavailable`, before incoming admission, and fresh-device
late join at `initial_bob_roster` with `MissingMegolmSession`. The stock Element
case passed all 11 facts. The live harness completed 39 checks successfully
and failed five, with none left unrun; 17 live Rust tests passed. All cleanup,
cache and checkout post-steps completed. Completed logs scanned with zero findings.


### Previous policy correction and precise remaining failures

[Run 38134894308](https://github.com/brk275/Morpheus/actions/runs/38134894308)
tested `ebccda37af584ba2e629ad5e7825f2e2a2a7d8a0`. Scope passed 384 helper
checks with one skip, including the hosted HTTPS cookie login/callback flow.
Apple Silicon, Intel Mac and Windows passed native check/build and 327 unit/vendor/SDK
tests each, plus ten fixture-selector checks without live inputs. Each desktop
suite reported one deliberately ignored live test. Windows also passed the
actual owned-window/display capture fixture, including its final fixed success
facts. All three cache and checkout post-steps completed.

Linux passed native check/build, Xvfb startup, 353 unit/vendor/SDK tests and 12
fixture-selector checks without live inputs, with one deliberately ignored live
test. These results include all five policy correction regressions, all five
passive observer unit tests and all 46 SDK sticky tests. The malformed-policy
assertion from the previous candidate now passes.

The Linux live harness completed 39 checks successfully and failed five, with
none left unrun; 17 actual live Rust tests passed. Chat, encrypted drafts,
room actions, rich messages, media, discovery, moderation, both search/sync
paths, recovery, both HTTPS OAuth phases and ordinary encrypted RTC passed.
OAuth included automatic refresh, remote logout and account/vault removal.
The official Element `v0.28.0-rc.1` pre-synced Sticky2025 case passed all 11
required facts. This remains a pre-synced compatibility result, not proof of
Current-format or fresh-device Element interoperability.

All four Current/Sticky2025 DM/room invitation cases failed at
`observer_roster`, with the fixed category `slot_unavailable`. They passed
sticky sync, own crypto, peer trust and join-without-media setup, then cleaned
up. Invitation receipt and incoming admission assertions were not reached.
Source inspection found a concrete event-type mismatch: the pinned writer sends
`m.rtc.slot`, while the observer requests `org.matrix.msc4143.rtc.slot`; locked
Ruma preserves those distinct strings. The screenshot candidate excluded the observer correction; the subsequent
correction is described above and still requires hosted verification.
Fresh-device late join failed at `initial_bob_roster`: Alice's membership was
absent from Bob's roster. The bounded diagnostic selected Alice's current
membership and reported `MissingMegolmSession` on Bob; it did not turn the
failed roster assertion into a pass.

Wayland's separate stdout proof now confirms an actual named-test failure and
one-failure summary, with `required_test_passed=false`. The first static frame
timed out at `first_frame_1`; no consumer processing callback was observed.
All 33 bindings used system PipeWire, all five services remained running and
both private output streams drained without truncation. The ordered trace
recorded an accepted valid producer queue before the consumer's local Running
transition. It does not establish the cause. The ordinary capture gate remains
failed; the manual system/control/patched experiment has not run.

All Linux fixture/container/network cleanup checks and cache/checkout
post-steps completed. All five completed job logs
scanned with zero findings and are retained privately with metadata and hashes.
No installers were uploaded. These failures keep the candidate below beta
readiness.

### Native screenshot capture, 2026-10-11

The Linux job in [run 38138168629](https://github.com/brk275/Morpheus/actions/runs/38138168629)
captured the actual Slint desktop, open account switcher and sign-in view from
`95caa416de0a4bd01c4084aa82e96270d40f64be`. All three 1560×1040 images use
fictional, in-memory preview data. The PNGs preserve the renderer's RGB pixels;
visual review checked the names and account IDs in all three views.
Artifact `11701413137` has SHA-256
`ea9320ac648a60d03d1c5a783d8445b61f794ddf0ed5d51ea6239146b973cc2d`.
Its four-file archive, source revision, run identity, PNG chunks and decoded
pixel hashes matched the provenance. The original archive and verification
receipt are retained privately. The README now uses the fresh desktop capture.

### Screenshot and microphone cancellation candidate preparation

That candidate added a debug-only native Slint preview capture before
account, configuration and network initialization. It renders three actual
UI states under Xvfb and losslessly encodes their RGB snapshots as PNG, with
source/binary/image provenance. Only the PNGs and provenance are uploaded;
raw snapshots and logs remain private. The converter and scope checks passed
24 Python tests and source review. Native capture subsequently passed as
recorded above.

The same candidate tightened microphone authorization during delayed device
opening, account changes and cancellation. The focused fake-device and
controller regressions passed in the hosted results recorded above.

### Previous incoming admission policy regression and correction

[Run 38132968786](https://github.com/brk275/Morpheus/actions/runs/38132968786)
tested `dcab6296bda12da362e54d6044c29d9de21eed29`. Scope passed 381 helper
checks with one skip, including the real HTTPS cookie login/callback flow.
Linux, Apple Silicon, Intel Mac and Windows compiled the new admission path and built
the native app. Linux also passed Xvfb startup. All four desktop suites then failed the same
`missing_or_malformed_push_rules_do_not_use_defaults` assertion: Linux had 118
other passes and each non-Linux suite 103, each with one explicitly ignored test.
All completed logs scanned clean and every cache/checkout post-step completed.

Pinned Ruma's derived Serde visitor accepts `global: []` as an empty `Ruleset`.
It also accepts positional arrays for some nested policy structs. The new
correction requires Matrix JSON object envelopes for push content, global
rules, individual rules and conditions, plus ignore-list content and entries.
It retains Ruma's typed field validation, optional fields, unknown extensions
and valid empty policy; an empty policy still denies notifications. The
regressions keep the original malformed-input assertion and add nested shape,
valid-rule round-trip and empty-policy behavior checks. The correction passed
on `ebccda3` as recorded above. Unexpected admission and revalidation errors
now retain a fixed failure category in the hosted fixture; expected replay and
withdrawal rejections remain silent. All 83 targeted Python checks and workflow
validation passed locally. No native test or fixture service ran locally.

The desktop unit failure prevented the later native-device, vendor, SDK,
Wayland and live Matrix/RTC gates from running on Linux. This run therefore
provides no new live observer, incoming admission, late-join or capture proof.
The later workflow retains a failed desktop-unit outcome while collecting
independent checks from the already-built app, then explicitly fails the final
gate. Native-device and vendored-security failures still stop subsequent work.

### Passive observer and capture evidence run

[Run 38129843715](https://github.com/brk275/Morpheus/actions/runs/38129843715)
tested `27ce324a92676dadb86b6a3b61f829c87c2daa72`. Apple Silicon, Intel Mac and
Windows passed native check/build and 298 unit/vendor/SDK tests each, including all five new passive
observer tests and 46 SDK sticky tests. Windows synthetic owned-window/display
capture passed. Their log scans found no credentials and all cache/checkout
post-steps completed. Intel also ran ten fixture-selector checks without live
inputs; these are not live-service acceptance evidence.

Linux passed its native build, Xvfb startup, 324 unit/vendor/SDK checks, 12
fixture-selector checks without live inputs, and 17 live Rust checks. The helper acceptance summary reported 39 passes and five
failures. Chat, account/sync isolation, recovery, moderation, search, both HTTPS
OAuth phases and ordinary encrypted RTC passed. The official Element
`v0.28.0-rc.1` pre-synced Sticky2025 case passed all 11 required facts; this
still does not establish Current-format or fresh-device Element acceptance.
Scope passed 367 helper checks with one skip, including hosted HTTPS cookie
login/consent.

All four expanded Current/Sticky2025 DM/room invitation cases failed in their
combined membership/observer stage before receiving an invitation. The new
passive observer has not passed live acceptance. The original stage discarded
the observer error category, so the retained evidence cannot identify whether
opening, roster sampling or the membership cross-check failed. The next
candidate separates these stages and retains one fixed failure category without
publishing identifiers, event contents or raw errors. The existing assertions
and time budgets remain in place.

Fresh-device late join again failed at `initial_bob_roster`: Alice's membership
was absent from Bob's roster and historical membership was not decrypted. The
bounded exact-event diagnostic described below has not run yet.

Wayland reached `capture-stage=complete` and exited zero with all 33 PipeWire
bindings from system libraries. Its private streams drained without truncation
or capture errors, but the harness did not find the exact named-test result
required for acceptance. The combined stdout/stderr log was deleted during
cleanup, so the missing proof cannot be reconstructed. This run is **not** a
capture pass. The new harness stores bounded stdout separately from native
stderr and requires both streams to drain, the exact test result, a unique
one-pass summary, correct bindings and owned cleanup. It does not accept exit
status or the final stage alone. The separate manual PipeWire experiment
remains unexecuted while this ordinary evidence gate is corrected.

All Linux fixture/container/network cleanup checks and CI post-steps completed,
and its cache was saved. Its log scan found no credentials. No installers were
uploaded. The observer, late-join and capture-evidence failures remain open.

### Previous corrected SDK and receipt baseline

[Run 38127191100](https://github.com/brk275/Morpheus/actions/runs/38127191100)
tested `139460d946d3fa7367397f9883ce5ebd7da19dd6`. Windows, Apple Silicon and
Intel Mac completed successfully: each passed its native check/build and 293
tests, including all 46 SDK sticky tests (12 new keyless-receipt regressions and
four upstream sliding-sync sticky tests). Windows synthetic owned-window/display
capture passed. All three logs scanned with zero findings, every post-step
completed, and their caches were current.

Linux passed its native app build and Xvfb startup, 16 configuration, 44 Matrix,
eight RTC protocol, 97 desktop, 60 native RTC/device, 46 SDK sticky tests and the
four vendor suites (1/32/10/5). Twelve additional fixture-selector checks ran
without live inputs and are not live acceptance evidence. The desktop suite
also reported one explicitly ignored test. Scope passed 334 helper tests with
one explicit skip, including the hosted HTTPS cookie login/consent flow.

The live suite passed chat/account/sync isolation, encrypted drafts, room
actions, formatted messages, media, discovery, moderation, both search/sync
paths, encrypted recovery, and both real HTTPS OAuth phases including vault
removal and remote logout. All four Current/Sticky2025 DM/room invitation cases
passed all 13 required facts: unsolicited encrypted receipt through sync before
the GET cross-check, actual SDK verification and targeting, then both server
redaction and sync-cache removal before expiry. These checks establish receipt
plumbing; they do not establish incoming UI, audible ringing or Accept behavior.

The ordinary encrypted RTC case passed. Fresh-device late join failed while
Bob waited for Alice's initial roster entry (`initial_bob_roster`, expected
present, observed absent; historical membership was not decrypted). The later
Element check still ran and passed all 11 facts against the official
`v0.28.0-rc.1` image, digest
`sha256:6d83fe15c425e1139edafdffe2b2073256608dcafdc554da5bf840de496891fe`.
This is the pre-synced Sticky2025 baseline, not Current-format or fresh-device
Element acceptance. The failed native late-join check remains open.

Wayland still failed the first static selected-display frame. All 33 origins
used system PipeWire, all five desktop services remained running, and the
complete trace recorded a valid producer queue before the consumer's local
Running transition, followed by no processing callback. The nonactive warning
was teardown-only; no unfinished-graph marker appeared. The cause remains
unproved. All explicit fixture/container/network cleanup checks and CI posts
completed; Linux saved its cache. Every job log scanned clean. No installers
were uploaded. These two Linux failures keep the candidate below beta readiness.

The later passive observer increment requires fresh encrypted slot reads and
bounded cross-checks against actual SDK membership metadata. It does not join a
call or grant device trust. The expanded live invitation checks on `27ce324`
failed as recorded above; the successful receipt baseline on `139460d` does not
verify the changed observer or receiver inputs. Incoming UI, audible ringing
and Accept behavior remain unimplemented.

A separate, manual Linux-only [startup comparison](WAYLAND_FIXTURE.md#explicit-systemcontrolpatched-startup-diagnostic)
implements system/control/patched PipeWire experiments. The ordinary capture
gate continues to require all 33 system bindings and its first static frame.
Private-client results cannot replace that evidence. The experiment has not
run; its review and mocked helper checks are preparation, not native validation.

The owner has deferred Debian packaging until a working public project email
is provided. Manual Linux package QA defaults to AppImage; Debian acceptance
remains open.

### Incoming admission and late-join diagnostic candidate

The next receiver increment adds `IncomingOffer` admission and revalidation,
without connecting the incoming-call UI or starting media. It requires an
unsolicited encrypted receipt from the selected account's sticky sync, current
local cross-signing trust bound to the original sender's encryption/signing
keys, actual valid push/ignore rules, a verified referenced membership, and a
matching live membership beside a freshly read encrypted call slot. Quiet voice
spaces and disabled notifications suppress admission. Remote revocations that
have not reached local sync are not established by these checks.

A process-owned replay ledger retains 1,024 account/room/event identities across
account switches and never extends their first monotonic deadline. It fails
closed on capacity exhaustion until restart; replay protection across restarts
remains open. Reference fetching cannot postpone the semantic ring deadline.
Any failed or cancelled revalidation permanently revokes that offer. The eventual controller
still needs change subscriptions, Accept-time revalidation and revocation during
joining; this snapshot API alone cannot authorize capture.

The existing four invitation fixtures now exercise actual admission, unchanged
wire dialect, deadline preservation, duplicate rejection, and withdrawal after
redaction before expiry. Their 13 required facts and overall time budgets stay
unchanged; fixed stages distinguish admission, revalidation and withdrawal.
Separate observer open/roster/leave stages retain only allowlisted failure
categories and cannot turn a failed assertion into passing evidence.
The `ebccda3` fixture executions failed at the preceding observer gate, so the
incoming admission assertions still have no passing live evidence.

Fresh-device late-join failure now has one bounded diagnostic fetch after the
existing 45-second roster assertion has already failed. It selects Alice's
exact currently verified membership and reports only pre-fetch presence flags
and fixed decryption categories. The fetch cannot make the failed acceptance
pass. No keys are imported, trust changed, membership republished or acceptance
timeout extended. On `ebccda3`, this diagnostic reported `MissingMegolmSession`
for the selected membership, as recorded above.

### Previous SDK test setup failure

[Run 38124824005](https://github.com/brk275/Morpheus/actions/runs/38124824005)
tested `af50e11fbd74c09a7a974ca9b7dc9196f41bfcf6`. Linux built the desktop app,
passed Xvfb startup, 16 configuration, 44 Matrix, eight RTC protocol, 97 desktop
and 60 native RTC/device tests, and all four vendor security suites (1/32/10/5).
The desktop suite reported one explicitly ignored live test. Scope passed
334 helper tests with one explicit skip, including the hosted HTTPS cookie
login/consent flow.

Windows and both Mac architectures passed their native check/build, 16 configuration,
44 Matrix, eight RTC protocol, 82 desktop and 49 native RTC/device tests, and
the same four vendor security suites. Each desktop suite reported one ignored
live test. The Windows owned-window/display capture check passed. All three
jobs then failed at the same five SDK test macro errors.

The new SDK regression test binary did not compile: five async test names
omitted the `test_` prefix required by the pinned SDK's test macro. No assertions
in that suite executed. Linux consequently skipped the live Matrix, OAuth,
invitation, RTC and Element checks. This run supplies no new unsolicited
invitation or sync-redaction evidence.

Linux's required Wayland gate also failed at the first static selected-display
frame. All 33 bindings used system PipeWire and all five desktop services
remained running. The valid producer queue preceded the consumer's local
Running transition; format and buffer setup completed, but no process callback
was recorded. Unlike the previous run, the unfinished-graph marker was absent.
The trace still does not prove why the first frame was not delivered.

All job logs scanned with zero findings and are retained privately with hashes.
Every cache and checkout post-step completed; Windows and both Macs saved
caches successfully. No installers were uploaded.

The follow-up changes only the five test names, leaving their assertions and
production code intact. One `--lib sticky` invocation retains both previous
filter sets and also includes the undecryptable-sticky and in-memory queue
tests. An SDK regression failure now permits independently built live app
checks to run and remains a failure at the final CI gate. Vendored security
failures still stop the job. The corrected run is recorded above.
Its focused local checks passed: 17 CI-selection tests, Actionlint, formatting
and whitespace checks. No native build, native test or fixture service ran locally.

### Previous completed run with live acceptance

[Run 38122640830](https://github.com/brk275/Morpheus/actions/runs/38122640830)
tested `954b2a517ce5389045f3deb84ec81e5bcbadd0f7` on all four standard public
runners. Windows and both Mac architectures passed their complete jobs, native
builds, 16 configuration, 44 Matrix, eight RTC protocol, 82 desktop and 49 native
RTC/device tests, plus the four vendor suites (1/32/10/5). The hosted Windows
owned-window/display capture check completed. Linux passed its native build,
Xvfb startup, the same configuration/Matrix/protocol checks, 97 desktop and
60 native RTC/device tests, and all vendor suites. Each desktop suite also
reported one explicitly ignored live test. Scope passed 314 helper tests with
one explicit skip, including the real hosted HTTPS cookie login/consent flow.

Linux still failed acceptance. The first static selected-display frame did not
arrive. All 33 PipeWire binding origins were correct; the five desktop services
remained running. The ordered trace places a valid producer queue and the
legacy trigger before the consumer's local node reached Running. The unfinished
graph marker precedes that transition; the nonactive-node marker occurs only
after teardown starts. Format, buffer allocation and Streaming were established,
but no consumer process callback was recorded. These facts narrow the startup
investigation without proving its cause. The required capture gate remains
failed despite the step's API success from `continue-on-error`.

The independent live harness passed account/sync isolation, encrypted drafts,
room actions, formatted messages, media, discovery, moderation, both search/sync
paths and encrypted recovery. `live_local` completed in 61.82 seconds. The
previous run's 900-second timeout did not recur; diagnostic changes alone are
not evidence of its root cause being fixed.

Both real HTTPS MAS processes passed: cancellation, login, callback replay
rejection, token rotation and vault persistence, then restart/restore,
expired-token rejection, automatic refresh, remote logout, account removal and
vault removal. This is protocol-fixture evidence; the external browser/provider
and installed-app handoff gates remain open.

All four outgoing invitation cases passed: DM and room calls in Current and
Sticky2025 formats. Each used the actual app sender, a fetched encrypted event,
verified sender/device metadata, membership and targeting checks, server
redaction and owned cleanup. These results do **not** prove unsolicited sync
receipt, incoming UI, audible ringing, offline replay or Element notification
interoperability.

The baseline encrypted RTC case passed. The fresh-device late-join case then
failed at its shared 45-second roster wait (`local_rtc.rs:247`); the retained
facts do not distinguish initial discovery, departure or rejoin. Later RTC and
Element cases were not reached. Matrix, OAuth and RTC service cleanup and the
checked container/network absence gates passed. All job logs scanned with zero
findings and are retained privately with hashes. Every post/cache step
completed; the caches were already current. No installer was uploaded.

### Receipt patch and live harness changes

Candidate `af50e11` adds a narrow patch to pinned `matrix-sdk-base` 0.19.1 that
retains keyless sticky receipts with the SDK's actual encryption metadata. It
keeps existing keyed semantics, original expiry and room generations; redaction, late keys,
replay, byte/count limits and broadcast lag have dedicated regressions. The
four live invitation cases now require unsolicited receipt **before** any GET,
then observed sync-cache redaction before expiry. The first SDK test setup
failure prevented those checks; the corrected run above passed them. See the
[patch contract](../vendor/matrix-sdk-base/MORPHEUS_PATCH.md).

The live harness now collects independent checks after a contained, scanned
assertion failure. Chat cases still share a dependent group; OAuth restore
still requires login; each invitation and RTC case gets fresh users and a room.
Process-containment, launch, credential-scan and service-cleanup errors remain
fatal. A fixed deadline permits 43 minutes of work, three minutes for nested
cleanup and eight minutes for shared cleanup, finishing inside 54 minutes of
the unchanged 55-minute workflow step. Failed and unexecuted cases remain
explicit failures. The late-join test now identifies which of its four roster
waits failed using only fixed phases and sampled presence booleans.

### Earlier completed candidate

[Run 38115477988](https://github.com/brk275/Morpheus/actions/runs/38115477988)
tested `3f1a3df4a1ed4ad125504d5c7d8daf45ae8b7dcb` on the four standard public
runners. Windows and both Mac architectures passed their complete jobs: each passed
16 configuration, 37 Matrix, eight RTC protocol, 74 desktop, and 36 native
RTC/device tests, plus the vendor security suites. The Windows owned-window
and display capture fixture reported `passed: true`, `stage: complete`.
This is actual synthetic capture evidence on the hosted Windows VM; it does
not establish physical desktop, permissions, packaged-app, or Narrator behavior.

Linux built the native app, passed Xvfb startup, 89 desktop and 47 native
RTC/device tests, the same configuration/Matrix/protocol suites, and all vendor
security checks. Its Wayland fixture confirmed all 33 bindings use system
PipeWire and received the first selected-display frame. The second capture
reached its first-frame deadline with consumer format, buffer allocation and
streaming established, but no process callback. The five desktop services
remained running. Producer markers aggregate both captures and cannot prove
that the second capture produced a frame. The full Wayland gate failed.

The independent live Linux harness passed message/sync isolation, encrypted
room and thread draft persistence, tracked-send acknowledgement, room actions,
formatted messages, media, discovery, moderation, search with both native
sliding and classic sync, and encrypted recovery. Moderation reported all four
required checks true: an existing unverified identity, unchanged identity trust,
and successful reinvite/rejoin after kick and after unban. This validates the
ordinary-invitation policy without automatically verifying the device.

The real MAS login/refresh process passed cancellation, login, callback replay
rejection, token rotation, vault persistence and plaintext-profile checks.
The separate restored process passed identity restore, expired-token rejection,
automatic refresh and updated vault persistence, then failed at logout. The
retained diagnostic does not distinguish the individual logout assertions.
RTC bootstrap, encrypted RTC and Element Call acceptance were not reached.
OAuth and Matrix service teardown and checked container/network absence passed.

All four native job logs passed credential scans with zero findings, and their
cache post-steps completed. Evidence is retained privately; no installers were
built or uploaded. This run does not satisfy the Linux live gate or beta release
checks.

### Focused Linux fixture follow-up

The [Wayland fixture](WAYLAND_FIXTURE.md#damage-driven-reopen-fixture) retains
its initial static frame check and supplies distinct moving source pixels on
each reopen. This matches the pinned backend's damage-driven protocol and
rejects stale frames. Untouched-static reopening remains unverified. The
[OAuth fixture](OAUTH_ACCEPTANCE.md) now serves its issuer over verified HTTPS:
the pinned OAuth library rejects HTTP token revocation before sending a request.
Its temporary CA has recorded ownership, bounded privileged commands and retryable
cleanup. Separate remote, account and vault logout facts remain mandatory.
Application source, dependency locks, production TLS policy and workflows are
unchanged by these corrections. The follow-up run below still fails acceptance.

Independent reviews covered the pixel assertions, worker cleanup, native TLS
configuration, certificate ownership and cleanup deadlines. Across the focused
OAuth, Wayland and CI-selection checks, 105 portable tests passed and four
hosted/Linux-only checks were skipped locally. Rust formatting, Actionlint and
whitespace checks passed. The actual changed-path selection contains only the
standard Ubuntu runner; the completed Mac and Windows jobs are not rerun for
these Linux fixture changes. No local native build, native test, service or
certificate installation was performed.

[Run 38117794649](https://github.com/brk275/Morpheus/actions/runs/38117794649)
tested the Linux-only fixture revision
`e22d685ceb774fae6030c1311802a6ec592e286b`. The scope job passed 274 helper tests,
including the real hosted HTTPS cookie flow; one separately invoked process
containment test was skipped in that helper suite. Linux passed 16 configuration,
37 Matrix, eight RTC protocol, 89 desktop and 47 native RTC/device tests, the
vendor suites, native build and Xvfb startup.

The Wayland gate failed at its **first** static selected-display frame. All 33
PipeWire binding origins were correct, the consumer reached format acceptance,
buffer allocation and streaming, and the five desktop services remained alive.
The producer recorded a screencopy-ready/enqueue attempt, but the consumer never
received a process callback. This run did not reach the new animated reopen
checks. Moving the source is not accepted as proof of static-source capture.

The preceding live chat, draft, moderation, both search/sync paths and recovery
checks passed again. HTTPS MAS startup and OAuth cancellation passed, but the
first login process failed at `browser_callback` before finishing login. The
current diagnostic does not distinguish the callback failure's cause. Restored
OAuth logout, native RTC and the stock Element peer were not reached. Both
service cleanup checks and the cache post-step completed; the cache was already
up to date. The completed scope and Linux logs scanned with zero findings and
are retained privately. No packages were built or uploaded.

### Account-selection and ringing validation work

Candidate `ae8d2c7`, included in `d155e25`, adds revocable account-selection tokens to queued
desktop notifications: an A-to-B-to-A switch cannot revive old work. Password
and browser sign-in, account switching, logout and sync shutdown cover the same
transition boundary. Source review found no unresolved issue; three token
regressions and the existing account/queue integration check cover the change.
The RTC boundary also contains 11 bounded notification-parser regressions for
the pinned deployed protocol. It does not yet send or display incoming rings;
see the [implementation boundary](../crates/morpheus-rtc/README.md#ringing-implementation-boundary).

Formatting, whitespace checks and 17 CI-selection helper tests passed locally.
The exact staged source scan found only the hash-verified public upstream test
JWT; the committed-history scan found zero credentials. These native unit checks
passed on all four standard public runners in `d155e25`; the Linux integration
failures described above remain open.

### Callback correction and capture diagnostics

The next fixture candidate corrects a source-confirmed mismatch with MAS 1.26.0:
the provider deliberately appends an empty fragment to its query-mode redirect,
but a browser omits that fragment from the HTTP callback. The Python driver now
removes only the already-validated empty suffix. Origin, path, state, code,
nonempty-fragment and production callback checks remain intact. The earlier
failure is consistent with this defect; the retained facts cannot establish its
sole cause. Fixed diagnostic categories preserve provider/callback distinctions
without printing URLs, state, authorization codes or provider descriptions.

Independent source reviews passed. The focused OAuth helper suites ran 83 tests:
82 passed and the real hosted HTTPS check was skipped locally. The native
callback regression passed with the Matrix suite in `d155e25`; actual
login/restore/logout phases were not reached. Wayland adds bounded, private producer queue and PipeWire diagnostics
to locate the missing delivery; this is not a capture fix or a passing gate.
The initial static-source frame assertion remains mandatory.

The focused Wayland helper suites ran 40 tests: 37 passed and three Linux-only
process checks were skipped locally. They cover the pinned-source patch guards,
fixed diagnostic output, bounded log draining, partial writes and cleanup.
Python syntax, Rust formatting, Actionlint and whitespace checks passed. No
local native compilation, native test, service or certificate installation ran.

### Outgoing invitation candidate

The next shared-source candidate adds an explicit **Call and notify** action
for Standard and Element Call Matrix 2.0. Join and voice-space entry remain
quiet. The worker uses the captured account-selection token and call generation,
requires its verified encrypted membership, checks current recipients and
room-mention permission, and suppresses notifications for occupied calls or
voice spaces. The pinned RTC dialect adapter controls the wire format. One
transaction has a 30-second invitation window; an ambiguous response offers no
retry. A known receipt permits bounded best-effort retraction after media stops.
Source review covered stale-account updates and the original expiry deadline.
Native compilation and runtime acceptance for this candidate remain pending.

Review also found that revocation precedes the active-account watch update while
sync shutdown is pending. The candidate adds a direct revocation wakeup for the
call's media loop, independent capture guard and invitation sender. Revocation
before subscribing and multiple pending listeners have focused regressions;
native execution is pending. This closes a cancellation observation gap in the
candidate and is not an explanation for the separate `live_local` timeout.

The hosted Linux harness adds four fresh encrypted cases: Current and Sticky2025,
each with direct-chat and room targets. Each invokes the actual app helper,
then Bob fetches and decrypts the event by the returned receipt, independently
checks its authenticated membership reference, targets and lifetime, and observes
server-side redaction. Fixture-only peer trust compares the actual bootstrapped
master keys before signing. The test opens no capture devices or media tracks.
Its main operation is bounded at 210 seconds, followed by bounded cleanup; the
outer command allows 300 seconds. The harness requires the exact named test,
all 11 fixed facts and a single final success marker. Its 54 portable helper
tests passed locally. No live outcome has been recorded yet.

This test establishes sender-event and explicit-fetch behavior only. Incoming
notification reception, sound, Accept/Decline, presentation in another client,
and real UI use remain separate gates. See the
[ringing boundary](../crates/morpheus-rtc/README.md#ringing-implementation-boundary)
for the pinned SDK's keyless sticky-event limitation.

## Beta gate implementation batch, 2026-10-10

Candidate `e411b0e` added delegated OAuth/OIDC and cancellation/refresh cleanup,
signed-update staging with installation disabled, remote-video track replacement
handling, installed-package smoke checks, and real Linux Matrix/RTC integration
on disposable services. Review fixed pending-store creation, partial OAuth token
revocation, a refresh-subscription race, and a stale video conversion that could
repaint after a track stopped. The Docker RTC fixture excludes the container's
unreachable loopback ICE socket while preserving host-loopback endpoints.

Before hosted validation, 39 Python helper tests, Rust formatting, Actionlint,
and whitespace checks passed locally. No native build was run locally for this
batch. [Run 38100413791](https://github.com/brk275/Morpheus/actions/runs/38100413791)
tested `e411b0ebdf6200cf68c3c4708f2b33bbf4c17596` on all four standard public
runners. Security/scope passed. Every native job reached the Matrix tests:
30 passed and the new OAuth fixture test failed. Linux, Windows, and Intel Mac
rejected incomplete fixture provider metadata; Apple Silicon exposed an
accepted-socket nonblocking inheritance race and selected the fixture's legacy
fallback. Later native build and integration steps were skipped, so this run
does not verify the desktop candidate. Its four native logs passed credential
scanning. No installers were built or uploaded.

The follow-up fixes the fixture's required response modes and accepted socket
handling without relaxing production OAuth validation. Delayed/split requests,
malformed successful discovery responses, and a fresh-device RTC late join now
have explicit regressions awaiting hosted execution. Independent desktop checks
can finish even when model tests fail; the final job still fails in that case.

The follow-up also adds a Wayland screen-capture adapter through the system
portal and PipeWire, with cancellable source selection and no source fallback.
A disposable Sway/wlr portal acceptance test checks actual selected-output
pixels, live changes, source removal, denial, cancellation, and cleanup of the
owned process groups. This virtual display check awaits Linux CI and cannot
replace GNOME/KDE window selection or physical desktop acceptance.

The Linux [AppImage transaction](APPIMAGE_UPDATES.md) now provides durable
replacement/recovery before account stores open, with cancellation and shared
call/update exclusion. Its tests use synthetic files; real signed installed
updates are still required. Package QA can select one Linux format and records
only that format as evidence. A missing Debian project contact fails preflight.

Before this follow-up's hosted validation, 61 Python helper tests passed
locally; three Linux process tests were skipped on macOS. Rust formatting,
locked offline dependency metadata, Actionlint, package configuration and
whitespace checks passed. Full-history scanning found no unresolved credentials;
the working-tree scan matched only the documented public upstream test JWT,
whose hash matches the reviewed fixture.

[Run 38102279481](https://github.com/brk275/Morpheus/actions/runs/38102279481)
tested follow-up `ec8e7f55ca20816d6ade41b60cac1ec95c3ffec5` on all four free
standard public runners. Security/scope and both Mac jobs passed. Each Mac ran
32 Matrix, 50 desktop, and 31 native RTC/device tests, plus configuration,
protocol, and vendor security suites. Opt-in integration wrappers without
fixture credentials are not counted as live acceptance.

Windows built the native desktop and passed its 50 desktop and 31 native
RTC/device tests, but one of 32 Matrix tests failed: cancellation left an
unfinished browser-login store behind. Its final outcome gate rejected the
candidate despite the model step's `continue-on-error` presentation. The fix
keeps the cleanup marker locked and present until the database payload can be
removed, with regressions for a real Windows delete-denied handle, interrupted
deletion, unknown files, and symlinks. The next run below verifies this fix.

Linux passed 32 Matrix, 65 desktop, and 35 native RTC/device tests, its native
build, and Xvfb startup. The first real Wayland portal open failed before frames
arrived. Later vendor and live Matrix/RTC steps were skipped. Source review found
that the stock wlr backend requires a DMA-BUF format that the fixture's Sway
software renderer does not supply. The [revised fixture](WAYLAND_FIXTURE.md)
builds a pinned backend with a small, isolated SHM compatibility adaptation;
it still requires actual selected-source frames and cancellation checks to pass.
Neither the failed run nor this unverified adaptation establishes stock desktop
compatibility. CI now collects the
independent Matrix/OAuth/RTC results after a portal failure, but still fails its
final gate when either the model tests or Wayland acceptance fail.

A new [delegated OAuth fixture](OAUTH_ACCEPTANCE.md) uses real MAS, Synapse,
PostgreSQL, and an OS vault. It checks login, cancellation, token rotation,
restart, automatic refresh after expiry, and logout in separate processes.
Its bounded HTTP form driver tests the browser protocol; actual OS-browser
handoff and callback-listener acceptance remain separate. This fixture has not
yet passed a hosted live run.

Before the next hosted run, the correction batch passed 97 Python helper tests
locally; three Linux process tests were skipped on macOS. Rust formatting,
Actionlint, and whitespace checks passed. Independent reviews covered pending
store deletion/registry sequencing, OAuth fixture isolation and diagnostics, and
the pinned Wayland backend adaptation. The Wayland harness now rejects a
zero-test success and requires its exact named capture test to pass. No native
build or service was started locally for this batch.

[Run 38104445127](https://github.com/brk275/Morpheus/actions/runs/38104445127)
tested `10f472e4266ffdb57744685b6faa19fabab8c335` on all four standard public
runners. Security/scope, Windows, and both Mac jobs passed. Each of these native
jobs passed 37 Matrix, 50 desktop, and 31 native RTC/device tests, plus the
configuration, protocol, and vendor suites. The Windows cancellation cleanup
and fresh-registry recheck regressions passed. Live opt-in wrappers remain
excluded from this evidence. All four native job logs passed credential scans.

Linux passed 37 Matrix, 65 desktop, and 36 native RTC/device tests, all vendor
suites, its native build, and Xvfb startup. The adapted Wayland backend received
CreateSession, SelectSources, and Start and announced an SHM buffer format;
the capture test still failed without an identifiable test phase or panic in
the sanitized evidence. No native signal or memory-lifetime cause is confirmed.
The independent live Matrix harness then failed during `local_matrix_env-up`,
before chat, OAuth, or RTC acceptance could run. The old log did not identify
which fixture operation failed. The candidate failed its final gates.

The next diagnostic batch records fixed Wayland test stages in a private file
that survives a libtest abort, bounded child exit/signal facts, and known panic
categories. The Matrix fixture reports fixed stages and failure categories,
plus numeric Docker/HTTP/OS statuses. Arbitrary service output, credentials,
and response bodies remain private. Synapse is now pinned to the same published
immutable image digest as the OAuth fixture. These changes improve failure
evidence; they do not establish that either Linux failure is fixed.

The next batch also adds four editable shortcut actions, a keyboard room picker
limited to the current Home or Space, portable binding validation, and shared
settings/TOML reload behavior. Independent review caught and corrected a
Shift+digit binding that Slint would deliver as a different logical character.
Native shortcut tests and actual keyboard/screen-reader acceptance are pending.

The [stock Element Call fixture](ELEMENT_CALL_ACCEPTANCE.md) pins an unmodified
official RC and requires the two live rosters, distinct decoded video in both
directions, browser-to-native audio, and departure. It explicitly tests a
pre-synced Sticky2025 baseline. No result is claimed yet, and it does not verify
the production Current default or the previously failing stable peer.

Before this batch's hosted validation, 158 Python checks passed and five
Linux/hosted process checks were skipped locally. Rust formatting, Actionlint,
JavaScript syntax, Python compilation, and whitespace checks passed. Review
fixed terminal-exit handling and owned-child cleanup in the Element harness.
A mandatory hosted regression must prove that killing its guarded namespace
wrapper also removes detached descendants before any real peer is launched.
The final namespace teardown and native/UI checks remain unverified locally.
During development, one cached Slint build-script invocation generated Rust
source in an automatically deleted temporary directory; it did not run Cargo,
a native compiler/linker, native tests, the app, or Matrix/RTC services. Later
UI edits still require the hosted compiler and keyboard acceptance.

Live Linux protocol tests and installed-package checks remain pending. Prior
candidate results below do not verify these changed inputs. OIDC provider/browser
acceptance, physical capture, screen readers, signed distribution, and updater
replacement/recovery remain open; this batch does not establish beta readiness.
Apple Developer and Windows signing credentials are not yet configured.

## Explicit update controls and shortcut compilation fix, 2026-10-10

[Run 38106808405](https://github.com/brk275/Morpheus/actions/runs/38106808405)
tested `b1d4631d6b3f5f701f28411eb5771235d77ad8c6` on all four standard public
runners. Each passed 12 configuration, 37 Matrix, and eight RTC protocol model
tests. Every desktop check then failed because the new shortcut module omitted
the `slint::ComponentHandle` trait import. Native builds, desktop/device tests,
and Linux live acceptance were skipped. All four native logs passed credential
scanning. The missing import is corrected in this follow-up; the Linux fixture
failures from the earlier run remain unresolved until their checks execute.

The follow-up connects Settings to explicit signed-update Check, Download,
Discard, and Restart actions. It keeps blocking transfers/preparation off the
UI thread, reads current status when UI notifications arrive, and retains the
startup transaction until application workers and the runtime stop. Missing
compiled feed/key or an unsupported installation disables the controls. No
public feed, private signing key, or runtime trust override is added.

Independent review fixed two lifecycle races: closing the window stops
notifications immediately and drains call cleanup alongside updater work;
queued account-view clearing cannot release an active recovery reservation.
A newly returned recovery key must be acknowledged before restart can reserve
the security gate. Nine controller tests and two security regressions were
added and await hosted execution. Rust formatting, Python helper tests,
Actionlint, whitespace checks, and a fresh staged-source secret scan passed
locally. The only scanner match is the previously reviewed public upstream
JWT fixture; no unresolved credential finding remains. No local native builds,
source generation, native tests, or Matrix/RTC services ran for this follow-up.

Manual package workflow edits no longer trigger an unrelated automatic native
matrix; package/install verification still requires an explicit dispatch for
the affected platforms. This source follow-up does not satisfy installed
signed-update, physical-device, screen-reader, or beta release acceptance.

### Hosted result and focused follow-up

[Run 38107865401](https://github.com/brk275/Morpheus/actions/runs/38107865401)
tested `95f61dab0bbc0b4c33a159a9b449170a63afef2a` on all four standard public
runners. Security/scope, Windows, Apple Silicon and Intel Mac passed. Each
native target passed 12 configuration, 37 Matrix and eight RTC protocol tests.
Windows and both Macs passed 65 desktop and 31 native RTC/device tests; Linux
passed 80 desktop and 36 native RTC/device tests. The native app built on every
target, Linux Xvfb startup passed, and all vendor security suites passed. This
includes the updater controller/security and shortcut regressions. The opt-in
test wrappers without configured fixtures do not count as live acceptance.

Linux failed its live gates. Its capture test exited via SIGSEGV (signal 11)
at `opening_display_1`, before recording a first frame. The portal backend had
received CreateSession, SelectSources and Start and announced SHM; all five
desktop services remained running. These facts narrow the failure but do not
by themselves identify the crashing function. The independent Matrix fixture
failed at `ready_a` with OS error 104, a connection reset, before live chat,
OAuth or RTC tests. Its disposable credential-vault checks and service cleanup
passed. All four native logs passed credential scans. No installers were built
or uploaded; the overall run failed.

The readiness follow-up retries an unwrapped `ConnectionResetError` within the
existing 120-second deadline in both Matrix and OAuth startup waits. Empty or
malformed responses also wait between attempts, and per-request timeouts cannot
exceed the remaining deadline. Exact OAuth issuer and redirect/proxy policies
are unchanged. Four new mocked regressions passed, including persistent reset
timeout and propagation of unrelated OS errors; the 47 existing OAuth/Matrix
helper tests and six diagnostic tests also passed. Independent source review
cleared the change. Actual service readiness still requires the next Linux run.

The next source batch also adds an explicit per-account/room Element Call
Matrix 2.0 choice and separately selected installed AppImage update and keyboard
harnesses. Their source review and offline tests are recorded in their focused
documents; none has yet passed installed or live interoperability acceptance.

Read-only inspection of the pinned WebRTC archive found weak PipeWire stubs
that can collide with the system API. The [isolation patch](../vendor/webrtc-sys/MORPHEUS_PATCH.md)
verifies the exact archive, renames all stub definitions and references in a
build copy, and removes optional LLVM address-significance metadata whose
indexes cannot safely be assumed unchanged. The Wayland fixture must verify
all 33 actual linked addresses before initialization, then still pass real
capture. This is a reviewed correction candidate, not proof of the crash's
cause or resolution. Separately, corrupted frame chunks are dropped before
pixel access, and namespace containment replaces the fixture's stale-PID
cleanup fallback.

Before hosted verification, the combined follow-up passed 207 Python checks;
five platform-specific checks were skipped on macOS. Rust formatting, locked
offline Cargo metadata, Actionlint, JavaScript syntax, Python compilation,
package configuration and whitespace checks for Morpheus edits passed. The
new vendored WebRTC source retains upstream whitespace unchanged; comparison
with the pinned registry source confirms only the documented patch files differ.
Independent reviews
covered protocol selection, package/update harnesses, readiness, capture
cleanup and archive isolation. No local native build, archive transformation,
link, source generation, native test or Matrix/RTC service ran for this batch.

### Linux capture and fixture follow-up, 2026-10-11

[Run 38110401901](https://github.com/brk275/Morpheus/actions/runs/38110401901)
tested `16fbd5b1e66b88096838e9955899a48cb5b36e08` on the four standard public
runners. Linux passed 14 configuration, 37 Matrix, eight protocol, 80 desktop,
41 native RTC/device tests, all four vendor suites, the native app build and
Xvfb startup. Its linked-address check confirmed all 33 PipeWire bindings use
the system library. Capture advanced past the earlier initialization crash,
then failed awaiting its first frame after the portal backend exited.
This is progress in diagnosis, not capture acceptance.

Windows, Apple Silicon and Intel Mac passed their native jobs: each ran 14 configuration,
37 Matrix, eight protocol, 65 desktop and 33 native RTC/device tests, plus the
vendor suites. Opt-in wrappers without fixture credentials do not establish
live acceptance. All four native logs passed credential scanning. Cache saves
completed on the three successful targets before the next candidate was pushed.
The overall run failed its Linux gates. No installers were built or uploaded.

The independent Matrix fixture now started both homeservers successfully and
passed its disposable vault checks. RTC setup failed before live acceptance;
the retained diagnostics cannot identify that operation. The follow-up retries
direct connection resets within the existing RTC readiness deadline and adds
fixed phase/error categories, without publishing exception or server bodies.
Read-only checks found no mismatch in the pinned RTC image manifests or the
inspected upstream configuration fields. Chat, draft, recovery and OAuth checks
now precede RTC startup; a failure still rejects the candidate and triggers
cleanup.

The [capture correction](WAYLAND_FIXTURE.md#spa-format-enumeration-correction)
adds BGRx to the accepted SPA alternatives as well as the default slot. A
serialized-offer regression awaits the next Linux native run. This verified
protocol defect is consistent with the backend failure, but its exact cause
is still unconfirmed. Native CI can now save dependency caches after a failed
acceptance check; the existing trusted-main-only write guard remains in place.

Independent source review cleared both fixes and the fixture sequencing.
The combined follow-up passed 233 portable Python checks with five platform
skips, Rust formatting, Actionlint, Python compilation, JavaScript syntax,
package configuration and whitespace checks. No local native build, source
generation, native test, archive transformation or Matrix/RTC service ran.

A subsequent Linux harness correction makes cleanup failures fatal, preserves
the original test failure or interrupt, and still attempts both service stops
when summary writing fails. Checked Docker inventories must prove fixture
absence before the harness takes ownership and after cleanup. An unavailable
daemon cannot count as an absent fixture. Retained test volumes and private
fixture data remain on the ephemeral runner; this check does not claim they
were wiped. All 33 focused Python harness checks passed with mocked lifecycle
and Docker operations; actual hosted execution of this correction is pending.

### Thread drafts and Windows capture follow-up

Per-thread drafts now share the encrypted SDK store and the existing flush
worker, with separate account/room/root keys. Cached restoration is immediate;
late disk results cannot overwrite edits or repopulate a logged-out account.
Review caught that SDK queue acceptance is not server acknowledgement: the
thread send path now observes the exact transaction, clears only an unchanged
submitted revision on acknowledgement, and retains drafts on paused, cancelled
or unknown outcomes. A per-thread session guard prevents duplicate submissions
while still allowing edits and other threads. Unknown outcomes require a restart
and checking the thread before resending. Cross-restart queue reconciliation
and retry/cancel controls remain open.

Four draft-cache regressions and extensions to the existing encrypted draft
restart/isolation and live thread-send fixtures are prepared. They have not yet
run natively for this follow-up. The separately selected
[Windows capture fixture](WINDOWS_CAPTURE_FIXTURE.md) exercises an owned
synthetic window and covered monitor through the production capture API, with
Job-object process containment and strict fixed-fact evidence. Its actual
WinForms/GDI execution also requires the next hosted candidate.

The combined follow-up passed 222 portable Python checks; five platform-specific
checks were skipped on macOS. Rust formatting, Actionlint, Python compilation,
JavaScript syntax, package configuration and whitespace checks passed.
Independent source reviews cleared both changes. No local native build, native
test, source generation, archive transformation or Matrix/RTC service ran.

### Hosted follow-up results, 2026-10-11

[Run 38112448483](https://github.com/brk275/Morpheus/actions/runs/38112448483)
tested `db25e42e38897acd746780722d1bac58a57a65b7` on all four standard public
runners. Security/scope and both Mac jobs passed. All four platforms passed
14 configuration, 37 Matrix and eight protocol tests, their native app build,
and the four vendor security suites. Each Mac and Windows passed 69 desktop
and 33 native RTC/device tests; Linux passed 84 desktop and 42 native RTC/device
tests and Xvfb startup. These include the new draft-cache regressions and the
Linux serialized SPA-format offer check. All four completed native logs passed
credential scans with zero findings. No installer was built or uploaded.

Windows failed the capture fixture before its native test started: cleanup
reported status 5. The step's `continue-on-error` presentation did not establish
success; the final outcome gate failed. Source review found that a failed
`TerminateProcess` can precede asynchronous completion. The correction waits
on the retained process handle, accepts only confirmed completion, and preserves
earlier failures alongside cleanup failures. Nineteen mocked lifecycle checks
pass; the Windows fixture still requires successful hosted execution.

Linux again failed its first-frame Wayland check. All 33 PipeWire bindings
resolved to the system library; this time the portal backend remained alive,
negotiated a format and added buffers. The capture still timed out without a
frame. SHM was offered, DMA-BUF was not, and all five fixture services remained
running. These facts narrow the failure but do not establish screen capture.

The live Linux vault and two-homeserver startup checks passed. The live account,
encrypted draft restart/isolation, room action, formatted message, media and
discovery tests each passed. Moderation failed at its post-kick invitation
check; later search, recovery, thread, OAuth and RTC/Element checks were not
reached. Both fixture shutdowns passed, and the native dependency cache finished
saving before further hosted work. The overall candidate remains unverified.

The next shared native batch adds persisted microphone and speaker selection
for the next call. Exact device IDs are resolved again at open time; unavailable
selections fail without choosing another endpoint. Current calls keep their
selection, discovery runs off the UI thread, and cancelled device opens cannot
start capture after their request expires. Independent source review and
formatting checks cleared; ten new focused regressions await hosted execution.
Physical routing, permissions, unplug behavior and picker accessibility remain
required.

The Linux follow-up completes the PipeWire consumer's required buffer-parameter
update after receiving a format. Its serialized offer permits only one packed
RGB plane in mapped memory and preserves producer-supplied row padding. Fixed
consumer-progress diagnostics distinguish allocation, streaming and delivery
timeouts. Independent source review cleared the API and buffer lifetime handling;
actual capture remains pending.

The [invitation policy](INVITATION_HISTORY_POLICY.md) now explicitly disables
the SDK's experimental incoming and outgoing invite-history bundles. Ordinary
invites retain server authorization and existing encryption trust checks.
Inspection found that automatic history sharing can reject an unverified sender
before sending the invite; the old panic location alone cannot confirm that
this caused the hosted failure. The next moderation gate requires actual
rejoins after kick and unban from an unverified identity with unchanged trust,
and reports only fixed failure categories. Root source review and 37 focused
Python harness checks passed. Hosted native and live execution remain required.

Before pushing this combined candidate, 256 portable Python checks passed and
five platform checks were skipped on macOS. Rust formatting, Actionlint,
Python compilation, JavaScript syntax, package configuration and whitespace
checks passed. Native builds, native tests, source generation, archive
transformations and Matrix/RTC services were reserved for the free hosted
runners. The shared audio/client changes require all four native targets.

[Run 38114660116](https://github.com/brk275/Morpheus/actions/runs/38114660116)
tested `2c652f13d332c8bb7999d9bd15edad994c55f41f`. All four targets passed
16 configuration, 37 Matrix and eight protocol tests and the default desktop
check, then failed the native build with Rust E0282 in the device-opening
closure. Native device, capture and live integration checks did not run after
that failure. All four completed native-job logs passed credential scanning;
their cache post steps finished before another candidate was pushed.
The correction explicitly gives the closure a `MediaError` result type.
Independent source review and formatting checks passed; hosted compilation is
still required. No installer was built or uploaded.

## Public source publication, 2026-10-10

- Published `brk275/Morpheus` as public pre-alpha source at
  `bc1614bbea5e8513c04c4cbbe4387cfb80fae54e`. Anonymous GitHub API access
  confirmed public visibility. Commit identities remain pseudonymous.
- The private cleanup push [run 38092927224](https://github.com/brk275/Morpheus/actions/runs/38092927224)
  skipped all hosted jobs. It performed no native build.
- Verified secret scanning and push protection enabled, private vulnerability
  reporting enabled, approval required for all outside contributors' workflows,
  and read-only default workflow permissions. GitHub reported no open
  secret-scanning alerts at publication; scans do not guarantee future safety.
- Standard hosted runners provide free public-repository build compute, not
  account credits ([GitHub runner documentation](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)).
  Earlier charges, storage rules, and paid-runner restrictions remain separate.
  Billing caps were not changed.

### Complete native CI matrix

[Run 38092977119](https://github.com/brk275/Morpheus/actions/runs/38092977119)
checked source revision `bc1614bbea5e8513c04c4cbbe4387cfb80fae54e` on standard
public runners. All native build and test steps passed on the first attempt:

| Target | Runner | Result |
| --- | --- | --- |
| Linux x64 | `ubuntu-24.04` | Native build, tests, and Xvfb startup smoke passed |
| macOS Apple Silicon | `macos-15` | Native build and tests passed |
| macOS Intel | `macos-15-intel` | Native build and tests passed |
| Windows x64 | `windows-2025` | Native build and tests passed |

The workflow completed successfully. Actual Rust host targets were verified
for all four jobs; 23,893 native-job log lines passed credential scanning with
zero unresolved findings. Private evidence retains the completed job records.

Each target checked the default desktop configuration, built `native-calls`,
and tested desktop controllers, native device guards, and the vendored security
patches. Real homeserver tests remain explicitly invoked local checks; hosted
jobs had no Matrix credentials. The standalone theme job was intentionally
skipped because the full native jobs already test configuration.

This run did not build or upload installers. It is not installed-package,
physical-device, signing, accessibility, or interoperability acceptance.
Nonfatal upstream warnings remain, including `block` future-Rust compatibility,
macOS duplicate JPEG symbols/debug unwind size, and deprecated native APIs.
The pinned checkout action also reports its Node 20 declaration while GitHub
runs it on Node 24. These need dependency/toolchain follow-up before release.

## 2026-10-10 public-source preparation

Public development does not authorize installer distribution. The original
binary release gates, including real devices and all supported platforms,
remain open.

- Removed automatic QA-account bootstrap from the desktop app. Explicit
  disposable tests remain; fixture generators reject checkout destinations,
  preserve owner-only files, and reject symlink writes.
- Passed 53 `lru` unit tests, 32 LiveKit tests, 10 MatrixRTC transport tests,
  31 native RTC/device tests, and 37 desktop controller tests on Apple Silicon.
  The cancellation cases cover a queued permission grant, a late grant after
  cancellation, permission denial/timeouts, and a cancelled open before native
  enumeration. They do not establish physical camera behavior on every OS.
- Local listeners confirmed authenticated requests do not follow
  301/302/303/307/308 redirects. Invalid foci fail before OpenID acquisition;
  unsafe SFU and region URLs are rejected before credential use. Captured logs
  omit reflected dummy bearer/proxy credentials. Unsupported proxy schemes
  open no socket; HTTP CONNECT remains supported.
- Passed 25 Python tests for CI scope, platform packaging configuration,
  private fixture paths, the loopback relay, and stale fixture mounts. Rust
  formatting, Actionlint, Python compilation, JavaScript syntax, and package
  configuration checks passed without building installers.
- Fresh disposable accounts on two local homeservers passed login, encrypted
  room, and token-isolation checks. The native RTC fixture's discovery,
  OpenID-to-JWT exchange through its private relay, returned loopback URL, and
  SFU reachability checks passed. The fresh native two-device encrypted call
  test passed 1/1 in 19.41 seconds: synthetic audio, camera and screen tracks,
  signaling reconnect, participant departure, leave-triggered key rotation,
  and media after rejoining. This is loopback acceptance on Apple Silicon,
  not physical-device or Element Call interoperability acceptance.
- Fresh secret scans found no live credentials. The upstream LiveKit public
  JWT test vector and one historical property-name comparison are documented
  scanner false positives with narrowly scoped fingerprints.
- Preserved a private manifest of historical run/source/artifact identities,
  then deleted three old package runs and all ten unsigned installer artifacts.
  A subsequent GitHub API read reported zero remaining Actions artifacts and
  zero releases. Historical package results do not verify this candidate.
- Hosted jobs are gated while private. Public CI uses standard runners,
  read-only permissions, pinned actions, approval for outside contributors,
  trusted-main cache writes, and no installer uploads. The complete public
  native matrix is recorded separately after publication.

## 2026-10-09 local desktop and Matrix checks

- Built the macOS Apple Silicon desktop binary with `native-calls` and packaged
  an unsigned `.app` from it. The bundle has microphone, camera, and screen
  capture purpose strings. Signing, notarization, and updater behavior remain
  untested.
- Launched the native Slint app and inspected its sign-in, bottom-left account
  menu, light theme, and font-scale controls through macOS accessibility and
  screenshots. The menu listed the active account, another signed-in account,
  Add account, Manage accounts, and Log out.
- In the desktop preview, starting a call and choosing another account opened
  a leave-call confirmation. Escape cancelled it without switching; keyboard
  confirmation ended the preview call and switched accounts. The same guard is
  wired for native calls, but a connected native call has not passed this check.
- The settings UI changed font scale to 110% and back to 100%, switched density
  between Compact and Comfortable, and persisted the selected values in
  `config.toml`. Unread-first sorting is also wired through that config.
- Signed in disposable Alice on loopback Synapse A and Carol on independent
  loopback Synapse B. Each account showed only its own rooms. A message sent
  from Alice's composer appeared in the room and was confirmed through the
  server API. Carol did not see Alice's unsent draft; it returned when switching
  back to Alice. Reopening the app restored Carol as the selected account with
  only her room visible.
- One account switch briefly showed an empty Alice timeline after a local
  homeserver restart. A later switch restored the server-confirmed message.
  The strengthened acceptance test now confirms the cached feed retains a
  sent message across switches and manager restart. A separate presentation
  bug that could briefly show the prior account's messages when both accounts
  selected the same room ID was fixed by clearing on account identity change.
- Two local Synapse servers passed password login, room-membership, encrypted
  room-state, and cross-server token rejection checks. The first gated Rust
  `AccountManager` acceptance test passed: account isolation, switching,
  selected-account restore, and Alice-to-Bob encrypted message decryption
  before and after Bob restarted. The stronger test passed with cached-feed
  checks and with a forced classic-sync session alongside native sticky
  sliding sync. Both current Synapse fixtures advertise sliding sync, so the
  classic policy is forced explicitly for that adapter check.
- A second gated acceptance test used fresh disposable Alice/Bob identities and
  an encrypted room. New-device Alice could not read a historical message
  before recovery; after importing the backup recovery key, the same timeline
  decrypted it. This establishes the core key-backup recovery path. The native
  Settings view now displays device verification, recovery, and key-backup
  status for the active account, and wires recovery setup and import to that
  core. A new key appears in a one-time reveal/copy dialog with explicit save
  guidance; the input clears after submission or leaving Settings. A shared
  gate prevents queued account changes and normal window close from dropping
  the only returned key before acknowledgment. The Security panel also has an
  own-device SAS flow: request verification, wait for the other device, show
  emoji and decimal comparisons, explicitly acknowledge a human comparison,
  confirm, or cancel. It never confirms a comparison automatically. The
  native-call app suite passed 24/24 tests, including gate, close-response,
  and explicit comparison checks; the default app check and native binary
  build passed. Fresh-device recovery and two-device SAS verification through
  the actual desktop UI still need live acceptance.
- A gated two-Synapse room workflow passed private room and Space creation,
  invitation acceptance and decline, room-ID joining, and joining a registered
  room alias. Repeated runs initially hit Synapse's local invitation rate limit
  (`rc_invites.per_user`, HTTP 429); the loopback fixture now raises that limit.
  The app wires those room controls and protects each pending action against
  account changes. A newly created Space has an account-scoped temporary rail
  item while the SDK's room classification catches up. The Space UI path has
  passed a native compile, but not a live desktop click-through.
- After native UI QA found new private rooms were cleartext, creation now
  includes Megolm state in the initial server request. A fresh loopback test
  passed 1/1 in 8.57 seconds: an independent client read encryption state in
  four new private rooms and a new DM, with each private room checked before
  invitation. The formatted two-device test passed 1/1 in 10.29 seconds on a
  newly created room without a separate encryption step. Existing cleartext
  rooms and Spaces are not converted by this change.
- The gated encrypted-room messaging test passed reply, edit, reaction, and
  thread send/read through the Matrix core. Bob received the changes, and
  operations bound to Alice were rejected after switching to Carol. A packaged
  macOS app then clicked through Reply, Edit, Heart, and Thread in a disposable
  local Synapse room. The UI displayed reply context, the edited body, a heart
  count, and the thread reply. Direct Synapse reads confirmed `m.in_reply_to`,
  `m.replace`, `m.annotation`, and `m.thread` relations to the intended events.
  This checks the native UI to server path for one account on macOS; encrypted
  UI actions and other desktop platforms still need acceptance checks. Visual
  QA found missing glyphs for Edit and Thread; those actions now use drawn
  Slint paths, and the thread panel keeps its reply count and short reply rows
  directly below the root message in the rebuilt macOS preview.
- A gated two-Synapse draft test saved different room drafts for accounts on
  separate homeservers, restarted the account manager, restored both drafts,
  cleared one, and removed each account. A raw scan of its account-data files
  found neither unique draft body in plaintext. Focused draft tests covered
  keeping text through queue acceptance, restoring it on server send failure
  when unchanged, and preserving a newer edit. Pending transaction metadata
  is currently in memory: if Morpheus exits after queueing but before send
  confirmation, a message sent after restart can leave a stale encrypted draft.
- A stable-path, ad hoc signed macOS QA bundle stalled in the OS credential-vault
  read on two launches. A SecurityAgent process appeared, but no prompt was
  exposed through the available app UI. The 30-second vault-read timeout then
  displayed an explicit sign-in error and enabled offline preview. This QA
  bundle reused the same account registry and Keychain item as earlier preview
  bundles, but `codesign -dr -` showed different designated CDHashes for each
  ad hoc signature. An access-control approval for the new code identity is
  the likely reason SecurityAgent appeared; the prompt's visibility and the
  exact blocked OS operation remain unverified. Repeat restore with a fresh
  test profile, a stable real signing identity, and a relaunch of the same
  signed build before treating credential restore as verified. No Keychain
  entries were read, changed, or removed during this diagnosis.
- A macOS offline-preview keyboard pass reached Home, room rows, the
  bottom-left account switcher, message Reply, and Join/Leave call controls.
  Tab, Space, Return, and Escape were exercised for account switching and the
  leave-call dialog; Reply focused the labeled composer. A later source-synced
  build confirmed the account menu focuses its first visible account, forward
  Tab cycles within the account manager and room dialog, and Escape returns
  focus to the trigger. The automation delivered Shift+Tab without a Shift
  modifier, so reverse traversal requires a physical-keyboard check. Native
  screen-reader checks and Windows/Linux keyboard runs remain outstanding.
- A loopback MatrixRTC stack passed authenticated transport discovery and
  OpenID-to-LiveKit JWT exchange. Synapse advertises the sticky sliding-sync
  feature required by the native call path. The gated native call test reached
  LiveKit signaling after adding the upstream macOS `-ObjC` link workaround.
  Docker Desktop's loopback-published media ports offered no viable ICE pair.
  A native LiveKit instance bound to macOS loopback connected both disposable
  participants over UDP and populated both rosters. An initial encrypted-key
  import failed because the devices lacked verified cross-signing identities.
  A fresh isolated fixture bootstrapped each identity and signed its device.
  The initial gated test passed in 17.07 seconds: Bob imported Alice's media
  key, decoded nonzero synthetic microphone audio, left, and rejoined. An
  expanded fresh-fixture run passed in 18.27 seconds: Bob also subscribed to
  Alice's `Camera` and `ScreenShare` tracks and decoded the expected red/green
  I420 pattern at 640×360 from each. This checks local macOS two-device media
  transport inside an encrypted call. The screen track carried generated pixels;
  OS capture, Morpheus call-stage video rendering, automatic network reconnect,
  and Element Call interoperability were untested at that point; a later
  legacy-format browser-peer run passed, as recorded below. A separate native Slint
  proof displayed moving synthetic frames and rotation on macOS; it did not
  consume the received RTC track. See [the video feasibility note](VIDEO_FEASIBILITY.md) and
  [the RTC test runbook](../tests/LOCAL_MATRIX_RTC.md).
- A follow-up macOS capture proof used real display and application-window
  pixels. The capture adapter produced nonblank 640×360 I420 frames without
  saving image files. In two fresh disposable-device loopback MatrixRTC runs,
  Bob decoded a nonblank encrypted screen track from the selected display
  (20.03 seconds) and window (18.32 seconds), respectively; both runs also
  retained synthetic camera/audio and leave/rejoin checks. These tests used the
  development binary and a local call service. Packaged-app permission prompts,
  continuous capture controls, Morpheus call-stage rendering, Element Call
  screen-share interoperability, and Linux/Windows capture remain unverified.
  After tightening the decoded-frame check to visible rows, a fresh display
  call passed again in 19.83 seconds. During setup, a generated password
  beginning with `-` exposed an argument-parsing bug in the disposable
  Synapse registration script; it now uses `--password=value`, and the next
  fresh fixture registered successfully. All local services were stopped.

## 2026-10-10 focused follow-up checks

- `ConfigStore::save` now writes a unique temporary file beside `config.toml`,
  flushes it, and persists it over the destination. The focused configuration
  suite passed 5/5 tests, including saving over an existing config and reporting
  a malformed theme after a selected theme file was previously missing. This
  checks the replacement path; forced process termination during a write and
  Windows filesystem behavior have not been exercised at runtime.
- A gated two-Synapse public-directory test passed after the disposable fixture
  allowed public-room publication. Alice searched two rooms over two pages,
  resolved an alias, and Bob joined one through a routed `matrix.to` link.
  Switching to Carol on independent Synapse B rejected delayed Alice actions
  and returned only B's directory result. The isolated servers do not test
  federated directory or join behavior.
- A gated moderation test created a disposable room on Synapse A and passed
  rename/topic, default-power permission rejection, promotion to moderator
  level 50, rejection of a moderator's self-promotion to 100, demotion, and a
  second permission rejection. The creator redacted Bob's message; an
  authoritative event GET showed redacted content and both client timelines
  removed the original body. Kick and reinvite, ban and blocked rejoin, then
  unban and rejoin also passed. A follow-up run asserted the account-scoped
  member list showed Bob as invited, joined, and banned at the corresponding
  server states and rejected a mismatched account ID. It passed 1/1 in 20.99
  seconds and removed its test devices/stores. This verifies local core
  operations, not moderation UI, federation, or every room-version permission
  rule.
- An isolated macOS native-app profile signed in disposable Alice on loopback
  Synapse A. The discovery view rendered a newly published room's name, alias,
  topic, member count, and accessible Join control; clicking Join opened the
  room and composer. A routed `matrix.to` alias for a second disposable room
  also joined and opened its conversation. The temporary QA login bootstrap
  was removed from source, Alice logged out, both rooms were unpublished, and
  the protected app profile was deleted. Federation and other desktop
  platforms remain untested.
- A gated encrypted-message search test passed on loopback Synapse. Alice sent
  a unique message and found both a common-word match and its exact marker,
  with sender, room, timestamp, body, and event ID. After switching to Carol on
  independent Synapse B, Alice's query was rejected and Carol found no matching
  result. Cross-room search returned the same room and event, and an
  event-focused timeline loaded the message in its context. Alice's index
  returned the same event after manager restart. A raw
  scan of both account stores found no unique message body in plaintext. The
  search index is encrypted per account and indexes locally as events sync or
  paginate. The same gated test passed again with forced classic `/sync`.
  An added edit case then found the replacement's new text and mapped the hit
  to the original visible event ID, with the edited body in its event-focused
  context. This expanded test passed on both sync paths in 15.87 and 15.53
  seconds respectively.
  A native current-room query/results panel and selected-event context are
  wired with account/room generation guards. Its sanitizer unit test and the
  13/13 native-call app suite passed. In an isolated macOS app profile, Alice
  sent a new message, searched its exact marker, saw one result with
  sender/time/body, and opened context containing that event and its neighbor.
  The disposable app account was logged out, both QA profiles and residual
  vault entries were removed, and the temporary login bootstrap was removed
  from source and binary. All-room UI, large-history behavior, and other
  platforms remain untested. The QA screenshot is at
  `/tmp/morpheus-search-ui-qa.png`.
- The native Members panel now fetches the selected room's joined, invited,
  and banned entries with account/room guards, sanitized display text,
  explicit membership and power labels, loading/error/empty states, a Refresh
  action, and a virtualized list. The then-current native app suite passed 20/20
  tests; default app check and native-call binary build passed.
  An isolated macOS app signed in disposable Alice using a protected fixture
  read locally at runtime. In Alice's test room, the panel showed Alice as
  Creator and Bob as Invited; in the encrypted test room it showed Bob as a
  joined Member. Refresh repopulated the real server rows. A same-account
  background refresh closed the Add-account dialog during Carol sign-in.
  The dialog persistence fix passed unit and app compilation checks. A later
  isolated macOS app added Carol on the second homeserver and switched between
  Alice's and Carol's distinct room lists. Both were logged out and the app
  registry contained zero accounts. Bob's temporary invite was revoked; the
  earlier QA app, profile, and credential helper were removed.
- A fresh disposable Alice/Bob MatrixRTC test passed in 18.16 seconds after
  Alice wrote the `io.github.brk275.morpheus.voice` room-state marker and Bob
  read it. The first attempt exposed that native sliding sync did not include
  this custom state event; the reader now fetches the state directly from the
  homeserver. The same passing run checked Morpheus's participant-roster
  wrapper, encrypted synthetic microphone, camera, and screen tracks, and
  leave/rejoin. It verifies the marker and roster core against local Synapse,
  not a voice-space settings or navigation UI.
- The native call stage now has a participant list and a remote video tile.
  Its worker selects a remote screen share ahead of a camera, bounds I420-to-RGB
  conversion to 2.07 million pixels with a 1920-pixel edge limit and about
  15 frames/s, and clears stale frames by
  call generation. The native-call app check and six focused app tests passed
  on macOS. A live RTC frame has not yet been viewed in the Morpheus call UI;
  this remains a separate acceptance step from the successful transport tests.
- A fresh isolated macOS app joined an **unencrypted** room call with a
  synthetic Bob peer. Its live roster showed Alice and Bob and retained the
  correct room title. A CoreAudio device open blocked the call worker before
  either roster or video could run; device opening now occurs on a detached,
  single-in-flight OS thread with a 30-second bound. The app then showed a
  view-only local-audio failure, kept the roster visible, and returned to idle
  immediately on Leave. The bottom-left account switcher guarded the active
  call. The camera tile did not receive a frame. A separate two-peer native
  probe kept Bob's camera publication alive and pushed 640×360 frames for 30
  seconds. Bob's own roster advertised `Camera, muted=false`, while Alice saw
  Bob with `streams=[]` and no camera `remote_track` in all 30 samples. The
  pinned RTC transport maps remote members to LiveKit identities using their
  sender device ID; current-format cleartext membership events provide no
  attributable device ID. Morpheus now rejects current-format calls in
  unencrypted rooms before opening or joining, with guidance to use an
  encrypted room. The same disposable Alice/Bob fixture passed the encrypted
  native audio/video test in 18.59 seconds, including decoded camera and
  screen-share I420 frames, leave, and rejoin. This does not yet validate
  Slint frame rendering or an encrypted GUI call.
  An app-owned Alice/Carol account switch across the two local homeservers
  passed afterward. The final native app suite passed 22/22 tests, including
  call generation and Security transition/close guards.
- A gated Alice/Bob encrypted attachment roundtrip passed twice on local
  Synapse. Alice queued a binary file, Bob downloaded and decrypted identical
  bytes, and the test checked the filename, MIME type, declared size, file
  kind, encrypted metadata, account isolation, and absence of a unique payload
  prefix in both account stores. The final run logged out its disposable
  devices and removed their temporary stores. The encrypted roundtrip passed
  again after Morpheus replaced the SDK's unbounded media convenience download
  with a streamed request that rejects each chunk above 25 MiB. Twelve focused
  Matrix core tests passed, including an oversized-stream rejection. Attachment
  controls, preview/open behavior, image/video/audio variants, an adversarial
  homeserver response, and macOS UI upload/download have not passed acceptance.
- A media hardening pass disabled automatic HTTP redirects, checked each
  authenticated-media 307/308 target before connecting, dropped the Matrix
  token after leaving the homeserver origin, and bounded redirect count,
  response streaming, and token-error parsing. The focused Matrix library
  suite passed 13/13 and the encrypted Alice/Bob attachment roundtrip passed
  again. Public HTTPS CDN redirects and an expired-token refresh still need
  live fixture checks. A further pass routes every off-origin CDN request
  through a no-proxy client with a custom reqwest DNS resolver. It rejects
  any answer set containing a non-public IP before reqwest connects to the
  checked socket addresses. The original homeserver retains normal proxy
  behavior; CDN redirects requiring a system proxy fail closed. Seven focused
  media unit checks passed, including public/private address classification,
  mixed-answer rejection, and a local HTTP connector refusing loopback. The
  disposable encrypted Alice/Bob attachment roundtrip passed once more. A live
  public CDN redirect and DNS-rebinding fixture have not yet been exercised.
- A follow-up code review found that the SDK retains requests after both
  recoverable and unrecoverable send errors. Morpheus now preserves the exact
  transaction and observes its eventual result; the selected account's room
  queues retry recoverable errors after 2–60 seconds. Re-selecting the active
  account also resumes paused room queues. A retained error no longer restores
  a draft or suggests a duplicate send. The focused Matrix suite passed 9/9,
  including a retained-then-sent event test. Automatic retry during a real
  network outage has not yet had a fault-injection acceptance run.
- Native attachment selection and save dialogs, a timeline attachment card,
  progress, cancellation, and same-transaction retry for a wedged upload are
  wired in the app. Save uses a user-chosen path and an atomic file replacement;
  the account and selected-room guards remain held through that write. App
  native-call check and 8/8 focused app tests passed. The gated encrypted
  Alice/Bob attachment test passed again with a receiver timeline filename
  assertion; disposable Synapse servers were stopped afterward. Live GUI
  click-through and image/video/audio previews remain open.
- The RTC core now exposes call-bound microphone mute and local deafen
  controls and rejects stale publication/control handles after leave and
  rejoin. Its device-audio suite passed 24/24. The app now has call-bound
  mute/deafen controls with generation guards; live device and cross-platform
  behavior remain open.
- The Element Call v0.26.1 browser peer passed the opt-in legacy `RoomState`
  compatibility test against a fresh local encrypted room. Morpheus saw the
  browser join, decoded microphone PCM and red/green I420 video, and observed
  departure; the final fresh-fixture run passed in 16.48 seconds. The browser
  used generated tracks and a disposable, cross-signed device. Native
  Alice/Bob current-mode media, roster, leave, and rejoin passed again in
  19.04 seconds with the pinned core's lowercase `m.call#room` default. An
  uppercase `m.call#ROOM` current-mode native regression failed roster
  discovery, so it is not the default. Element Call's current `matrix_2_0`
  mode did not pass the local fixture; see the [RTC runbook](../tests/LOCAL_MATRIX_RTC.md).
- A fresh stock Element Call v0.26.1 `matrix_2_0`/`Sticky2025` attempt
  received and decrypted both membership events, and its SDK calculated two
  members from them. Its live call-view and session-manager arrays remained
  empty. The sticky update callback had fired while both events still had
  type `m.room.encrypted`; after decryption no further update refreshed the
  live arrays. Morpheus observed the browser participant, but camera
  subscription timed out at 49.25 seconds, so the interop test failed. A
  separate explicit `refreshRoom` diagnostic had passed media transport;
  that intervention is outside stock Element Call behavior. See the
  [RTC runbook](../tests/LOCAL_MATRIX_RTC.md).

- A separate fresh account on loopback Synapse B, which advertises no
  MatrixRTC LiveKit transport, created an encrypted private room. An opt-in
  native test passed 1/1 in 4.74 seconds: discovery, slot opening, and joining
  each returned `NoLiveKitTransport`. Complete server room-state reads showed
  no `m.rtc.slot` or `org.matrix.msc4143.rtc.slot` before or after the call
  attempts. The same account then sent an encrypted chat message; its queue
  completed, the SDK timeline showed the body, and Synapse stored ciphertext.
  This checks the chat-preserving service gate on B, not other homeservers or
  the desktop error view. See the [RTC runbook](../tests/LOCAL_MATRIX_RTC.md).

## 2026-10-10 local administration and recovery checks

- A new ad hoc signed macOS `.app` and isolated profile signed into an existing
  disposable account. Before recovery it showed Device Unverified and Recovery
  Incomplete. Importing that account's protected recovery key in the native
  Security UI completed with Device Verified, Recovery Enabled, and Key backup
  Enabled. A second fresh unverified app requested own-device SAS; the
  recovered device accepted, the requester started, and the responder
  accepted the SAS method. Both displayed identical emoji and three decimal
  numbers. After a private human comparison, each app explicitly confirmed;
  both devices reached Verified. The requester briefly displayed “Could not
  check verification status” after confirmation because the completed request
  was pruned; manual Refresh showed Verified. The completion UI now checks
  durable device trust, with a focused test passing; a live retest of this fix
  remains open. No recovery key, emoji, number, or account secret is recorded
  here. This was a macOS live UI check; other platforms remain open.
- A focused loopback Synapse moderation test passed (1 test, 0 failures;
  22.80-second run after a 9.35-second compile). It
  exercised room rename/topic, server rejection of an unauthorized rename,
  power promotion/demotion, redaction of another member's message, invite,
  kick/reinvite, ban/blocked join, and unban/rejoin. The account-bound
  invite/kick/ban/unban and rename/topic/redaction wrappers rejected a
  mismatched selected account.
- In a separate disposable macOS Slint profile, a room creator invited a
  guest, raised the guest's exact Matrix power level to 50, kicked and
  reinvited the guest, banned the guest, then unbanned them. The UI reported
  each action and direct server reads confirmed the power and membership
  changes. The destructive actions required confirmation; Cancel, Tab to
  Confirm, and Escape were exercised. The roster needed a manual Refresh after
  each action because the SDK snapshot briefly retained older membership
  state. A bounded settle-and-refresh change is awaiting build and UI retest.
  Permission failure was covered in the core test, not this UI pass.
- A second fresh macOS profile created a disposable room, saved its new name
  and topic through the Room details dialog, sent a message, and opened its
  redaction confirmation. Escape canceled the first attempt; on reopening,
  Tab moved from Cancel to Redact and Return confirmed. The UI displayed the
  saved room details and a redaction success notice. Direct Synapse reads
  confirmed the exact room name and topic state values, a redacted message
  event, and absence of the original message body. This checks one macOS UI
  path; a GUI permission denial, the revised automatic roster refresh, and
  Windows/Linux paths remain untested.
- A fresh macOS recovery profile imported an existing backup key and then
  displayed two historical encrypted events. Loading earlier history before
  and after the import produced no UI error. The SDK still showed Backup
  Enabled, Recovery Incomplete, and Device Unverified in this fixture; the
  second device had not accepted SAS. This is a decryption UI pass, not proof
  of complete recovery or verified identity.
- Opt-in Markdown outbound content passed 4/4 focused serialization tests:
  the Matrix plain `body` retains source text, safe Markdown becomes formatted
  HTML, raw HTML tags remain literal text, and unsafe link schemes do not
  survive. The updated native-call app suite passed 28/28, including the
  completed-SAS-request fallback and notification policy checks. The app
  binary built on macOS Apple Silicon. This preceded the later native rich
  timeline renderer.
- A separate opt-in loopback test passed 1/1 in 9.92 seconds against a fresh
  encrypted two-person room. Alice sent a formatted main message, reply, and
  edit; Bob's SDK decrypted each event. The test checked readable Matrix
  `body`, sanitized `formatted_body`, the reply and replacement relations,
  and an outer `m.room.encrypted` event on Synapse for all three sends. After
  switching from Alice to Carol on the second homeserver, stale Alice send,
  reply, and edit commands were rejected. This verifies the core send path on
  local Synapse, not a native composer click-through or another platform.
- After adding native `StyledText` display, four focused converter tests passed
  for supported formatting, escaped literal markup, safe links/lists, and
  bounds. A fresh encrypted two-device test passed 1/1 in 10.61 seconds:
  Bob's decrypted main message and edit both reached the safe rich projection,
  with the unchanged Matrix plain body still available. That native-call app
  suite passed 31/31, including the browser-link guard. A live decrypted
  rich-rendering click-through and other-platform acceptance remain open.
- A native macOS click-through reused the exact signed canonical-ID QA app and
  protected two-homeserver session from notification testing, then created a
  fresh private room on loopback Synapse B and enabled Megolm before sending.
  The **Md** button changed from “Use Markdown formatting” to “Clear Markdown
  formatting”; the UI sent `morpheus-ui-md-qa-200a **bold**
  <script>alert(1)</script>` and showed the source in its timeline. With **Md**
  off, it sent a second literal-source control message. Direct server reads
  confirmed both corresponding events (`$uHPp_CFocFIQvSXQtzjkkYEs6jQgv6CijQMWWUMwCpY`
  and `$uCBqr_O3Iyy1sN6eRbBm6RQmUqjSyCUAqy1baZ2_fKE`) were
  `m.room.encrypted` with ciphertext and no plaintext formatting fields.
  The choice was plain in another room, restored to on when returning to the
  QA room, stayed plain on account A, and restored to on after switching back
  to account B. This proves the native toggle, send route, and session-local
  account/room mode scoping; the exact UI-sent ciphertext was not decrypted
  to inspect its inner `formatted_body`. The separate two-device core test
  above covers formatted payload and decryption. Windows/Linux UI acceptance
  remains open.
- A rebuilt, ad hoc signed macOS QA app used a fresh account-free profile and
  a deterministic styled sample in the interactive `#general` preview. In a
  native screenshot, Sam's `great` rendered bold in the timeline. Its
  accessibility text remained the unmarked plain sentence. Opening Sam's
  thread initially exposed a plain-text root; after changing the root to the
  same safe `StyledText` component used by the timeline and replies, a rebuilt
  screenshot showed `great` bold in both places, with the thread root's plain
  accessibility label and `REPLIES · 0` composer intact. A final rebuild after
  adding a display-aligned accessible projection and preserving rich edit
  source passed the native-call app suite 33/33; another native screenshot
  showed bold `great` in both places and AX returned the unmarked sentence
  for both. Clicking Edit on a plain preview message loaded its original
  source and the “Save edit” control. The copied two-account
  notification QA profile did not restore in the re-signed bundle: the app
  stayed at disabled “Signing in…” despite receiving its profile path. No
  credentials were entered. This verifies native preview rendering and thread
  layout, not a live encrypted event displayed by the new renderer.
- A fresh encrypted two-device native MatrixRTC test passed 1/1 in 18.34
  seconds. After Bob left, Alice's local call reported media-key index 1;
  after Bob rejoined, he imported Alice's key and decoded newly published
  camera frames. This checks leave-triggered key rotation and fresh-join
  media, not recovery from a dropped network transport. The earlier screen
  and camera assertions in the same test also passed.
- A later fresh-fixture native RTC run passed 1/1 in 18.72 seconds after
  closing Alice's LiveKit signalling socket through the pinned test hook.
  The existing call reported degraded then healthy media connection without
  leaving its MatrixRTC membership; Bob decoded a new encrypted camera
  publication after the resume. Leave-triggered key rotation and post-rejoin
  media passed again. This is signal-resume evidence, not a full network outage
  or SFU restart check.
- A signed canonical-ID macOS QA app installed in `~/Applications` signed in
  to two disposable homeservers. An alternate-room message for active account
  A generated a `usernoted` request under `io.github.brk275.morpheus`; after
  temporarily enabling the app in Notifications and Sleep Focus, macOS logged
  delivery to alert, lock screen, and Notification Center with no Focus
  suppression. A message in the visible room produced no request; after
  switching to B, an A message produced none; and after both QA app processes
  fully quit, a B message produced none. The prior notification and Focus
  settings were restored and the QA installation removed. This is OS delivery
  evidence, not a captured banner or preview-content check. Linux/Windows,
  notification sound, and packaged release behavior remain open.
- A separate ad hoc signed macOS Apple Silicon debug app used a fresh,
  account-free offline profile to click through the new Notifications settings.
  Accessibility state showed desktop notifications change On→Off, sounds
  On→Off, and message previews Off→On. `config.toml` saved the corresponding
  `enabled = false`, `sounds = false`, and `show_message_preview = true` values.
  Adding `  Release   Candidate  ` displayed and saved `Release Candidate`;
  adding the same phrase in lowercase showed a duplicate error, and a phrase
  containing U+202E showed the control/direction-character error. The switches
  and phrase survived Home→Settings navigation and a quit/relaunch of that
  profile. An external, atomic TOML edit then changed the switches to On, On,
  Off and added `Watcher QA`; all four changes appeared live in Settings.
  Referencing a malformed theme pack showed the TOML parse error while the
  last valid notification controls remained visible; removing the reference
  cleared the error. That first binary left a highlight validation error
  visible after the input was cleared. In a later rebuilt, account-free macOS
  QA app, adding `Release Candidate`, attempting its lowercase duplicate,
  clearing the input, and navigating Home→Settings showed the duplicate error
  disappear on edit and stay cleared after navigation; the saved phrase stayed
  visible. This is preferences UI and file watcher evidence on macOS, not a
  notification banner, sound, or Linux/Windows acceptance check.

## Native outgoing video GUI acceptance (partial)

An ad hoc signed macOS Apple Silicon QA app used a temporary, debug-only
in-process login bootstrap with a protected disposable fixture. This bypassed
a Keychain ACL timeout when a separately signed test seeder tried to restore
its account in the GUI; the app's ordinary sign-in path has separate earlier
QA coverage. On the fresh `46632fc9428249d8` encrypted room, Bob's verified
device was running **before** Alice opened the call. The GUI joined, displayed
both participants, and reported active display sharing. Bob saw Alice as
reachable and decoded **one nonblank encrypted ScreenShare I420 frame**. The
GUI returned Share to Off after Stop and cleared its local call state on Leave.
This establishes a real GUI-to-peer frame path, but one frame does not establish
sustained picture or remote publication removal after Stop.

On a second fresh encrypted room (`93241672ba46d084`), Bob was again signed
in and verified before Alice opened the slot. Alice's signed GUI showed both
participants and an active display share. Bob decoded **10 nonblank encrypted
ScreenShare I420 frames** from Alice. This establishes sustained GUI display
picture delivery on the tested macOS machine. Alice then stopped sharing and
left; the GUI cleared both local controls. Bob-side publication removal after
Stop was not asserted in this run.

The same run's FaceTime HD Camera selection never reached Active: the UI
showed Starting, then Stopping, then a 20-second no-frame error. The pinned
Nokhwa macOS backend maps the camera's native 8-bit `420v` format incorrectly
and had an unbounded frame receive. A locally patched Nokhwa build in the
second run bounded frame receive and requested 8-bit NV12; the FaceTime camera
still produced no first frame, ending with a three-second channel timeout.
Further session/delegate diagnostics and camera publication remain open.
Earlier fresh-device observer attempts started Bob **after** Alice's encrypted
sticky membership was sent. Bob saw only himself for 45 seconds although
Alice saw Bob. When Bob's device existed first, Bob saw Alice as reachable.
The working hypothesis is that Bob could not decrypt Alice's historical
Megolm membership and the RTC sticky feeder projected only decrypted live
events. This is a beta call-join limitation requiring recovery or membership
republication; it is not evidence that every late call join fails.

Docker Desktop initially stopped responding but recovered; local Synapse,
native LiveKit, and RTC transport probe passed. Generated-pixel and command-line
capture proofs remain separate from the GUI evidence above.

When the disposable stack is healthy, run one bounded macOS acceptance pass:

1. Use the [RTC fixture runbook](../tests/LOCAL_MATRIX_RTC.md) to create a fresh
   encrypted Alice/Bob room. Keep credentials in its mode-0600 fixture file.
   Start Bob as a separately verified device in the headless RTC observer,
   then sign Alice into an isolated Morpheus app profile and join that room's
   call. Verify the two-person roster first. Repeat with a newly created Bob
   device joining after Alice to exercise encrypted membership recovery.
2. Open Camera, choose one labeled source, and wait at most 30 seconds for the
   UI to report it active. Bob must observe an unmuted Camera track and decode
   at least 10 nonblank frames with nonzero dimensions in 30 seconds. Stop
   camera; Bob must see that publication end within 15 seconds and the UI must
   show it off. Repeat once to catch a stop/start device race.
3. Share a display, then a window, one at a time. For each, inspect the picker
   label before selection, confirm the OS permission prompt if shown, require
   the UI active state and 10 nonblank decoded ScreenShare frames at Bob
   within 30 seconds, then Stop share and require publication removal within
   15 seconds. A changed window title or display geometry deliberately ends
   capture and requires reselection; check that the UI reports this rather
   than continuing on a changed source. The publication uses 1280×720 frames
   while its selected source identity remains unchanged.
4. Start camera and share together, then Leave. Require both local active
   indicators to clear and both Bob publications to end within 15 seconds.
   Repeat while choosing the bottom-left other account: the leave-call dialog
   must appear; cancelling retains the call and tracks, confirming leaves and
   switches only after both publications stop. No Alice source may appear in
   the new account's call stage.

Record app build, macOS and permission state, fixture mode, Bob's decoded
frame counts/dimensions, and any timeout. Keep the disposable credential file
out of logs. This checks one macOS run; Windows/Linux and packaged-app
permissions remain separate release gates.

## Earlier GitHub CI history

These results predate public-source publication. The newest candidate and its
remaining gates are recorded at the top of this log. Historical unsigned
package runs and their downloadable artifacts were removed before publication.

[Run 38012987752](https://github.com/brk275/Morpheus/actions/runs/38012987752)
passed on macOS Apple Silicon and Intel; Windows lacked `sqlite3.lib`, and
Ubuntu lacked `glib-2.0.pc` for the native-call build. Those CI dependencies
were fixed before [run 38019553752](https://github.com/brk275/Morpheus/actions/runs/38019553752),
which passed Ubuntu and both macOS targets. Its Windows native app link failed
because the pinned WebRTC library uses the static MSVC CRT while Rust had used
the dynamic CRT. A target-specific static-CRT build flag was checked in
[run 38021445771](https://github.com/brk275/Morpheus/actions/runs/38021445771):
Ubuntu and both macOS jobs passed; the Windows tests, desktop check, and
native-call app build passed. The final run conclusion was success on all four
jobs. The native audio mixer test runs on macOS Apple Silicon and passed there;
it is skipped on the other runners. [Run 38023378684](https://github.com/brk275/Morpheus/actions/runs/38023378684)
passed all four jobs for the subsequent room, keyboard, and video-feasibility
commit.
[Run 38024603658](https://github.com/brk275/Morpheus/actions/runs/38024603658)
passed all four jobs for the encrypted-drafts and macOS real-capture commit.
[Run 38026269910](https://github.com/brk275/Morpheus/actions/runs/38026269910)
passed all four jobs for the encrypted-attachment and native call-stage commit.
At that point, the newer discovery, streaming media, call-control, and slot
changes had not yet run in GitHub CI.
[Unsigned internal package QA run 38029335100](https://github.com/brk275/Morpheus/actions/runs/38029335100)
passed macOS Intel, Linux x64, and Windows x64 packaging jobs; its macOS
Apple Silicon job failed before steps due Actions capacity/budget. The latest
[CI run 38032533729](https://github.com/brk275/Morpheus/actions/runs/38032533729)
did not start any platform job because GitHub reported an Actions budget
preventing further use. That candidate had no four-platform CI result.
[Run 38042914642](https://github.com/brk275/Morpheus/actions/runs/38042914642)
for commit `31a2d65` also failed before any job steps on all four runners:
GitHub reported the same Actions budget restriction. This is a CI capacity
failure, not evidence of a source or platform build failure.

## Native timeline stress preview

An isolated macOS Apple Silicon debug app loaded 10,000 mixed-height synthetic
messages in the offline preview. The status was visible within 531 ms of the
UI action plus accessibility observation. Full accessibility trees exposed
only 5–7 numbered message rows per sampled viewport, including correct
consecutive ranges at the beginning, around message 5,200, and through
message 10,000. Ten page scrolls and three scrollbar seeks completed without
blank sampled views; the full timings, memory comparison, renderer counts,
and measurement limits are in [the 10,000-row QA record](../tests/TIMELINE_10K.md).
This synthetic check does not close the real Matrix room performance gate.

## Still required for beta

The current checks do not establish current-format Element Call interoperability,
live received video in the Morpheus call stage,
packaged-app screen-sharing controls and permissions, Linux or Windows GUI
behavior, screen-reader acceptance, install/update/remove behavior, signed
packages, or Cinny-level messaging and administration breadth.

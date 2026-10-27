# Verification

Morpheus is pre-alpha software with no approved installer release. Verification
is recorded by producing revision, selected checks and platform. Source review,
helper checks, native tests and installed-package acceptance are separate evidence.

The [feature matrix](FEATURE_MATRIX.md), [packaging gates](PACKAGING.md) and
[release checklist](RELEASE_CHECKLIST.md) define the remaining release work.
The repository's [build and review policy](../AGENTS.md) governs check selection.

## Source and results

These are fixed checkpoints, not a claim that every check passed on one revision:

| Source | Producing record | Scope and limit |
| --- | --- | --- |
| [Main checkpoint `07e568f`](https://github.com/brk275/Morpheus/commit/07e568f0a5bc36d84682d6cc096c3f6d256ebeaa) | [CI 40525793578](https://github.com/brk275/Morpheus/actions/runs/40525793578) | Validation helpers passed. This is not a full native matrix or release result. |
| `07e568f` | [Windows host inventory 40526392503](https://github.com/brk275/Morpheus/actions/runs/40526392503) | Failed. Windows source-build admission remains open. |
| [Candidate `2b4f276`](https://github.com/brk275/Morpheus/commit/2b4f276379692e3e3e50b48bb80e5ade9e3f345d) | Source-reviewed candidate | A source review is not combined platform verification, beta readiness or release approval. |

Installed-package, physical-device, recovery, SSO, full interoperability,
screen-reader, licensing, signing, update and beta release gates remain open.
Installer downloads remain disabled. See the [release checklist](RELEASE_CHECKLIST.md).

## Explicit manual CI selection

Canonical `CI` manual dispatch defaults to `platform=all`, `checks=all` and
`matrix_suite=full`: the complete Apple Silicon, Intel Mac, Windows and Linux
native matrix and every existing check. Shared native changes and release
acceptance still require that full scope. Automatic PR/main impact selection is
unchanged and cannot consume manual narrowing inputs.

After source review, a focused retry may select `linux`, `windows`, `macos-arm`
or `macos-intel`, and one existing route: `models`, `desktop`, `desktop-video`,
`devices`, `vendor`, `sdk`, `matrix` or `wayland`. `helpers` runs only the existing Linux validation
helper job; `config` selects the existing Linux configuration job. Security
history scanning and helper tests remain mandatory for every route. Focused
Matrix/Wayland require explicit `platform=linux`. Only the Matrix route accepts
one of the harness's existing non-full `--suite` choices; all other routes
require `matrix_suite=full`. Unknown or incompatible choices fail closed.

Routes select existing commands and their required setup/build inputs. Matrix
and owned capture drivers compile their own existing test targets; focused
routes omit unrelated desktop builds. The three core-only Matrix suites
(`drafts-only`, `timeline-failures`, `media-only`) also omit the unrelated owned
native screen client build. These partial results do not establish production
app linkage or omitted tests. Routes do not create new test suites or relax assertions, deadlines, containment, cleanup
or private log scans. The native outcome gate requires each selected command to
run and pass and each omitted command to remain skipped. Preview publication
remains limited to its existing reviewed automatic route. Manual invocations
are public-repository, owner-only, standard hosted runs, with read-only tokens,
no persisted checkout credentials and unchanged trusted-main-only cache writes.
A focused result is partial evidence, never full matrix or beta acceptance.
Focused native jobs and the aggregate use distinct names; they do not emit the
canonical full `CI acceptance` status.
Per-source/per-OS concurrency serializes runs of the same candidate and platform;
inspect active jobs/results before dispatch rather than queueing a duplicate.

PR and main runs are not automatically interchangeable. PR jobs test GitHub's
synthetic merge revision using the reviewed base classifier and restore-only
caches; main jobs test the actual merged revision, current classifier and main
cache/provenance context. A green PR alone does not establish equal source,
dependency, toolchain, build and verification inputs. Reuse requires exact producing-source/input equivalence and immutable results;
a passing result from different inputs is not merge verification.

## Recording and interpreting results

For each result, identify its producing source and tree, selected checks,
platform and relevant dependency, build and toolchain inputs. Record failures,
omissions and cleanup alongside passes. Link the producing run; do not copy a
passing status onto changed inputs or infer a production fix from a diagnostic run.

A source-only candidate, compile-only check or focused fixture does not establish
all-platform, installed-package, physical-device or full interoperability acceptance.
Review source and dependencies before execution, then select the affected checks
under [AGENTS.md](../AGENTS.md). No result on this page authorizes a release.

## Historical producing records

The [verification history](VERIFICATION_HISTORY.md) preserves the earlier log,
including original producing revisions, failures and limitations. Its dated
“current” sections are historical snapshots. Use those records for the exact
scope they establish; later source changes require applicable new evidence.

## Still required for beta

The current checks do not establish current-format Element Call interoperability,
live received video in the Morpheus call stage,
packaged-app screen-sharing controls and permissions, complete installed-app
interaction on Linux and Windows, screen-reader acceptance, install/update/remove
behavior, signed packages, or the full messaging and administration requirements.

<details>
<summary>Links to earlier verification sections</summary>

<a id="verification-log"></a>
- [Verification log](VERIFICATION_HISTORY.md#verification-log)
<a id="current-status-2026-10-22"></a>
- [Current status: 2026-10-22](VERIFICATION_HISTORY.md#current-status-2026-10-22)
<a id="current-producing-results"></a>
- [Current producing results](VERIFICATION_HISTORY.md#current-producing-results)
<a id="earlier-qualified-producing-results"></a>
- [Earlier qualified producing results](VERIFICATION_HISTORY.md#earlier-qualified-producing-results)
<a id="appearance-and-keyboard-validation-2fddb7f"></a>
- [Appearance and keyboard validation: `2fddb7f`](VERIFICATION_HISTORY.md#appearance-and-keyboard-validation-2fddb7f)
<a id="installed-linux-keyboard-check-4037b3a"></a>
- [Installed Linux keyboard check: `4037b3a`](VERIFICATION_HISTORY.md#installed-linux-keyboard-check-4037b3a)
<a id="installed-linux-keyboard-progress-cad1abc"></a>
- [Installed Linux keyboard progress: `cad1abc`](VERIFICATION_HISTORY.md#installed-linux-keyboard-progress-cad1abc)
<a id="windows-native-vault-acceptance-cad1abc"></a>
- [Windows native vault acceptance: `cad1abc`](VERIFICATION_HISTORY.md#windows-native-vault-acceptance-cad1abc)
<a id="per-account-theme-validation-and-integration-0d75626"></a>
- [Per-account theme validation and integration: `0d75626`](VERIFICATION_HISTORY.md#per-account-theme-validation-and-integration-0d75626)
<a id="development-check-selection"></a>
- [Development check selection](VERIFICATION_HISTORY.md#development-check-selection)
<a id="native-vault-and-installed-accessibility-diagnostics-3fe1285"></a>
- [Native vault and installed accessibility diagnostics: `3fe1285`](VERIFICATION_HISTORY.md#native-vault-and-installed-accessibility-diagnostics-3fe1285)
<a id="native-vault-and-installed-accessibility-diagnostics-ff2c353"></a>
- [Native vault and installed accessibility diagnostics: `ff2c353`](VERIFICATION_HISTORY.md#native-vault-and-installed-accessibility-diagnostics-ff2c353)
<a id="windows-native-acceptance-74bcac2"></a>
- [Windows native acceptance: `74bcac2`](VERIFICATION_HISTORY.md#windows-native-acceptance-74bcac2)
<a id="linux-installed-startup-and-accessibility-74bcac2"></a>
- [Linux installed startup and accessibility: `74bcac2`](VERIFICATION_HISTORY.md#linux-installed-startup-and-accessibility-74bcac2)
<a id="linux-package-startup-diagnostic-0f4f170"></a>
- [Linux package startup diagnostic: `0f4f170`](VERIFICATION_HISTORY.md#linux-package-startup-diagnostic-0f4f170)
<a id="ordinary-unsigned-package-validation-a38fe43"></a>
- [Ordinary unsigned package validation: `a38fe43`](VERIFICATION_HISTORY.md#ordinary-unsigned-package-validation-a38fe43)
<a id="native-and-live-acceptance-a38fe43"></a>
- [Native and live acceptance: `a38fe43`](VERIFICATION_HISTORY.md#native-and-live-acceptance-a38fe43)
<a id="remaining-system-wayland-failure"></a>
- [Remaining system Wayland failure](VERIFICATION_HISTORY.md#remaining-system-wayland-failure)
<a id="sign-in-compilation-repair-f4170ba"></a>
- [Sign-in compilation repair: `f4170ba`](VERIFICATION_HISTORY.md#sign-in-compilation-repair-f4170ba)
<a id="native-and-live-acceptance-51c04ce"></a>
- [Native and live acceptance: `51c04ce`](VERIFICATION_HISTORY.md#native-and-live-acceptance-51c04ce)
<a id="windows-capture-failure"></a>
- [Windows capture failure](VERIFICATION_HISTORY.md#windows-capture-failure)
<a id="live-acceptance-stack-overflow-diagnosis-2026-10-15"></a>
- [Live acceptance stack overflow diagnosis, 2026-10-15](VERIFICATION_HISTORY.md#live-acceptance-stack-overflow-diagnosis-2026-10-15)
<a id="completed-native-run-93d1358"></a>
- [Completed native run: `93d1358`](VERIFICATION_HISTORY.md#completed-native-run-93d1358)
<a id="account-access-and-style-expansion-2026-10-12"></a>
- [Account access and style expansion, 2026-10-12](VERIFICATION_HISTORY.md#account-access-and-style-expansion-2026-10-12)
<a id="hosted-candidate-stopped-at-ui-compilation-cab6c91"></a>
- [Hosted candidate stopped at UI compilation: `cab6c91`](VERIFICATION_HISTORY.md#hosted-candidate-stopped-at-ui-compilation-cab6c91)
<a id="follow-up-native-execution-65b4ceb"></a>
- [Follow-up native execution: `65b4ceb`](VERIFICATION_HISTORY.md#follow-up-native-execution-65b4ceb)
<a id="completed-prior-candidate-6abdfb7"></a>
- [Completed prior candidate: `6abdfb7`](VERIFICATION_HISTORY.md#completed-prior-candidate-6abdfb7)
<a id="current-hosted-acceptance-2026-10-11"></a>
- [Current hosted acceptance, 2026-10-11](VERIFICATION_HISTORY.md#current-hosted-acceptance-2026-10-11)
<a id="pending-native-verification-emoji-catalog-and-incoming-call-sound"></a>
- [Pending native verification: emoji catalog and incoming-call sound](VERIFICATION_HISTORY.md#pending-native-verification-emoji-catalog-and-incoming-call-sound)
<a id="room-settings-candidate-stopped-at-compilation-8a1ca01"></a>
- [Room-settings candidate stopped at compilation: `8a1ca01`](VERIFICATION_HISTORY.md#room-settings-candidate-stopped-at-compilation-8a1ca01)
<a id="real-native-preview-and-expanded-live-fixtures-c02a1b7"></a>
- [Real native preview and expanded live fixtures: `c02a1b7`](VERIFICATION_HISTORY.md#real-native-preview-and-expanded-live-fixtures-c02a1b7)
<a id="unverified-source-in-8a1ca01-room-settings-and-departure-observation"></a>
- [Unverified source in `8a1ca01`: room settings and departure observation](VERIFICATION_HISTORY.md#unverified-source-in-8a1ca01-room-settings-and-departure-observation)
<a id="native-suites-pass-live-projection-and-actor-setup-remain-blocked-4fd8071"></a>
- [Native suites pass; live projection and actor setup remain blocked: `4fd8071`](VERIFICATION_HISTORY.md#native-suites-pass-live-projection-and-actor-setup-remain-blocked-4fd8071)
<a id="composer-compiler-failures-and-palette-follow-up-03cb9d5"></a>
- [Composer compiler failures and palette follow-up: `03cb9d5`](VERIFICATION_HISTORY.md#composer-compiler-failures-and-palette-follow-up-03cb9d5)
<a id="id-comparison-build-failure-and-image-preview-candidate-7357111"></a>
- [ID comparison build failure and image-preview candidate: `7357111`](VERIFICATION_HISTORY.md#id-comparison-build-failure-and-image-preview-candidate-7357111)
<a id="composer-build-failure-and-reviewed-follow-up-56688d0"></a>
- [Composer build failure and reviewed follow-up: `56688d0`](VERIFICATION_HISTORY.md#composer-build-failure-and-reviewed-follow-up-56688d0)
<a id="incoming-controls-and-membership-renewal-c7ad50c"></a>
- [Incoming controls and membership renewal: `c7ad50c`](VERIFICATION_HISTORY.md#incoming-controls-and-membership-renewal-c7ad50c)
<a id="composer-candidate-source-review-only"></a>
- [Composer candidate: source review only](VERIFICATION_HISTORY.md#composer-candidate-source-review-only)
<a id="installed-appimage-capture-candidate"></a>
- [Installed AppImage capture candidate](VERIFICATION_HISTORY.md#installed-appimage-capture-candidate)
<a id="incoming-call-controls-implementation"></a>
- [Incoming call controls implementation](VERIFICATION_HISTORY.md#incoming-call-controls-implementation)
<a id="fresh-device-call-membership-renewal-candidate"></a>
- [Fresh-device call membership renewal candidate](VERIFICATION_HISTORY.md#fresh-device-call-membership-renewal-candidate)
<a id="hosted-result-of-the-renewal-candidate"></a>
- [Hosted result of the renewal candidate](VERIFICATION_HISTORY.md#hosted-result-of-the-renewal-candidate)
<a id="linux-pipewire-startup-comparison"></a>
- [Linux PipeWire startup comparison](VERIFICATION_HISTORY.md#linux-pipewire-startup-comparison)
<a id="stable-call-slot-lookup-correction"></a>
- [Stable call-slot lookup correction](VERIFICATION_HISTORY.md#stable-call-slot-lookup-correction)
<a id="native-preview-and-microphone-cancellation-results"></a>
- [Native preview and microphone cancellation results](VERIFICATION_HISTORY.md#native-preview-and-microphone-cancellation-results)
<a id="previous-policy-correction-and-precise-remaining-failures"></a>
- [Previous policy correction and precise remaining failures](VERIFICATION_HISTORY.md#previous-policy-correction-and-precise-remaining-failures)
<a id="native-screenshot-capture-2026-10-11"></a>
- [Native screenshot capture, 2026-10-11](VERIFICATION_HISTORY.md#native-screenshot-capture-2026-10-11)
<a id="screenshot-and-microphone-cancellation-candidate-preparation"></a>
- [Screenshot and microphone cancellation candidate preparation](VERIFICATION_HISTORY.md#screenshot-and-microphone-cancellation-candidate-preparation)
<a id="previous-incoming-admission-policy-regression-and-correction"></a>
- [Previous incoming admission policy regression and correction](VERIFICATION_HISTORY.md#previous-incoming-admission-policy-regression-and-correction)
<a id="passive-observer-and-capture-evidence-run"></a>
- [Passive observer and capture evidence run](VERIFICATION_HISTORY.md#passive-observer-and-capture-evidence-run)
<a id="previous-corrected-sdk-and-receipt-baseline"></a>
- [Previous corrected SDK and receipt baseline](VERIFICATION_HISTORY.md#previous-corrected-sdk-and-receipt-baseline)
<a id="incoming-admission-and-late-join-diagnostic-candidate"></a>
- [Incoming admission and late-join diagnostic candidate](VERIFICATION_HISTORY.md#incoming-admission-and-late-join-diagnostic-candidate)
<a id="previous-sdk-test-setup-failure"></a>
- [Previous SDK test setup failure](VERIFICATION_HISTORY.md#previous-sdk-test-setup-failure)
<a id="previous-completed-run-with-live-acceptance"></a>
- [Previous completed run with live acceptance](VERIFICATION_HISTORY.md#previous-completed-run-with-live-acceptance)
<a id="receipt-patch-and-live-harness-changes"></a>
- [Receipt patch and live harness changes](VERIFICATION_HISTORY.md#receipt-patch-and-live-harness-changes)
<a id="earlier-completed-candidate"></a>
- [Earlier completed candidate](VERIFICATION_HISTORY.md#earlier-completed-candidate)
<a id="focused-linux-fixture-follow-up"></a>
- [Focused Linux fixture follow-up](VERIFICATION_HISTORY.md#focused-linux-fixture-follow-up)
<a id="account-selection-and-ringing-validation-work"></a>
- [Account-selection and ringing validation work](VERIFICATION_HISTORY.md#account-selection-and-ringing-validation-work)
<a id="callback-correction-and-capture-diagnostics"></a>
- [Callback correction and capture diagnostics](VERIFICATION_HISTORY.md#callback-correction-and-capture-diagnostics)
<a id="outgoing-invitation-candidate"></a>
- [Outgoing invitation candidate](VERIFICATION_HISTORY.md#outgoing-invitation-candidate)
<a id="beta-gate-implementation-batch-2026-10-10"></a>
- [Beta gate implementation batch, 2026-10-10](VERIFICATION_HISTORY.md#beta-gate-implementation-batch-2026-10-10)
<a id="explicit-update-controls-and-shortcut-compilation-fix-2026-10-10"></a>
- [Explicit update controls and shortcut compilation fix, 2026-10-10](VERIFICATION_HISTORY.md#explicit-update-controls-and-shortcut-compilation-fix-2026-10-10)
<a id="hosted-result-and-focused-follow-up"></a>
- [Hosted result and focused follow-up](VERIFICATION_HISTORY.md#hosted-result-and-focused-follow-up)
<a id="linux-capture-and-fixture-follow-up-2026-10-11"></a>
- [Linux capture and fixture follow-up, 2026-10-11](VERIFICATION_HISTORY.md#linux-capture-and-fixture-follow-up-2026-10-11)
<a id="thread-drafts-and-windows-capture-follow-up"></a>
- [Thread drafts and Windows capture follow-up](VERIFICATION_HISTORY.md#thread-drafts-and-windows-capture-follow-up)
<a id="hosted-follow-up-results-2026-10-11"></a>
- [Hosted follow-up results, 2026-10-11](VERIFICATION_HISTORY.md#hosted-follow-up-results-2026-10-11)
<a id="public-source-publication-2026-10-10"></a>
- [Public source publication, 2026-10-10](VERIFICATION_HISTORY.md#public-source-publication-2026-10-10)
<a id="complete-native-ci-matrix"></a>
- [Complete native CI matrix](VERIFICATION_HISTORY.md#complete-native-ci-matrix)
<a id="2026-10-10-public-source-preparation"></a>
- [2026-10-10 public-source preparation](VERIFICATION_HISTORY.md#2026-10-10-public-source-preparation)
<a id="2026-10-09-local-desktop-and-matrix-checks"></a>
- [2026-10-09 local desktop and Matrix checks](VERIFICATION_HISTORY.md#2026-10-09-local-desktop-and-matrix-checks)
<a id="2026-10-10-focused-follow-up-checks"></a>
- [2026-10-10 focused follow-up checks](VERIFICATION_HISTORY.md#2026-10-10-focused-follow-up-checks)
<a id="2026-10-10-local-administration-and-recovery-checks"></a>
- [2026-10-10 local administration and recovery checks](VERIFICATION_HISTORY.md#2026-10-10-local-administration-and-recovery-checks)
<a id="2026-10-17-durable-draft-send-recovery-source-only-unverified"></a>
- [2026-10-17 durable draft-send recovery (source only, unverified)](VERIFICATION_HISTORY.md#2026-10-17-durable-draft-send-recovery-source-only-unverified)
<a id="2026-10-20-room-creation-and-owned-navigation-source-only-unverified"></a>
- [2026-10-20 room creation and owned navigation (source only, unverified)](VERIFICATION_HISTORY.md#2026-10-20-room-creation-and-owned-navigation-source-only-unverified)
<a id="native-outgoing-video-gui-acceptance-partial"></a>
- [Native outgoing video GUI acceptance (partial)](VERIFICATION_HISTORY.md#native-outgoing-video-gui-acceptance-partial)
<a id="earlier-github-ci-history"></a>
- [Earlier GitHub CI history](VERIFICATION_HISTORY.md#earlier-github-ci-history)
<a id="native-timeline-stress-preview"></a>
- [Native timeline stress preview](VERIFICATION_HISTORY.md#native-timeline-stress-preview)

</details>

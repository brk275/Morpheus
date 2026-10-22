# Packaging Morpheus

Shared package configuration is in [`Packager.toml`](../Packager.toml); Debian and other Linux settings live in [`packaging/linux.toml`](../packaging/linux.toml). The wrapper merges only the current platform's settings. Its build hook compiles `morpheus-app` in release mode with `native-calls` and the lockfile. Read [AGENTS.md](../AGENTS.md) before building; select the affected platform rather than starting a full packaging matrix.

```sh
cargo install cargo-packager --version 0.11.8 --locked
python3 scripts/package.py
```

The package output goes to `dist/`. This command does not make a public release. The packaged call feature still needs real MatrixRTC interoperability and device testing before a beta can be distributed.

## Selecting a reviewed package source

The manual **Package validation** workflow runs from `main`. Leave
`source_revision` blank to build that workflow revision, or enter a complete,
lowercase 40-character commit ID that has received source and dependency review.
Branch names, tags and shortened hashes are rejected. This input does not grant
source-review approval; follow [the contribution review policy](../AGENTS.md#review-externally-submitted-code-before-execution)
before dispatching it.

Package workers check out the selected commit and verify its identity and clean
tracked source before installing the toolchain or building. Before any worker
starts, the selection job scans the selected commit's complete fetched history
with the reviewed `scripts/scan_history.py` and `scripts/public-fixtures.json`,
both read from the workflow's own revision, never from the candidate: pinned
default secret rules on a bare mirror, excepting only reviewed public fixtures by
exact fingerprint and file content. Reports stay on the runner; the log shows
only a closed summary of counts and hashes. Candidates must include the current
package QA tools, including
`scripts/package_revision.py`. Package receipts and update-source exports identify
the selected source; `GITHUB_SHA` continues to identify the workflow itself.

Candidate revisions restore caches without writing shared entries. Only the
workflow's own `main` revision can populate the trusted cache. Package cache
namespaces include the root manifest and lockfile hashes, separating the verified
SDK downloader from legacy extraction caches. Select only the
affected platforms and check active runs first. QA installers remain on the
runner; signing, licensing and installed-package release gates still apply.

## Preparing corresponding source evidence

An explicit source-only path prepares the committed Rust workspace, local
patches and every source listed in its `Cargo.lock`. The bundle builder creates
no installer, runs no dependency hooks, performs no downloads and changes no release gate.
The ordinary `--check` and package commands keep their existing behavior.

The permanent **Package validation** workflow has an explicit `source_bundle`
input, off by default. Select `platform=linux`, `linux_formats=appimage`,
`source_bundle=true`, `appimage_capture=false` and `appimage_updates=false`.
Optionally provide the complete reviewed `source_revision`. Other combinations
fail selection. The source-only job runs after the same independent selected
source/history scan and holds the candidate's Linux concurrency lock. It
installs pinned Rust and the reviewed scanner; it never starts native builds,
installer preparation, capture, updates, uploads or shared-cache writes.

On the fresh worker, the production acquisition helper runs pinned Cargo
`fetch --locked` with a new private Cargo home and a separate working directory.
It permits only the three reviewed Git pins and the locked crates.io registry,
discards ambient credentials/configuration, authenticates every archive checksum
and locates mirrors by raw pinned-commit identity. This fetch does not run
dependency build scripts or procedural macros. Its owned namespace stops before
the workflow invokes the genuine outer `scripts/package.py` source command,
which owns a separate canonical namespace. No nested privilege workaround is
used. An always-run finalizer checks the source/tree, bundle/manifest hashes and
counts, scans segregated diagnostics and deletes the source caches, private plan
and output. Only its closed hash/count/cleanup result enters public logs; the
bundle remains unpublished and source-content/license review remains open.
The selected source must include the source-bundle helpers. Unexpected source
origins, ambient ancestor Cargo configuration or incomplete caches fail closed.

After source and dependency review, use a fresh standard hosted Ubuntu 24.04
worker for the public repository's manual `main` context. Bind
`MORPHEUS_PACKAGE_SOURCE_REVISION` to the complete reviewed commit ID and use the
same checked-out source identity as package validation. Prepare the inputs on
that worker through separately reviewed acquisition, such as the existing
locked Cargo fetch. Never acquire or execute dependencies on the owner's PC.
The bundle builder itself reads preexisting inputs only.

Create a private JSON input plan (`0600`) with exactly these fields:

```json
{
  "schema": 1,
  "registry_cache": "/home/runner/.cargo/registry/cache/index.crates.io-example",
  "git_mirrors": {
    "git+https://github.com/element-hq/matrix-rust-rtc?tag=v0.5.0-rc.2#53880ef1ef66b62df555ce71ccd1d6c85b813596": "/home/runner/source-inputs/rtc.git",
    "git+https://github.com/jplatte/const_panic?rev=e0b317a9a7bde2d48a7d15b6a60d70e4a41d3b5f#e0b317a9a7bde2d48a7d15b6a60d70e4a41d3b5f": "/home/runner/source-inputs/const-panic.git",
    "git+https://github.com/matrix-org/matrix-rust-sdk?rev=c6ad52ecc09eaa36601f55effb0a48a533eb87dd#c6ad52ecc09eaa36601f55effb0a48a533eb87dd": "/home/runner/source-inputs/matrix-sdk.git"
  }
}
```

The cache directory name and mirror locations are examples, not download
endpoints. Supply actual canonical, owned, non-writable-by-other-users paths.
Use the **exact** Git source strings in the selected revision's lockfile; every
Git source must have one complete local repository or bare mirror containing
its pinned commit. Every registry package must have its original
`name-version.crate` in the cache. Missing inputs, unknown source types,
unresolved workspace package identities, submodules and unsafe or dangling
source links stop preparation. No dependency may silently disappear.

Create an empty private output directory (`0700`) outside the checkout, then
invoke the existing packaging entry point explicitly:

```sh
python3 -I -B scripts/package.py --formats appimage --gitleaks /reviewed/gitleaks \
  --source-bundle-inputs /home/runner/source-inputs/plan.json \
  --source-bundle-output /home/runner/morpheus-source-export
```

The existing PID namespace, owned-process cleanup, log scanning and source
identity guards apply. Git reads only raw commit, tree and blob objects with
hooks, replacement objects, external diff/textconv and lazy fetching disabled;
global/system Git configuration and inherited `GIT_`/`SSH_` overrides are
discarded. No checkout occurs. Export attributes cannot omit or substitute
files. Internal Git symlinks are recorded and validated; registry links are
rejected. Inputs, paths, entry counts, expanded data, command output and command
lifetimes are bounded. The outer owner retains a bundle only after namespace
cleanup, log scanning and private temporary-tree deletion succeed.

`morpheus-corresponding-source.tar.gz` contains the complete committed workspace
archive, each authenticated Git tree archive, the original checksummed registry
archives, `MANIFEST.json` and `BUILDING.txt`. The manifest binds the source commit
and tree, every workspace/Git file's mode and hashes, lockfile hash, exact
dependency archive identities, package manifest locations, declared licenses
and candidate license-text hashes. It includes all locked packages, including
platform-specific entries. Published crate development metadata may name files
excluded from publication; the published root identity and complete original
archive are retained. This does not attest that unpublished development inputs
are available. Paths to local caches and Git configuration are not exported.

The receipt and manifest always state `distribution_ready=false`. The manifest
also states `license_compliance_verified=false`. SPDX declarations and candidate notices
do not establish compliance. Native/prebuilt build inputs, system packages,
compiler/runtime sources and exceptions, generated preferred source forms,
relinking/install information, full license obligations and source-content
review/scanning remain explicit unresolved requirements. The archive is
qualified Rust/workspace source evidence, not an offline rebuild guarantee or
approval to distribute. This mode neither uploads the bundle nor authorizes
installer publication, signing or updates.

## Unsigned package validation

The Windows job prepares [verified NSIS tools](../packaging/windows/NSIS_TOOLS.md)
before running Cargo Packager. All three downloads have exact size and SHA-256
pins; preparation refuses an existing tool cache. Its pure/mocked checks and
in-memory validation of the pinned inputs pass. At `a38fe43`, actual Windows
preparation, silent installation, owned-window startup and uninstallation
passed in [run 38743493394](https://github.com/brk275/Morpheus/actions/runs/38743493394).
This unsigned smoke does not establish signing, Narrator, account or update
acceptance.

The manual [`package-qa.yml`](../.github/workflows/package-qa.yml) workflow runs
only on public `main` using standard hosted runners. It requires an explicit
platform choice (`linux`, `windows`, `macos`, or `all`), defaulting to Linux.
Shared release candidates require `all` and `linux_formats=both`; a Linux-only
packaging edit selects only Linux. The additional `linux_formats` choice
(`both`, `appimage`, or `deb`) permits a focused check of one Linux format.
Results name the formats actually tested; a partial run cannot satisfy the
complete Linux release gate. It pins Rust 1.99.0 and Cargo Packager 0.11.8. To dispatch an
appropriate check after confirming no equivalent run is active:

```sh
gh workflow run package-qa.yml --ref main -f platform=linux -f linux_formats=appimage
```

Debian validation is currently deferred by the owner pending a working project
maintainer email. Use AppImage for current Linux checks; this leaves the Debian
release gate open.

The workflow builds DMGs on both native Mac architectures, NSIS on Windows,
and DEB/AppImage on Ubuntu 24.04. It installs, inspects, briefly launches, and
removes these packages using [`scripts/package_smoke.py`](../scripts/package_smoke.py).
Installers stay on the ephemeral runner: there is no artifact upload or release publication. Public
source and logs do not imply tested, signed installers. Native CI and packaging
share a candidate/platform lock. Their workflow-level groups are separate;
check active and completed runs before dispatching equivalent work.
All hosted jobs are gated off while the repository is private; paid runs require
separate owner authorization.

For a local package, use `python3 scripts/package.py --formats deb,appimage`,
`--formats dmg`, or `--formats nsis` on the relevant platform. Run
`python3 scripts/package.py --check` to validate configuration without building.
Direct `cargo packager` calls bypass the Linux overlay and are not the supported
packaging entry point.

### Automated installed-package checks

The installation helper runs only when `GITHUB_ACTIONS=true`; it is intended
for disposable hosted runners, never a developer's existing application
installation. It uses the fresh runner/container application profile and does
not sign in or operate media devices. The debug-only `MORPHEUS_QA_PROFILE_DIR`
override is deliberately not used for release packages.

| Target | Automated checks |
| --- | --- |
| macOS Apple Silicon and Intel | Verify and mount the DMG read-only without interaction; copy the app into a temporary Applications directory; check native Mach-O architecture, bundle identifier/version, microphone/camera/screen purpose strings, exact license and theme contents; compile the Swift accessibility bridge and prove its read-only permission state; create the one synthetic release profile described below; launch the installed executable for 12 seconds and stop it; relaunch the same bundle executable for the bounded keyboard/accessibility check; remove the profile only after both launches verifiably stopped; remove the app, detach the DMG. |
| Windows x64 | Silently install NSIS into a temporary path containing spaces; check x64 PE32+ GUI subsystem, license/theme and Add/Remove Programs registration/version; create the one synthetic release profile described below; require the installed process to survive 12 seconds and create a Win32 window with its visible flag set; close/stop the process. Then relaunch the same installed executable for the bounded keyboard/UI Automation check, remove the profile only after both launches verifiably stopped, and run the real silent uninstaller and verify application directory and registration removal. |
| Linux x64 DEB | Resolve/install the DEB in a fresh Ubuntu 24.04 container with no compiler or library development packages; verify declared ABI floors and `libxi6`, direct shared libraries and dynamic loading of `libXi.so.6` **before** adding Xvfb; check ELF architecture/symbol floors, desktop entry/icon and license/theme; launch as an unprivileged user under X11/Xvfb for 12 seconds and require a visible window owned by that launch; stop the process and purge the package. |
| Linux x64 AppImage | In a separate fresh Ubuntu 24.04 container with a minimal X11 desktop runtime, inspect the extracted metadata/resources/ELF, then launch the **AppImage itself** as an unprivileged user using its extract-and-run runtime (no FUSE mount); require a visible window owned by that launch and 12-second survival. Only then install accessibility test tools and relaunch the same package for the bounded keyboard/AT-SPI checks below. Remove the portable file. No DEB is installed in this container. |

The AppImage's Ubuntu 24.04 runtime baseline includes the host ALSA library
(`libasound2t64`, providing `libasound.so.2`) as well as the X11/desktop libraries
listed in the smoke helper. ALSA remains host-provided for audio-device access,
following the [AppImage exclusion policy](https://github.com/AppImageCommunity/pkg2appimage/blob/master/excludelist#L118-L121).
The first launch runs before adding accessibility tools; those tools must not
supply a missing application runtime dependency implicitly. A passing launch
does not verify a microphone, speaker or other physical device.

The Linux runtime image is resolved once per job and used by immutable image ID
for both containers. Logs record that image ID and each installer's filename,
size and SHA-256, followed by machine-readable results. Multiple matching
installers are rejected to avoid testing a stale package accidentally. ELF
requirements above `GLIBC_2.39` or `GLIBCXX_3.4.30` fail the smoke until the
declared compatibility baseline is deliberately reviewed.

Linux window ownership requires `_NET_WM_PID` to identify a live process in the
launcher's still-owned process group and session, carrying its fresh nonce.
This accepts the AppImage runtime's child process and rejects an unrelated
window with the same title. Raw application logs are never printed on startup
failure; the keyboard driver's entire output is private except for validated
fixed phases and boolean results. No accessibility trees or screenshots are
published. A selector pass that could not read one node's child count (libatspi
reports an unanswered `ChildCount` call as `-1` without an error, and pyatspi
repeats that call for every index, where a `-1` would otherwise end the list
early), that received no object for a child index, or that skipped a node for a
transient IPC error is not evidence: the driver counts it and runs another pass
within the existing wait. A partial traversal can still reject two matches as
ambiguous; only a complete traversal can establish a unique label or keyboard
focus. Only libatspi's
exact `-1` sentinel is treated this way; any other invalid length still fails
the run.

On Linux startup failure, a bounded private-log reducer may report a fixed
loader-error category and a reviewed literal library name. It requires exit
127 plus a complete recognized loader diagnostic; exit 127 alone stays
unclassified. Unknown library names, paths, symbols, version strings and raw
log text are withheld. Oversized or ambiguous diagnostics cannot select a
specific missing library. The original failure and cleanup remain mandatory.

#### Installed Linux keyboard and accessibility API check

`scripts/linux_package_ui.py` runs only as the disposable container's unprivileged
user, under private DBus and Xvfb. It has a 180-second acceptance deadline and
bounded process-group cleanup inside a 240-second launcher deadline. The normal
package smoke installs `python3-pyatspi`, `python3-gi`, `at-spi2-core`,
`libglib2.0-bin` (for `gdbus`) and `xdotool`
**after** the minimal-runtime launch succeeds, so those tools cannot conceal a
missing dependency in that earlier check. No second native build is needed.

The driver uses fresh release XDG configuration/data/cache directories and no
account. It checks the sign-in field names and password accessibility role,
enters the synthetic preview through its accessible action, then:

- Anchors focus through AT-SPI and sends actual X11 XTEST key events for
  Return, Space, Tab, Shift+Tab and Escape. It checks focus state, account-menu
  dismissal, keyboard reachability of Manage accounts, modal Tab wrapping and
  focus returning to the account trigger after Escape.
- Opens Settings and increases font scale from 110% to 120% with a key; verifies
  the displayed value and saved TOML. An invalid external TOML edit must show an
  accessible settings error while the displayed scale stays at the last valid
  120%. Restoring the valid file must clear the error.
- Sends a normal `WM_DELETE_WINDOW` request, requires zero exit status and no
  remaining process carrying this launch's nonce, then reaps the anchored
  process group. Forced cleanup on failure never counts as normal exit.

This is focused API/keyboard coverage, **not Orca acceptance**, whole-app keyboard
reachability, contrast validation, notification delivery after quit, or real
account switching. Preview entry now uses the shared keyboard action component,
but this installed driver still enters through its accessible action and does
not claim keyboard-only entry.
The check does not cover the DEB's UI, Wayland, VoiceOver or Narrator. No native
media device or permission prompt is exercised.

#### Installed Windows keyboard and UI Automation check

`scripts/windows_package_ui.py` runs only on a fresh standard GitHub-hosted
`windows-2025` runner (`RUNNER_ENVIRONMENT=github-hosted`, `ImageOS=win25`),
after the plain visible-window smoke of the same installed executable passed
and before the uninstaller runs. It uses Windows built-ins only: Windows
PowerShell 5.1 with the `System.Windows.Automation` client assemblies and
`SendInput`, driven through [`scripts/windows_package_ui.ps1`](../scripts/windows_package_ui.ps1).
No tool is installed or downloaded.

The release executable resolves its profile through the shell known folders
(`RoamingAppData\brk275\Morpheus` for configuration and the account store,
`LocalAppData\brk275\Morpheus` for local data and cache), never through
`APPDATA`-style environment variables, and its debug-only profile override is
compiled out. Its first start creates the account store inside that profile, so
the smoke establishes one synthetic profile **before** the plain launch: it
resolves the same known folders with declared native signatures and bounded
string reads, refuses to continue if either `brk275` organization folder,
Morpheus root, or a reparse point at those paths already exists, creates both
roots with a marker file carrying the run nonce, and seeds only
`config\config.toml` with a 110% font scale. The plain launch and the keyboard
relaunch share that profile; the driver only proves the markers and refuses
anything else. The smoke removes the roots only after the plain launch stopped
and the driver reported that its job is empty; otherwise the synthetic profile
is retained on the disposable runner and the smoke fails. Nothing ever reads,
redirects or deletes existing profile or vault material, and no registry or
shell setting changes. No account is used and no credential is entered.

The app and the PowerShell bridge run inside one kernel job object (each
assigned before its first instruction, kill-on-close) so the bridge's
compiler and every other descendant are accounted for, including after a
failed start or handshake. After one owned visible window appears, the bridge
takes bounded raw-view snapshots of that window's UI Automation tree only: the
recorded-plus-queued element total is capped at 5000 before any sibling is
enqueued, names are cut at 256 characters before they are kept, the driver
reads reply lines through a 4 MiB bound before allocation and a bounded queue,
and a capped or overlong result fails instead of standing for a tree. A
snapshot in which any node could not be read or stopped belonging to the owned
process is incomplete and is retried within the existing wait; a partial pass
can still reject two matches as ambiguous but can never establish a label or
focus. Then:

- Checks that `Homeserver URL` and `Matrix ID or username` are edit controls
  and that `Password` is an edit control with the UI Automation password flag.
  No value, text or value pattern is ever requested from any field.
- Focuses the preview action through UI Automation and presses Return as a
  real key, entering the fictional offline preview without signing in.
- Repeats the Linux account-menu sequence with real Return, Space, Tab,
  Shift+Tab and Escape keys: menu open, dismissal with focus return, keyboard
  reachability of Manage accounts through menu choices only, modal Tab order
  and Escape back to the trigger. Before every key the focused element must
  belong to the owned process and the bridge must observe the exact owned
  window handle and process as the foreground window immediately before its
  single native `SendInput` call, which inserts the complete chord (Shift,
  key down, key up, Shift) serially so no other target can intervene. One
  `SetForegroundWindow` recovery is attempted and counted, otherwise the run
  fails rather than typing toward another window. A chord the system accepted
  only partly fails the run; its held keys are released only toward the
  still-owned window.
- Opens Settings and reaches `Increase font scale` with bounded real Tab
  presses (160 maximum), checking the owned focused name after each one.
  Settings rows below the fold are clipped out of the accessibility tree until
  focus reveals them, so the driver never waits for the distant scale label
  first. Once focused, it checks the `110%` row label, presses Space, checks
  `120%`, and reads the saved TOML for `scale_percent = 120`.
- Posts a normal `WM_CLOSE` to the owned window after reverifying its owner,
  requires exit status zero within 15 seconds and zero active processes in the
  job, then releases the job. Forced job termination on failure never counts
  as a normal close, and the profile is removed by the smoke, not the driver.

The whole driver and bridge stream stays private; only the fixed JSON record
with reviewed phase tokens, booleans and bounded counts (`selector_matches`,
`incomplete_passes`, `foreground_recoveries`, `bounded_snapshots`) is printed
and validated by the smoke before it is accepted. It also carries the bounded
`window_inventory` of the poll that decided window selection. That inventory
holds closed class families, complete numeric style words, visibility, owner,
cloaking and clamped size, as described in
[the window probe notes](WINDOWS_WINDOW_PROBE.md#installed-window-inventory).
Names, titles, values, handles, process identifiers, positions, paths and raw
PowerShell or application output are never published. It has a 180-second deadline inside the smoke's 360-second launcher
bound.

This is focused UI Automation/keyboard coverage, **not Narrator acceptance**,
whole-app keyboard reachability, live account or credential-vault behavior,
the installed-settings file watcher (the invalid-TOML edit check exists only
on Linux), signing or installer UI. It requires the hosted runner's
interactive desktop; a session without one fails closed at the foreground
check instead of sending input elsewhere.

#### Installed macOS keyboard and accessibility API check

`scripts/macos_package_ui.py` runs only on fresh standard GitHub-hosted
`macos-15` and `macos-15-intel` runners (`RUNNER_ENVIRONMENT=github-hosted`,
`ImageOS=macos15`), after the plain 12-second launch of the same installed
bundle executable passed. It uses system frameworks only: the smoke compiles
[`scripts/macos_package_ui.swift`](../scripts/macos_package_ui.swift) with the
runner's Swift toolchain (`xcrun swiftc`) before any application starts, and
the resulting bridge links AppKit, ApplicationServices and CoreGraphics. No
tool is installed or downloaded, and the binary stays on the ephemeral runner.

**Permission.** The bridge reports its own `AXIsProcessTrusted()` and
`CGPreflightPostEventAccess()` state; both calls are read-only and never
prompt. The smoke proves both are true before the profile or the application
exists, and the driver proves them again at every real run. A false value
fails closed with `permission_unavailable`; nothing requests, grants or
changes a permission, and TCC is never touched. GitHub's hosted macOS image
pre-authorizes its runner process chain for Accessibility and event posting;
the check depends on that image state and reports it as a fixed token, so an
image change fails the run instead of silently degrading it.

**Profile.** The release executable (directories 6 / dirs-sys) resolves its
profile from `HOME`, then the passwd entry, to `~/Library/Application
Support/io.github.brk275.Morpheus` (configuration file and account store at
its root) and `~/Library/Caches/io.github.brk275.Morpheus`. The smoke
requires `HOME` to be the runner account's real home, never redirects it,
requires every folder from the home down to each Library base (`~`,
`~/Library`, `~/Library/Application Support`, `~/Library/Caches`) to be a
plain directory rather than a symbolic link before any create, open or
remove, refuses to continue if either project folder exists or is a symbolic
link, creates both folders with a marker file carrying the run nonce, and
seeds only `config.toml` with a 110% font scale. Both launches share that
profile; the driver only proves the markers. The smoke removes the two
folders only after the plain launch stopped and the driver reported a
verified stop; otherwise the synthetic profile is retained and the smoke
fails. The Library folders themselves are never created or removed, and a
redirected ancestor stops the removal as well, so foreign material behind a
link is never deleted. The ancestor check is lexical: a `..` component in a
profile path is refused rather than resolved. Folders above the home are not
checked.

**Containment.** macOS has no job object. The application is started with its
own session and process group; the leader stays an unreaped child (`waitid`
with `WNOWAIT`) until every cleanup signal and the final accessibility action
have happened, so its PID and group ID cannot be recycled under the check.
Ownership is proven from three independent reads of the PID: group and
non-zombie state from the kernel process table (`ps -axo pid=,pgid=,stat=`),
the session from `getsid`, and `proc_pidpath` against the installed bundle
executable; the bridge additionally checks the running application's bundle
identifier and executable URL before attaching.

The attach step works as follows:

- **Path comparison.** The bridge resolves the executable URL with `realpath`
  and compares it with the driver's fully resolved path, which is the form
  `proc_pidpath` reports. Foundation's URL standardization is not used: it
  strips a leading `/private` (documented for `NSURL`), so a bundle installed
  under `/var` or `/tmp` would never compare equal.
- **Registration wait.** `proc_pidpath` can show the installed executable as
  soon as `exec` completes, before AppKit has registered the process as an
  application. So the driver asks again only while the bridge reports absent
  metadata: no running application, no bundle identifier or no executable
  URL.
- **Budget.** Registration has a 10-second monotonic budget:
  - no attempt starts once the budget has passed, and a zero budget makes
    none;
  - each bridge reply wait ends at the budget, as well as at the 20-second
    reply bound and the acceptance deadline;
  - the 0.2-second pause between attempts is clipped to what remains;
  - a success counts only if the budget has not passed, both when the reply
    arrives and after the ownership recheck that follows it.

  Other bridge commands keep their existing bounds.
- **Ownership rechecks.** Before and after every attempt, including failed or
  timed-out ones, the driver rechecks that the anchored process is alive,
  unreaped and still owned. A lost anchor takes precedence over other
  failures. These rechecks are OS queries with their own bounds (the
  process table read times out after 5 seconds). Elapsed time can therefore
  exceed the budget by their duration, but a late success is never accepted.
- **Run-loop turn.** Before each lookup the bridge turns its otherwise idle
  main run loop once, for at most 0.05 seconds, in the default mode.
- **Failure.** A present identifier or executable that differs, or an
  executable that cannot be resolved, fails at once. The bridge sets its
  target only after every check passes.
  - A budget that runs out between attempts fails as `wait_timeout` with the
    last absent-metadata token.
  - A reply wait cut by the budget, or a late success, fails as
    `wait_timeout` with no bridge token.
  - Transient answers are not recorded, so a passing report is unchanged.

Darwin's `ps` prints the
session structure's kernel address under `sess` (Apple adv_cmds
`ps/keyword.c`), never the session ID, so that column is not read. Every
group the smoke starts on macOS (the plain 12-second launch, the bridge and
the keyboard relaunch) stops through the same proof: the group is signalled
with TERM and then KILL while the leader is still an unreaped child, the
process table is read while that PID still names the group, and the leader is
reaped only once no live member remains. A member alive at the bound, or a
table that cannot be read or trusted, is the fixed failure
`process_remaining`, the leader stays anchored, cleanup is reported as failed
and the smoke retains the profile; when the plain launch fails this way, the
keyboard relaunch never starts. A trustworthy table is a complete
`pid pgid stat` snapshot with at least one row: blank lines are ignored, but
any other malformed row, an old four-column snapshot or an empty one is
rejected for ownership and cleanup alike, so an unparsed process is never
mistaken for an absent one. A descendant that moved itself to another group
is outside this proof. Nothing is launched through `open` or LaunchServices.

**Flow.** After one on-screen window owned by the process exists, the bridge
takes bounded snapshots of that process's accessibility tree only (recorded
plus queued elements capped at 5000 before any child is enqueued, names cut
at 256 characters, reply lines bounded at 4 MiB before allocation through a
bounded queue). Names come from `AXTitle`; only an `AXStaticText` without a
title may fall back to `AXValue`, and the driver rejects any value-sourced
name on another role. Editable and protected fields are never asked for a
value. A snapshot with an unreadable or unowned node is incomplete and is
retried; a partial pass can reject two matches as ambiguous but never
establishes a label or focus. Then:

For `AXChildren`, an explicit `AXError.noValue` with a nil payload is a
declared empty collection, as used by native AppKit leaf buttons. It is not
an unreadable subtree. Unsupported attributes, messaging errors, invalid
elements, malformed success payloads and contradictory non-nil `noValue`
payloads remain incomplete. Successful arrays also need an
explicit CF array type, a count within the retained-node bound and an AX
element type check for every child before conversion. Swift's conditional
cast alone does not validate those CF element types. These changes require
native classifier and owned-tree checks on both Mac architectures, followed by installed-app
verification; a synthetic AppKit subject does not prove Morpheus's full tree.

- `Homeserver URL` and `Matrix ID or username` must be `AXTextField` without
  the secure subrole; `Password` must be `AXTextField` with
  `AXSecureTextField`.
- Focus moves through `AXFocused` on the owned element, and Return enters the
  fictional offline preview without signing in.
- The account-menu sequence of the Linux and Windows checks runs with real
  Return, Space, Tab, Shift+Tab and Escape transitions. Each transition is a
  `CGEvent` posted directly to the owned process (`postToPid`), never to the
  session, and the owned process must be the frontmost application
  immediately before every post, including releases; one activation recovery
  is attempted and counted, and a failed proof ends the chord with nothing
  further posted.
- Settings opens and `Increase font scale` is reached with bounded real Tab
  presses (160 maximum); only then are the `110%` label, Space and the `120%`
  label checked, followed by the saved TOML.
- The window's own close button is pressed through its accessibility action
  after an ownership recheck. The run requires observable exit status zero
  within 15 seconds and no live member left in the leader's group; a merely
  terminated or not-running state never substitutes for status zero.

The whole driver, bridge and compiler stream stays private; only the fixed
record `installed_macos_keyboard_ax` with reviewed tokens, booleans and bounded
counts is printed and validated by the smoke. Names, values, paths, PIDs,
accessibility trees and raw output are never published.

This is accessibility API/keyboard coverage, **not VoiceOver acceptance**,
whole-app keyboard reachability, live account or keychain behavior, the
installed-settings file watcher (Linux only), signing, notarization or
Gatekeeper behavior of a signed release.

After applicable native jobs and cache posts finish, review their results and
run only the affected ordinary package check with `platform=linux`,
`linux_formats=appimage`, `appimage_updates=false`, `appimage_capture=false`.
An independent package check may diagnose installation while a separate native
gate remains failed; it cannot replace or clear that failure.
The helper and its Python tests do not trigger an automatic native matrix;
installed acceptance requires that explicit Linux package run. Debian `both`
remains blocked until its working public maintainer contact is configured.
The owner explicitly deferred Debian packaging on 2026-10-11. Manual QA now
defaults to AppImage; do not invent an address or retry DEB builds while that
contact is missing. Debian's release checks remain open.

These checks are implemented automation, not evidence that a particular
candidate passed. A successful run establishes only the checks listed above
for its exact source and package hashes. macOS startup is process liveness, with the separate installed accessibility check above;
the Windows visible-window flag, UI Automation tree and virtual X11 window do not establish visual
quality or complete accessibility. Signing/notarization, installer UI, full desktop
integration, real account/credential-vault behavior, native media permissions,
Wayland, hardware, updates and rollback remain release acceptance gates.
The helper rejects a missing Debian `Maintainer` field. The packaging wrapper
also checks this metadata before compiling a DEB; a working public project
contact remains required. Other package formats can be checked independently.

Earlier private QA runs uploaded unsigned installers for five days. On
2026-10-10, their three runs and all ten artifacts were removed before source
publication. A private manifest retains producing revisions and immutable
artifact identities. Public
repositories expose Actions history and logs. Existing historical evidence
below describes older inputs and does not verify changed source or packages.

The first private QA DMG displayed the entire AGPL as a mount agreement because `license-file = "LICENSE"` was set. The packaging config now bundles `LICENSE` as a readable resource instead; the Debian package also puts it in `/usr/share/doc/morpheus/copyright`. This removes the DMG mount agreement and the NSIS license page. Later extracted Apple Silicon, Windows, and Linux artifacts contain the license; installed-package behavior still needs checking on every platform. Provide recipients the Corresponding Source separately as required by the AGPL.

The Linux QA executable built on Ubuntu 24.04 required `GLIBC_2.39` and `GLIBCXX_3.4.30`. The Debian `Depends` list declares matching libc and libstdc++ floors plus the ELF's other direct libraries, desktop rendering, X11/Wayland input, and PipeWire for call capture. Ubuntu 22.04 package compilation was attempted, but its PipeWire 0.3.48 headers do not compile with xcap's `libspa 0.10.1` dependency. Support for older Linux distributions needs a separately tested screen capture dependency or toolchain strategy. Until then, treat Ubuntu 24.04 as the build and installation baseline. Verify the generated Debian control file, inspect the ELF symbol versions, and install and launch both Linux formats on a clean Ubuntu 24.04 desktop before distributing them.

### Private artifact evidence, 2026-10-10

- The first Apple Silicon DMG (run `38026272398`, commit `43d5e77`) passed `hdiutil verify`, but mounting showed the full AGPL agreement and its app bundle lacked a LICENSE resource. The app launched to its native sign-in screen from a temporary install under an isolated home directory.
- The first Intel macOS DMG from the same run also passed `hdiutil verify`. Its bundle contains an x86-64 executable and all three media permission strings, but no LICENSE resource; `codesign` reports that it is unsigned. It was not launched on Intel hardware.
- The first Windows NSIS installer from the same run is a valid NSIS-3/LZMA archive. Its payload contains an x86-64 `morpheus-app.exe` and `aurora.toml`, but no bundled LICENSE. Neither the installer stub nor the executable has an Authenticode certificate. The extracted executable uses the PE console subsystem, so the next Windows package must be checked for a GUI subsystem and a launch without a console window. It has not been installed or launched on Windows.
- The corrected Apple Silicon DMG (run `38029020876`, commit `8de16d9`) passed `hdiutil verify` and mounted noninteractively without an agreement. Its app bundle contains a LICENSE byte-identical to the repository file, the expected `io.github.brk275.morpheus` metadata and permission strings, and an arm64 executable. A temporary install opened the native sign-in screen under an isolated home directory. It remains ad hoc signed with no Developer ID or notarization.
- The Ubuntu 22.04 job in run `38029020876` installed its apt dependencies but failed compiling `libspa 0.10.1` against PipeWire 0.3.48 headers; it produced no Linux package. Run `38029335100` returned Linux packaging to Ubuntu 24.04.
- The Ubuntu 24.04 job in run `38029335100` produced valid x86-64 DEB and AppImage artifacts. The DEB control contains the declared `Depends`; both formats include LICENSE, and the DEB also puts it in `/usr/share/doc/morpheus/copyright`. The ELF requires `GLIBC_2.39` and `GLIBCXX_3.4.30`, matching the package floors. A clean Ubuntu 24.04 x64 container installed the DEB, but Xvfb launch exposed a missing `libxi6` runtime dependency. After adding `libxi6` manually, the app stayed alive under Slint software rendering for ten seconds; the `Depends` list now includes it for the next package. The extracted AppImage payload likewise launched under Xvfb. Direct AppImage runtime execution was blocked by the Apple Silicon host's x86-64 container emulation, so test it on native Linux x64.
- The Windows x64 job in run `38029335100` produced a 35,087,286-byte NSIS installer. Extracting it showed the app, uninstaller, theme, and AGPL license; the license matches the source after Windows line-ending normalization. The installer stub is a GUI executable, but the packaged app still uses the PE console subsystem at this older commit. Current source adds the Windows GUI subsystem attribute, which needs a fresh package and Windows install/launch/uninstall acceptance.
- The macOS ARM job in run `38029335100` never obtained a runner. GitHub annotated it as blocked by an Actions budget, with a separate macOS ARM capacity notice. This is not a package build result. Check the account's Actions budget before another private run.

Earlier private QA generated a DEB without a `Maintainer` control field. Actual Debian builds now fail preflight until a project maintainer identity and working public project email are configured in top-level `authors`. A personal identity is not required. `package.py --check` validates configuration parsing only; it does not pass this build preflight or establish package readiness.

## macOS permissions and signing

[`packaging/macos/Info.plist`](../packaging/macos/Info.plist) supplies purpose strings for microphone, camera, and screen capture. Cargo Packager merges these entries with its generated app metadata. [`packaging/macos/entitlements.plist`](../packaging/macos/entitlements.plist) grants audio input and camera access when a hardened-runtime app is signed. Screen recording also requires the user's macOS permission.

For a release candidate, sign with a Developer ID Application identity, notarize, then inspect the actual bundle:

```sh
plutil -p dist/Morpheus.app/Contents/Info.plist
codesign --verify --deep --strict --verbose=2 dist/Morpheus.app
codesign --display --entitlements - dist/Morpheus.app
spctl --assess --type execute --verbose dist/Morpheus.app
```

Signing identity and notarization credentials belong in the release environment, not this repository. Run microphone, camera, and screen capture permission flows on the **signed package**; unsigned developer runs do not validate those flows.

## Updates and controlled betas

The app now has a signed-update discovery and staging service using the pinned
official `cargo-packager-updater` 0.2.3 with a documented security patch. It
requires compiled HTTPS feed and public-key configuration, bounds responses,
refuses redirects, checks the package format and version, and reverifies the
exact staged bytes. Settings now has explicit Check, Download, Discard and
Restart controls, disabled when the compiled feed/key or supported installation
is absent. No public feed is enabled. The controller runs blocking work in the
background, rejects stale results and drains work before normal shutdown.
Verified replacement, interrupted-update recovery, rollback, and
call-controller behavior remain release gates; the original upstream installers cannot run
through the patched public APIs. Linux now has a durable AppImage transaction
and startup recovery foundation, with shared call/update exclusion. See
[AppImage updates](APPIMAGE_UPDATES.md) for its signed compatibility footer,
pre-store commit boundary, packaging order and remaining acceptance checks.
Recovery-key acknowledgement and the shared call lease block unsafe restarts.
The new UI/controller integration still needs hosted and installed-package
verification. macOS and Windows installation remains disabled pending their
platform transactions and verification.

Cargo Packager's default formats produce a macOS `.app` and `.dmg`, a Windows
NSIS installer, and Linux `.deb`, AppImage, and Pacman packages. An update for
macOS must be an `.app` bundle even when originally installed from a DMG; a DMG
is not an updater payload. Debian and Pacman installations use their package
managers. Linux self-update is limited to a writable AppImage; Windows requires
an explicitly marked NSIS build. Do not enable installation until the recovery
and installed-package checks pass.

An updater release needs an HTTPS feed returning `204 No Content` or a `200 OK` JSON response with a newer semantic version, artifact URL, exact artifact signature, and supported format. The app verifies that signature with an embedded public key; the signing private key stays in the release environment. This signature is separate from macOS notarization and Windows code signing. Test the actual installed upgrade, bad signature, interrupted download, failed install, and recovery path on each supported platform before enabling it.

This source repository is public; its releases and Actions artifacts cannot provide private beta access. Do not embed a repository token in the app. Any future restricted beta needs a separately controlled distribution service with per-user authorization for both its feed and assets. Keep automatic updates disabled until access controls and the signing flow are verified. Public installer releases must pass the release checklist.

## Platform acceptance

Build and install on macOS Apple Silicon and Intel, Windows x64, and Linux x64 under both Wayland and X11. Include an Ubuntu 24.04 Linux desktop to check the current compatibility baseline. Check install, launch, update, uninstall, credential storage, and native call devices on each system. The signed release pipeline and installed update acceptance remain incomplete. The owner confirmed on 2026-10-10 that neither Apple Developer membership nor Windows code signing is configured; unsigned QA does not satisfy these gates.

For Linux attachment open/save dialogs, the app builds `rfd` with its XDG portal backend. Runtime testing needs a working `xdg-desktop-portal` service and a desktop-specific portal implementation; `rfd` can fall back to `zenity` when the portal request fails. Test attachment pickers under both Wayland and X11 with the packaged app. The packaging runner's successful compile does not prove that a target desktop provides either dialog path.

Cargo Packager configuration and updater: <https://docs.crabnebula.dev/packager/configuration/>, <https://docs.crabnebula.dev/packager/updater/>. Apple permission and hardened-runtime requirements: <https://developer.apple.com/documentation/xcode/configuring-the-hardened-runtime>, <https://developer.apple.com/documentation/screencapturekit>.

### Scrollable installed appearance checks

The Linux installed keyboard driver reaches the font-scale action with at most
160 real Tab keys, rechecking the owned foreground window for each key. It
requires a complete unambiguous selector at the target before activation.
Waiting for a distant label before scrolling is insufficient: the pinned
Slint/AccessKit stack omits clipped items from AT-SPI. Fixed appearance
checkpoints distinguish reaching the control, initial/saved scale, invalid-edit
feedback, retained scale and restoration; arbitrary labels and raw errors are
not public diagnostics. Success still requires persistence, invalid-edit
retention, normal close and cleanup, in addition to the earlier sign-in and
account-menu checks. This does not establish screen-reader acceptance.

The macOS and Windows drivers retain a separate, closed `appearance_stage`
through lower-level selector operations and cleanup. It identifies opening
Settings, reaching the scale control, reading the seeded value, increasing it,
reading the changed value and persisting it. Only successful persistence marks
this stage complete; the overall installed check still requires normal close
and cleanup. Unknown stage strings are rejected rather than included in reports.

The macOS account-menu check also identifies its fixed keyboard boundary:
focusing the trigger, opening with Return, closing with Escape, reopening with
Space, tabbing to Manage accounts, opening that dialog, its Tab/Shift-Tab order,
and closing it. These tokens contain no account labels or focused control text.
A timeout at one boundary remains a failed installed check; these diagnostics
do not substitute for keyboard or screen-reader acceptance.

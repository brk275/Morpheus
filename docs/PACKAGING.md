# Packaging Morpheus

Shared package configuration is in [`Packager.toml`](../Packager.toml); Debian and other Linux settings live in [`packaging/linux.toml`](../packaging/linux.toml). The wrapper merges only the current platform's settings. Its build hook compiles `morpheus-app` in release mode with `native-calls` and the lockfile. Read [AGENTS.md](../AGENTS.md) before building; select the affected platform rather than starting a full packaging matrix.

```sh
cargo install cargo-packager --version 0.11.8 --locked
python3 scripts/package.py
```

The package output goes to `dist/`. This command does not make a public release. The packaged call feature still needs real MatrixRTC interoperability and device testing before a beta can be distributed.

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
| macOS Apple Silicon and Intel | Verify and mount the DMG read-only without interaction; copy the app into a temporary Applications directory; check native Mach-O architecture, bundle identifier/version, microphone/camera/screen purpose strings, exact license and theme contents; launch the installed executable for 12 seconds, stop it, remove the app, detach the DMG. |
| Windows x64 | Silently install NSIS into a temporary path containing spaces; check x64 PE32+ GUI subsystem, license/theme and Add/Remove Programs registration/version; require the installed process to survive 12 seconds and create a Win32 window with its visible flag set; close/stop the process, run the real silent uninstaller and verify application directory and registration removal. |
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
for its exact source and package hashes. macOS startup is process liveness;
the Windows visible-window flag and virtual X11 window do not establish visual
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

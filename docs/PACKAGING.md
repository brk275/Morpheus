# Packaging Morpheus

The package configuration is in [`Packager.toml`](../Packager.toml). Its build hook compiles `morpheus-app` in release mode with the `native-calls` feature and uses the lockfile. Run packaging on each target operating system:

```sh
cargo install cargo-packager --version 0.11.8 --locked
cargo packager --release
```

The package output goes to `dist/`. This command does not make a public release. The packaged call feature still needs real MatrixRTC interoperability and device testing before a beta can be distributed.

## Unsigned internal package QA

The manual [`package-qa.yml`](../.github/workflows/package-qa.yml) workflow packages the private repository's `main` branch on macOS Apple Silicon and Intel, Windows x64, and Linux x64. It pins Rust 1.99.0 and Cargo Packager 0.11.8, using `Packager.toml`'s release build hook. It produces a DMG on each Mac runner, an NSIS installer on Windows, and both a Debian package and AppImage on Linux. Run it from the GitHub Actions tab or with `gh workflow run package-qa.yml --ref main` after the workflow lands on `main`.

The workflow only runs while the repository is private. It has a read-only token, passes no signing or notarization credentials, and only uploads Actions artifacts named `morpheus-UNSIGNED-INTERNAL-QA-...`. They expire after five days. [GitHub requires repository read access to download artifacts](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts). Delete these runs and artifacts before changing the repository's visibility, since public repositories expose Actions history and logs. These packages are for install smoke tests and preliminary device permission checks by authorized collaborators; they are not signed, notarized, updater payloads, or release candidates. On Linux, set the downloaded AppImage executable bit with `chmod +x` before running it, because Actions artifact upload does not preserve file permissions.

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

## Updates and private betas

The updater is not integrated yet. Cargo Packager's default formats currently produce a macOS `.app` and `.dmg`, a Windows NSIS installer, and Linux `.deb`, AppImage, and Pacman packages. `cargo-packager-updater` supports installing signed updates into a macOS `.app`, a Windows NSIS installer (or WiX if packaged), and a Linux AppImage. The macOS updater needs an `.app` update bundle even when the installed app originally came from a DMG; a DMG is not an updater payload. Debian and Pacman installations should use their package managers. On Linux, enable self-update only when running from a writable AppImage.

An updater release needs an HTTPS feed returning `204 No Content` or a `200 OK` JSON response with a newer semantic version, artifact URL, exact artifact signature, and supported format. The app verifies that signature with an embedded public key; the signing private key stays in the release environment. This signature is separate from macOS notarization and Windows code signing. Test the actual installed upgrade, bad signature, interrupted download, failed install, and recovery path on each supported platform before enabling it.

Assets in this private GitHub repository require authorized access. Do not embed a repository token in the app. Distribute pre-release packages manually to authorized testers through restricted GitHub releases or Actions artifacts, or provide a private update service with per-user authorization for both the feed and downloads. Keep automatic updates disabled until that service and the signing flow are verified.

## Platform acceptance

Build and install on macOS Apple Silicon and Intel, Windows x64, and Linux x64 under both Wayland and X11. Check install, launch, update, uninstall, credential storage, and native call devices on each system. The updater and signed release pipeline have not been implemented or validated yet.

Cargo Packager configuration and updater: <https://docs.crabnebula.dev/packager/configuration/>, <https://docs.crabnebula.dev/packager/updater/>. Apple permission and hardened-runtime requirements: <https://developer.apple.com/documentation/xcode/configuring-the-hardened-runtime>, <https://developer.apple.com/documentation/screencapturekit>.

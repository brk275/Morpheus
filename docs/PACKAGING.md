# Packaging Morpheus

The package configuration is in [`Packager.toml`](../Packager.toml). Its build hook compiles `morpheus-app` in release mode with the `native-calls` feature and uses the lockfile. Run packaging on each target operating system:

```sh
cargo install cargo-packager --locked
cargo packager --release
```

The package output goes to `dist/`. This command does not make a public release. The packaged call feature still needs real MatrixRTC interoperability and device testing before a beta can be distributed.

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

## Platform acceptance

Build and install on macOS Apple Silicon and Intel, Windows x64, and Linux x64 under both Wayland and X11. Check install, launch, update, uninstall, credential storage, and native call devices on each system. The updater and signed release pipeline have not been implemented or validated yet.

Cargo Packager configuration: <https://docs.crabnebula.dev/packager/configuration/>. Apple permission and hardened-runtime requirements: <https://developer.apple.com/documentation/xcode/configuring-the-hardened-runtime>, <https://developer.apple.com/documentation/screencapturekit>.

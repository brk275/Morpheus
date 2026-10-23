# Installed AppImage update acceptance

`scripts/appimage_update_acceptance.py` is an explicitly selected Linux CI
harness. **Its first hosted result is pending.** Adding it or passing its
mocked Python tests is not installed-update acceptance. Ordinary builds still
have no update feed or verification key compiled in.

## Invocation and build identity

The optional AppImage update mode in the existing package QA workflow supplies:

```sh
MORPHEUS_FREE_HOSTED_UPDATE_QA=1 /usr/bin/python3 scripts/appimage_update_acceptance.py \
  --source "$GITHUB_WORKSPACE" --target-dir "$GITHUB_WORKSPACE/target" \
  --gitleaks /absolute/path/to/gitleaks --execute-on-free-hosted-runner
```

It refuses other operating systems, root execution, self-hosted runners,
missing explicit opt-in, or missing GitHub Linux x64 runner identity. The
workflow selects a free standard public-repository runner; this helper cannot
independently audit GitHub billing. It starts no separate hosted build.

The helper exports the checked-out `GITHUB_SHA` into a private external
directory. Two genuine release AppImages use that same source path and target
cache. Only the app's explicit `[package] version` and its `Cargo.lock` package
version change (`-updateqa.1` → `-updateqa.2`); package version and output paths
are ephemeral Cargo Packager configuration overrides. Source, other lock
entries, native call features and persistence code remain identical. The
just-built binaries' supported metadata command must report identical
persistence fingerprints. Each final AppImage receives its compatibility
footer before official Cargo Packager **0.11.8** signing.

The footer producer and installed parser require exact integer protocol **2**.
Legacy protocol1 packages require manual migration; an older helper is not
accepted merely because its persistence cohort matches. Forward application
also requires the final successful ordinary-app-scope authority, its consumed
socket EOF, actual original thread-group pidfd death and a fresh lock/journal
recheck. Source regressions for those boundaries do not establish the ordinary
UI/runtime shutdown or older-kernel AppImage runtime route as accepted. Native
backend disposal and signed installed behavior remain separate open gates.

The public verification key and loopback HTTPS feed are compiled into these
disposable builds using the existing production build configuration. They are
the same in both builds. No runtime key/feed override, alternate signature
format, application testing API, or TLS verification bypass is introduced.

## Trust and UI path

A newly generated private signing key stays outside the checkout; the official
signer receives its private file path through its documented environment
variable, never command arguments. A second disposable key creates the
wrong-signature case. No official release key is accessed or claimed tested.

OpenSSL creates a one-day CA and a server certificate with the `127.0.0.1` IP
SAN. Only the CA's public certificate enters the disposable runner's normal
system trust store. The feed binds loopback only, serves fixed routes, and uses
normal HTTPS verification, including the updater's native trust roots. The
owned CA file is fingerprint-checked, removed, and the trust index refreshed
during cleanup. It is never installed on a developer machine.

The native app runs under Xvfb/X11 and a fresh D-Bus accessibility session.
AT-SPI invokes the normal interactive-preview, Settings, Check, Download,
Discard and Restart actions. It checks the live accessible status text and
button enabled state. After restart, a different native app process must own
the accessible window. No UI callback or updater function is called directly.

The current mode uses **`APPIMAGE_EXTRACT_AND_RUN=1`** for the genuine AppImage
runtime. It does not establish FUSE mounting or Wayland installation behavior.

## Separate cases required for a pass

| Case | Required evidence |
| --- | --- |
| Wrong-key signature | Normal HTTPS discovery succeeds; Download reports verification failure; Restart stays disabled; installed bytes and journal remain unchanged. |
| Truncated download | Server advertises the full content length and closes after a prefix; normal download fails; Restart stays disabled; installed bytes and journal remain unchanged. |
| Discard | A genuine signed download becomes ready; the normal Discard action removes its actionable state; installed bytes and journal remain unchanged. |
| Signed invalid ELF rollback | A separately signed negative fixture changes only ELF64 `e_phentsize` to zero, retaining valid signed metadata. Linux rejects execution, the actual supervisor restores the old bytes, and a different old-app process shows its UI with no remaining journal. |
| Genuine upgrade | The unchanged genuine new AppImage installs; a different app process displays its UI; installed hash and durable `Committed` journal match the exact new bytes/version and old hash. |

The rollback fixture deliberately exercises failure to execute. It is **not**
evidence for a genuine candidate's runtime crash, every pre-store interruption,
or every filesystem failure. The [Linux ELF loader's program-header size
check](https://github.com/torvalds/linux/blob/v6.8/fs/binfmt_elf.c) provides the
rejection independently of any Morpheus testing branch.

Each case uses a fresh private installation/profile and checks that an unrelated
data sentinel is unchanged. No Matrix account is signed in. This limited
sentinel assertion does not prove encrypted-store recovery or migrations.

## Containment, privacy and limits

The existing QA-only PID namespace wrapper drops privileges/capabilities before
repository code executes. Its real hosted group-kill regression is mandatory
before UI cases. Xvfb, D-Bus, the driver, AppImage runtime, update supervisor and
restarted app share that namespace; namespace exit stops even detached app
groups. Waitable process anchors are retained until outer group cleanup.

Keys, packages, profiles, exported source and raw logs live in an external 0700
temporary root. Keys and logs are 0600. Commands have deadlines, and exception
details/command arguments are never published. Logs are scanned before fixed
success booleans are emitted. No installer, private key, profile, screenshot,
raw log or signing material is uploaded. On an externally forced job kill,
the GitHub-hosted runner's disposal remains the final resource boundary.

The successful public identity record contains only the source commit,
genuine old/new artifact hashes and versions, persistence cohort, signer
version and public-key fingerprint. The signed invalid ELF fixture has its own
explicitly separate hash. Private paths, feed URLs, signatures and keys are
excluded. The actual disposable installers are deleted after verification.

Passing this harness would cover these **disposable-key, extract-and-run X11**
cases only. Official signing-key custody, public-feed activation, real release
artifacts, FUSE/Wayland, power loss, full/read-only filesystems, moved installs,
live call/recovery exclusion, and post-update Matrix/encryption use remain
separate gates. macOS signing/notarization and Windows install/update acceptance
are unaffected.

Required runner tools include the pinned packager, Gitleaks, OpenSSL,
`update-ca-certificates`, Xvfb, D-Bus, AT-SPI and `/usr/bin/python3` with the
distribution's `pyatspi`/GI packages. The harness does not buy services,
provision certificates externally, or dispatch workflows.

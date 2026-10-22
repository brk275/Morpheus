# Linux native source builds

The fixed-client source-build proposal supports **Ubuntu 24.04 x86_64 GNU**.
Other Linux distributions, architectures and installed packages have not been
verified by this path. Ordinary Cargo checks without native calls/device-screen
remain available and do not prepare or load this client.

## Prepare explicitly

Install the existing native dependencies plus `meson`, `ninja-build`,
`pkg-config`, `libdbus-1-dev`, `libsystemd-dev`, and `systemd-dev`. Preparation
requires an unprivileged user and a new absolute directory outside the checkout.
The driver verifies the same Ubuntu source archives and exact upstream
`800a25a01f99156d7d954f4d7fe1b0a6cf8ae7e3` patch as the fixed-client AppImage
builder. It builds only the client library; it does not replace the daemon,
portal, SPA plugins or system installation. Source archives, applied source,
licenses and private bounded command logs remain under that prefix.

For an ordinary source build:

```sh
python3 scripts/build_linux_native.py --variant native \
  --prefix /absolute/new/private-prefix -- \
  build --locked -p morpheus-app --features native-calls
```

The optional Cargo command accepts `build`, `check` or `test` with `--locked`.
It uses a bounded owned process group; this developer convenience is not a
sandbox or hosted acceptance. Its command output is retained privately under
the prefix. Keep the prefix for the lifetime of its executables. A fresh build
may choose a new prefix; remove an old prefix only after its executables are no
longer needed. Do not copy just the binary into a package.

For several Cargo commands, prepare once without the trailing command, then
set both variables explicitly to that prefix:

```sh
export MORPHEUS_PIPEWIRE_CLIENT_VARIANT=native
export MORPHEUS_PIPEWIRE_CLIENT_RECEIPT=/absolute/new/private-prefix/stage/build-client.txt
```

Hosted callers use `--github-env` in an earlier step, with a run/attempt-specific
prefix under `RUNNER_TEMP`. Preparation uses the existing checked PID namespace
there. It runs at most four 125-second downloads and the existing 600-second
whole client build, within a separate bounded preparation command. Subsequent
native App builds/units, RTC tests and existing protocol fixtures inherit the
receipt. Their test, first-frame, capture and cleanup deadlines are unchanged.
The permanent CI prerequisite is Linux-only; its actions, permissions, cache
write guard, native matrix and all selected capture assertions are unchanged.

## Normal loader and identity

The native variant changes only the verified Meson library target name, giving
it `libmorpheus-pipewire-0.3.so.0` as its actual ELF SONAME. The normal linker alias
still serves `-lpipewire-0.3`. Cargo hooks verify the prepared receipt and bytes,
then add ordinary executable/test RUNPATH and link search to that owned prefix.
They run no preparer, compiler, downloader, loader or application themselves.
There is no `LD_PRELOAD`, global `LD_LIBRARY_PATH`, fake frame or test shim.

The stock system cannot satisfy the distinct native dependency if the prefix
is missing. This is not protection against an owner replacing its filesystem
or forging build inputs. Native preparation validates the ELF SONAME, build-ID,
NEEDED set and SHA before publishing the receipt. Source/build identity remains
separate from AppImage's deployed byte identity: deployment tools may rewrite
bytes while preserving the expected build-ID.

Both Cargo hooks track the receipt environment/path and library bytes. Every
hosted attempt prepares a new prefix, forcing restored build-script outputs
and the relevant final app/test links to refresh instead of retaining a vanished
previous runner path. Fresh hosted verification must inspect the actual app and
RTC test executable DT_NEEDED/RUNPATH, including an absent-prefix refusal.

System modules loaded from `client.conf` can themselves need the ordinary
PipeWire SONAME. The source alone does not prove that a stock library cannot
coexist through these transitive module loads. The acceptance still requires
all 33 actual collision addresses to resolve to the exact owned client with
matching hash/build-ID/mapped inode, followed by the full real capture scenario.
Stock PipeWire, WirePlumber and portal service mappings are checked separately.
A distinct filename alone is not acceptance evidence.

## Acceptance and packaging boundaries

The ordinary Wayland gate now truthfully identifies the owned fixed native
client and stock services. All first static-frame, second-display, changing
pixels, reopen, source-loss, stop, denial and cancellation assertions remain.
The earlier unmodified Ubuntu24/1.0.5 client failures remain failed historical
evidence. The separately selected A/B experiment can still compile its explicit
`system-probe` variant; it cannot produce owned-native origin evidence.
GNOME/KDE, unmodified distribution backends, hardware buffers, real accounts and
installed packages remain separate release requirements.

The canonical fixed-client AppImage wrapper explicitly prepares the original
`libpipewire-0.3.so.0` variant. Its deployed paths, original library target,
post-deployment hash/build-ID checks and source notices remain in force. Its
receiving peer's compile receives the same explicit AppImage build input; the
peer is not substituted for the installed sender.

Generic Linux packaging has one explicit private QA route:
`python3 scripts/package.py --formats appimage --gitleaks /reviewed/scanner`.
It delegates to the canonical fixed-runtime/original-SONAME wrapper, including
its actual installed-capture, cleanup and log-scan gate. Only after those gates
and temporary-root cleanup pass does it atomically retain one digest-matched
QA installer in an empty, owned 0700 `dist` directory for the original package
smoke gate. No installer is uploaded or published. Default/all/DEB selections
are not reinterpreted; DEB maintainer and compatible packaging remain deferred.
This is a public, owner-selected standard-hosted QA path, not ordinary local
developer packaging or release support.

The existing disposable update harness uses the same original-SONAME builder
through a private build-only entry. A current clean reviewed Git checkout must
reproduce the complete exported source; only the two existing app-version
fields may differ. It accepts bounded HTTPS loopback feed/public-key compile
metadata, never signer keys or a general packaging-config override. The official
signer, installed update, invalid-signature/truncation/discard, rollback and
cohort assertions remain separate and unchanged. Build-only never claims the
installed-capture gate passed.

Neither route is runnable merely because its entry point exists. The exact
static-runtime tool/source prerequisite closure and scanner must be admitted and
provisioned on a fresh standard Ubuntu24/x64 hosted runner first. The
accompanying Linux-only permanent workflow proposal supplies the scanner
argument and the exact 18-DEB/634-file runtime host prerequisite set. It preserves
the original 15 versions and 632 rows, adds only matching compiler/binutils
siblings and two host-library hashes, and refuses inconsistent standard-host
dependencies without downloads or resolver fallback. This proposed provisioning,
its maintainer hooks and ambient dynamic dependencies still require independent
review and a fresh hosted check before any package run. Mac/Windows commands,
installed capture/update/smoke assertions and their budgets are unchanged.
The pre-existing AppImage runtime dependency finding remains open until the
patched runtime is actually built and the installed Linux gates pass. No package
execution, merge, release, signing or vulnerability waiver is implied.

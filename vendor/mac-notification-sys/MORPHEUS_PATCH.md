# Owned native notification patch: mac-notification-sys

Upstream: mac-notification-sys 0.6.15, crates.io archive SHA256
`fd604973958ddcc11b561193c0fb96ba146506ef2f231ef2e7c35fd2cbc9beca`.
The original manifest and README declare the dual MIT/Apache license, and their
attribution remains intact. The published archive omitted standalone license
texts. Its `.cargo_vcs_info.json` records upstream revision
`deb559683962d314a7693478476bf8d052efa84e` of
[mac-notification-sys](https://github.com/h4llow3En/mac-notification-sys).

`LICENSE-MIT` and `LICENSE-Apache` are unmodified copies of the files at that exact
revision ([MIT](https://raw.githubusercontent.com/h4llow3En/mac-notification-sys/deb559683962d314a7693478476bf8d052efa84e/LICENSE-MIT),
[Apache](https://raw.githubusercontent.com/h4llow3En/mac-notification-sys/deb559683962d314a7693478476bf8d052efa84e/LICENSE-Apache)).
Their upstream Git blobs are `a6a98a84ac3f77aa01837efa8f4ddf70fa8f5b62` and
`8dada3edaf50dbc082c9a125058f25def75e625a`, respectively. The MIT text retains its
original 2017 Felix Döring copyright notice; the Apache text is unchanged,
including its application appendix. All archive source/build files, README and
original Cargo manifest match the same upstream revision. No parent-project
license or invented grant is substituted. This restores the omitted full texts;
legal distribution approval and the remaining notice obligations stay separate.

The additive native route reserves its finite exact record before entering
Foundation and retains the same pthread/autorelease-pool ownership. Native
exceptions preserve the exact unresolved record. Foreign callbacks carry only
bounded correlation/event data; Rust pointer entry has an explicit unsafe
contract. No callback holds an SDK client, UI or account context. The original
Objective-C build hook remains part of the reviewed source and build graph.

The managed normalized manifest omits only development-tool tables. The complete
published normalized manifest is preserved as Cargo.toml.upstream; normal/build
dependencies, features and default features stay unchanged. All original archive
members, including tests, Cargo.toml.orig and the upstream lock, remain preserved.

The app must own the real native worker, retain finite identity obligations on
both success and error, and actually join worker/TLS and supervisor. Closed
admission alone is not completion. Whole-platform shutdown, real native API and
installed-update behavior still require verification; no distribution, signing
or complete OS-quiescence acceptance is established by source review.

# Owned native notification patch: notify-rust

Upstream: notify-rust 4.18.2, crates.io archive SHA256
`4587364a9a0074333429b3df75a30a205340c56a536ca3eb6ca0e59b87bbf8af`. Original MIT and Apache-2.0 attribution and license texts are retained.

The additive adapter accepts a bounded identity, original submission handover and
terminal cancellation. It creates no worker itself. Linux's private zbus executor
is driven on a fresh owned native thread outside any entered Tokio runtime.
macOS and Windows use their same-version native patches. Legacy APIs retain
their code and features. Submitted clicks remain fresh protected intents, never
old-account submission or call Answer authority.

The only active library-test development dependency was maplit. Its one two-pair
expected-map initializer is replaced with standard-library HashMap::from, with
identical expected keys/values and assertion. src/hints.rs.upstream preserves the
complete original. Original example/integration/doctest tools are retained in
the preserved upstream manifest; their verification is not claimed.
Linux daemon activation and exact RPC reply-identity corrections, actual app
presenter/native book and all-platform cleanup are required inputs.

The managed normalized manifest omits only development-tool tables. The complete
published normalized manifest is preserved as Cargo.toml.upstream; normal/build
dependencies, features and default features stay unchanged except the explicitly
documented Windows bindings feature. Original tests, examples, licenses,
Cargo.toml.orig and upstream lock remain preserved.

The app must own the real native worker, retain finite identity obligations on
both success and error, and actually join worker/TLS and supervisor. Closed
admission alone is not completion. Whole-platform shutdown, real native API and
installed-update behavior still require verification; no distribution, signing
or complete OS-quiescence acceptance is established by source review.

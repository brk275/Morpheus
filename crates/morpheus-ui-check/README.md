# UI compiler check

```sh
cargo check --locked -p morpheus-ui-check
```

Compiles the production Slint interface with the app's pinned compiler, without
building Matrix, native call adapters or the desktop runtime. This catches
syntax, type and layout-constraint errors early. It does not run the interface
or replace desktop, accessibility or platform checks.

Follow the repository's [build policy](../../AGENTS.md) when checking contributions.

# SQLite WAL page-buffer initialization backport

Exact base: libsqlite3-sys 0.38.2, original crates.io archive SHA256 f1d20bef17f513b9b3004532233187769cd072d790971f4e4da0e346eb6401e8.

Backport SQLite commit e5bf119821f25be1d7f10ef0a2edbe175289166e (Fossil f9b4ee369557c36c606666bd7e6d8dc4685d87531993d3ce4e7a718aa814a5ac) for RUSTSEC-2026-0459 / CVE-2026-119375. The same small sqlite3WalReadFrame change is applied to sqlite3/sqlite3.c and the unselected sqlcipher/sqlite3.c: initialize the output tail when the caller buffer exceeds the WAL page size, then read only the actual frame size. All other original archive bytes, C/Rust APIs, version and source-ID strings, build logic and license texts remain unchanged.

Morpheus enables the existing Matrix SDK bundled-sqlite feature on supported platforms. No SQLCipher, extension-loader, bindgen or OpenSSL feature is enabled by this patch. Do not treat the unchanged 0.38.2 version string as an upstream patched release: the provenance and exact backport identify this local correction. Build admission must exclude linked/system SQLite overrides (notably LIBSQLITE3_SYS_USE_PKG_CONFIG), validate selected features and linked inputs, and retain full source/license notices.

The vulnerability requires opening a crafted database plus sibling WAL. The app normally owns its local profile; no remote database-file import route has been demonstrated. Filesystem privacy and per-record encryption do not repair the native WAL reader. Source review is not runtime, migration or installed-update qualification.

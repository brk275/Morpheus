# Morpheus patch: lru 0.16.4

Source: crates.io lru 0.16.4, upstream revision d8c7f5ca51a86a8f561c14e21508a0f757aa05ad.
The original MIT LICENSE is retained.

Backport https://github.com/jeromefroe/lru-rs/pull/238 (upstream commit
2776ded, merged as f9a7f00) for RUSTSEC-2026-0253. Detach a popped node before
freeing it or invoking a potentially panicking key destructor. The upstream
regression test is retained with additional assertions for the panic and cache
contents. This keeps Tantivy and the pinned Matrix SDK dependency interfaces
unchanged. Replace the local patch when Tantivy accepts a tested fixed release.

The package version remains 0.16.4 for dependency compatibility. Version-only
advisory scanners can still flag it; verify this patch and its regression test
instead of globally suppressing memory-safety advisories.

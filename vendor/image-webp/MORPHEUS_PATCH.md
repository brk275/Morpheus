# Morpheus patch: image-webp 0.2.4

Source: the crates.io `image-webp` 0.2.4 archive, SHA-256
`525e9ff3e1a4be2fbea1fdf0e98686a6d98b4d8f937e1bf7402245af1909e8c3`.
Its recorded upstream revision is
`08b57d473c90148dd4b37a075df365afc3dfcc2b`. Both original licenses,
copyright notices and package manifests are retained. The provenance JSON
records every original file and the sole production source change.

## Bounded lossless decoder complexity

A small VP8L entropy image can select up to 65,536 groups, each containing
five Huffman trees. The advertised canvas dimensions and the generic
`image::Limits` setting do not bound this tree allocation. Morpheus rejects
more than 128 groups with the existing `MemoryLimitExceeded` error, after
reading the entropy labels and before constructing any group trees. The
check is in the shared lossless decoder path. VP8 and VP8L remain supported;
unusually complex lossless files can exceed this application policy.

This is a complexity cap, not a total decoder or process memory budget.
Each group has at most 3,136 alphabet symbols and five lookup tables of at
most 1,024 `u32` entries each. Actual storage also depends on Rust's tree-node
layout and allocator overhead. Caller-owned outputs, retained animation
frames, canvas copies, transform buffers and VP8 partitions are separate.
Morpheus must validate bounded input and all embedded frame dimensions and
chunk ranges before decoding, and must retain frame, cumulative work,
metadata, cancellation and concurrency limits. A generic allocation-limit
setter alone does not satisfy those requirements.

The application regression module exercises the public image decoder with
small VP8L streams. It checks that labels requiring 129 and 65,536 groups
produce the memory-limit error before missing outer tree bytes are read,
that all 128 admitted groups decode exact expected pixels, and that a
normal single-group stream still decodes. These cases require the app's
WebP feature and run with its existing hosted unit checks. No upstream
development tools or native decoder dependencies are enabled by this patch.

The upstream package is excluded from the application workspace, so its
development-only dependencies are not added to the workspace lock. Its
production dependencies remain the already pinned `byteorder-lite` and
`quick-error`. Reassess this patch against a reviewed upstream release
before removing it.

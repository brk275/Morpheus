# Native screenshots

These are real Slint window captures of Morpheus at
[`c02a1b7`](https://github.com/brk275/Morpheus/commit/c02a1b78fc1ce6a7b28b66c4b22768e6f30594c2),
using fictional offline preview accounts. They were captured on the standard
Ubuntu runner in [CI run 38167245637](https://github.com/brk275/Morpheus/actions/runs/38167245637),
attempt 1, on October 11, 2026.

The images are 1440 × 900 lossless PNGs with unchanged pixels and no embedded
metadata. They are application screenshots, not generated artwork. A preview
capture demonstrates the interface; it does not establish live Matrix or release
acceptance.

## Conversation

![Native conversation with fictional preview accounts](desktop.png)

## Account switcher

![Upward account menu above the active Matrix profile](account-switcher.png)

## Sign-in

![Native Matrix sign-in screen](sign-in.png)

## Capture provenance

The native capture command is `--morpheus-capture-preview`; CI converts its RGB
snapshots to PNG with `scripts/encode_preview_pngs.py`. The source artifact was
`native-preview-c02a1b78fc1ce6a7b28b66c4b22768e6f30594c2-1`,
ID `11712208811`. Its archive SHA-256 is
`c98b6e17ebc865d62a17f0ad67869d36ec3e61fc4c5127fc7398db428d42b915`.
`desktop-preview.png` was copied here as `desktop.png`.

| File | SHA-256 |
| --- | --- |
| `desktop.png` | `aa4cba901aec171f367e7f16c6f467c10df3b432659ab26d5c499b4e55ff381f` |
| `account-switcher.png` | `02d0546a30c30fc60a399d8ecf2097e622a1863c7ca57845a1c6e753e36fb0b3` |
| `sign-in.png` | `bbe4b3d5e7f1871221e7b6d818c0e3744257e5cf2cdf3a61d5893e82c26fb2ad` |

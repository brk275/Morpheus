# Native screenshots

These are real Slint window captures of Morpheus at
[`51c04ce`](https://github.com/brk275/Morpheus/commit/51c04ceec75538ff0bff3ef43cbfd3531870b3cb),
using fictional offline preview accounts. They were captured on the standard
Ubuntu runner in [CI run 38725671739](https://github.com/brk275/Morpheus/actions/runs/38725671739),
attempt 1, on October 15, 2026.

The images are 1440 × 900 lossless PNGs with unchanged pixels and no embedded
metadata. They are application screenshots, not generated artwork. A preview
capture demonstrates the interface; it does not establish live Matrix or release
acceptance.

## Terminal and account switching

Monospaced messages, aligned nickname columns, square controls, no message
avatars and a minimal room header. The upward account menu keeps separate
aliases close at hand.

![Terminal preset with the account switcher open](account-switcher.png)

## Nocturne

Stacked messages, avatars, room context and the members panel.

![Nocturne conversation with fictional preview accounts](desktop.png)

## Paper

Serif message text, warm light surfaces, compact headers and no message avatars.

![Paper preset with serif message text](paper.png)

## Account controls

The account manager exposes direct removal and one optional local app password
for protected switches. The fixture includes a pending-removal row to show the
retry control. See [account access](../ACCOUNT_ACCESS.md) for behavior and limits.

![Account manager with fictional accounts and removal controls](manage-accounts.png)

![Local app password dialog with empty fields](account-password.png)

## Sign-in

![Native Matrix sign-in screen](sign-in.png)

## Capture provenance

The native capture command is `--morpheus-capture-preview`; CI converts its RGB
snapshots to PNG with `scripts/encode_preview_pngs.py`. The source artifact was
`native-preview-51c04ceec75538ff0bff3ef43cbfd3531870b3cb-1`,
ID `11967634194`. Its archive SHA-256 is
`73f845089059c77ba899127b89d9d553ba59125fca61f43ee919aad84c1e2e87`.
`desktop-preview.png` was copied here as `desktop.png`, and `paper-preview.png`
as `paper.png`. Every image was copied byte for byte after privacy and visual
review. All six images below came from the same native executable, SHA-256
`b38d72688e565e9f725eb20383bb7351c07c4ac0225b08814830c404e6438a43`.

| File | SHA-256 |
| --- | --- |
| `desktop.png` | `83e665a72322f850ed43fe97994ad1037a4f32d0b39cd6ede06f1753f40d016d` |
| `account-switcher.png` | `da0e3a939f41d58c240b4faabe7e7c4f83814646b0faccc47d164c238d2be959` |
| `paper.png` | `e457518119ad00235a9a426704a4d293db72f32a25bf59dd399e8045b2a5c6a6` |
| `manage-accounts.png` | `3d957064e50094c781b3bb5feeda10eec83f8d1fcafcf0a214744fe0188f0796` |
| `account-password.png` | `2d7c5b6deaf529594910a08b8f961b4d1a09a6a9e188848a5c188b8b73adc035` |
| `sign-in.png` | `ad7c4ce4af8d49c746694db24eedb0120122cdf1bde39f175dc1c2bd1522cd64` |

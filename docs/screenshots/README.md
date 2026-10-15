# Native screenshots

These are real Slint window captures of Morpheus at
[`a38fe43`](https://github.com/brk275/Morpheus/commit/a38fe43ba149cd5f1384dfcb736a26ec4f652001),
using fictional offline preview accounts. They were captured on the standard
Ubuntu runner in [CI run 38737708120](https://github.com/brk275/Morpheus/actions/runs/38737708120),
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

## Sidebar and labeled rail

Optional text navigation, a combined room/DM list and unread-first ordering:

![Terminal preset with text navigation and combined conversations](terminal-sidebar.png)

The narrow icon rail can also show Space labels. Long names shorten visually;
controls retain the complete accessible name.

![Terminal preset with a labeled Space rail](terminal-space-labels.png)

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
`native-preview-a38fe43ba149cd5f1384dfcb736a26ec4f652001-1`,
ID `11974809132`. Its archive SHA-256 is
`794d015daf2aa08e06d7f35ef2477039d2f654eff24d4766229fc31fc2692e99`.
`desktop-preview.png` was copied here as `desktop.png`, and `paper-preview.png`
as `paper.png`. Every image was copied byte for byte after privacy and visual
review. All eight images below came from the same native executable, SHA-256
`ddc66c3c73379f0b0a5e3581eb53548b3beedba56882c9eadc6a45eb2d404b17`.

| File | SHA-256 |
| --- | --- |
| `desktop.png` | `83e665a72322f850ed43fe97994ad1037a4f32d0b39cd6ede06f1753f40d016d` |
| `account-switcher.png` | `41e564379d0d30b7c5eef2e13047c3d0e9933b063ef438ad6ae777124605d459` |
| `paper.png` | `e457518119ad00235a9a426704a4d293db72f32a25bf59dd399e8045b2a5c6a6` |
| `manage-accounts.png` | `730375e5c7105414022e9a5069cbd525c522db67f21ae0c1c23f47e0d6e479a3` |
| `account-password.png` | `2ff2503a6d165e9b5617ef92ab66fcfad72eb607a193939c5f112f08ca49a21a` |
| `sign-in.png` | `efcf41e65b1ca636511c612f2fbb56a024ba9b1f32c1ef906766e10a9d6ef367` |
| `terminal-sidebar.png` | `49bc38a632967106e2e068348dda67412f3d7957392cc82e79e485c0295494f4` |
| `terminal-space-labels.png` | `6a889b7599c7ca89cb92875bd643027b127359dab8aa0a3eaea4f4087cdcf112` |

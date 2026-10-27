# Morpheus gallery

Real native captures with fictional offline accounts. Each image is a
1440 × 900 lossless PNG. Open a theme image for the full-size capture.
These show the interface; Morpheus remains pre-alpha.

## Themes

| Terminal | Nocturne | Paper |
| --- | --- | --- |
| [![Terminal with text navigation](terminal-sidebar.png)](terminal-sidebar.png) | [![Nocturne with stacked messages and avatars](desktop.png)](desktop.png) | [![Paper with serif messages and light surfaces](paper.png)](paper.png) |
| Monospaced text, nickname columns and square controls. | Room context, avatars and a members panel. | Warm light surfaces, serif text and compact headers. |

[Customize a preset or make your own theme](../CUSTOMIZATION.md).

## Navigation and accounts

The bottom-left profile menu opens upward to switch between accounts.

![Terminal with the account switcher open](account-switcher.png)

<details>
<summary>Labeled Space rail</summary>

Long Space names shorten visually; controls retain their complete accessible name.

![Terminal with a labeled Space rail](terminal-space-labels.png)

</details>

<details>
<summary>Account management and local app password</summary>

Direct removal, cleanup retries and one optional password for protected switches.
The pending-removal row is a fixture. [Account controls](../ACCOUNT_ACCESS.md).

![Account manager with fictional accounts](manage-accounts.png)

![Local app password dialog with empty fields](account-password.png)

</details>

<details>
<summary>Sign-in</summary>

![Native Matrix sign-in screen](sign-in.png)

</details>

## Capture provenance

<details>
<summary>Source revision, capture run and image checksums</summary>

Captured from source `2b4f276379692e3e3e50b48bb80e5ade9e3f345d` on the standard Ubuntu runner in
[CI run 40543416624](https://github.com/brk275/Morpheus/actions/runs/40543416624),
attempt 1. This capture does not imply that the complete CI run or beta release gates passed.
The pixels are unchanged and the files contain no embedded metadata.

The native capture command is `--morpheus-capture-preview`; CI converts its RGB
snapshots to PNG with `scripts/encode_preview_pngs.py`. The source artifact was
`native-preview-2b4f276379692e3e3e50b48bb80e5ade9e3f345d-1`, ID `12752219176`.
Its archive SHA-256 is
`3a95b49e569ba2e266c78dc6f52bcd61829daf27ddc0f815695d344ef5fa4a2f`.
`desktop-preview.png` was copied here as `desktop.png`, and `paper-preview.png`
as `paper.png`. Every image was copied byte for byte after privacy and visual
review. All eight images below came from the same native executable, SHA-256
`e304274e18697584879c8c4723ffde3b7bf7fb207bb18ae7413e5dc5b7d4d592`.

| File | SHA-256 |
| --- | --- |
| `desktop.png` | `1239b00823817939e08a6f9a9fa72802a65ed5d98c5ece360401c5681bac70cd` |
| `paper.png` | `e42272abbc885a58648b0a964638c62ebb8c4bcc24f6712bba8a291aa5c9427f` |
| `account-switcher.png` | `7201f0b8be3fc6b95eb09f2ef85623bce27d7aff999c2f97d4f56ad5bdb9bd7d` |
| `manage-accounts.png` | `e6ff8713d1fd5476dd9c191e332bcbcf79e7adc749ed0f013c1f55c98f7e6766` |
| `account-password.png` | `9a110a2dcf2469e37de52874da350ce2d64044fc4da46b990d19712ef277f755` |
| `sign-in.png` | `cb82a16035ee1318923d9e4463396995b12337bb609066ba7b5f2f10444672a4` |
| `terminal-sidebar.png` | `bab46d740a4da7946276d2ceb0f04bcac3cb8567fd948ab32d4f2f9b871e4b5f` |
| `terminal-space-labels.png` | `721c39fad97b7f11f556923095485f27b149c169a881c8b1e82ced3e47ff1ec2` |

</details>

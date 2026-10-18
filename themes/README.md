# Morpheus themes

Color palettes for Morpheus. See the [customization guide](../docs/CUSTOMIZATION.md)
for typography, layout, semantic colors and sender palettes.

| Palette | Appearance |
| --- | --- |
| [Nocturne](nocturne.toml) | Charcoal surfaces with mint accents. |
| [Terminal](terminal.toml) | Green charcoal surfaces with muted green accents. |
| [Paper](paper.toml) | Warm light surfaces with forest accents. |
| [Aurora](aurora.toml) | An editable example with navy surfaces and violet accents. |

## Use a palette

Choose a base palette in Settings, or set its name in `config.toml`:

```toml
[appearance]
theme_pack = "terminal"
```

A palette changes colors. The **Nocturne**, **Terminal** and **Paper** appearance
presets also set fonts, message layout, spacing and geometry.

To edit a palette, copy its TOML file into the `themes` directory beside your
`config.toml`. Keep its filename and select that theme name. Changes reload live;
invalid edits report an error and preserve the last working appearance.
Theme files contain data and cannot execute code.

[Full configuration reference →](../docs/CUSTOMIZATION.md)

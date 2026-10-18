# Appearance contract, color tokens and Settings search

**Status: source checkpoint 1, not built, formatted or executed.** This page
records the contract, the token and search-target inventory and the native
transparency choice that the code on this branch implements. The sections
marked *Remaining* list what is not done yet; nothing here claims coverage of
"all" appearance.

The single source of truth is `crates/morpheus-config/src/registry.rs`:
`COLOR_TOKENS`, `FIELDS` and `SETTING_TARGETS`. The validator, the Appearance
workspace, Settings search and (planned) the offline schema command read those
tables; the tables below are a copy for reading.

## One declarative contract

`config.toml` (schema version 1) and the selected theme file are the only
appearance state. Settings writes the same keys a person or an assistant edits
by hand; there is no hidden GUI-only setting. Undo history in the color editor
is per-window presentation state; the file stays the record.

Inheritance, lowest to highest, for one color token:

1. The built-in palette of the effective theme mode. The mode is
   `account_overrides."<account>".theme` while that account is active, else
   `appearance.theme`.
2. The theme pack named by `appearance.theme_pack`: its eleven `[colors]`, its
   optional `[roles]` and `[senders]`.
3. Sparse overrides in `[colors]` of `config.toml`, keyed by token ID.

Overrides apply in every theme mode and survive choosing a starting point
(presets change typography, layout, geometry and `theme_pack`, never
`[colors]` or `[window]`). Color overrides are global; per-account color
overrides are not part of this checkpoint.

```toml
schema_version = 1

[appearance]
theme_pack = "nocturne"

[colors]            # sparse: only the tokens you change
canvas = "#1E2330"
link = "#AFCDFFCC"  # alpha last
scrim = "#00000099"

[window]
opacity_percent = 85
```

### Hex convention

`#RRGGBB` or `#RRGGBBAA`: six or eight hexadecimal digits, either case, with
alpha **last** (`00` fully transparent, `FF` opaque). There is no `#RGB`
shorthand, no color names and no `rgb()` syntax. The editor writes uppercase,
and omits the alpha pair when it is `FF`. Theme files use the same convention;
an older build that only knew `#RRGGBB` rejects a file containing alpha.

### Validation and last-valid behavior

Unknown keys, unknown token IDs, malformed colors and out-of-range numbers
reject the whole edit. Live reload, Settings saves and (planned) the offline
validator share one bounded reader:

| File | Limit |
| --- | --- |
| `config.toml` | 256 KiB |
| Theme file | 64 KiB |

Only a regular file is read. Symbolic links are followed for reading. A
directory, FIFO, socket or device is refused before it is opened, so it cannot
block the reader. That type check and the open are separate system calls: a
file swapped for a FIFO in between is not covered, and no race safety is
claimed. Size is enforced on the bytes actually read. Text must be UTF-8.

A rejected edit leaves the last valid appearance applied. Settings re-reads the
file before every save, so an invalid hand edit is never overwritten by a UI
action. Settings writes `config.toml` with a same-directory temporary file and
an atomic rename, which replaces a symbolic link rather than writing through
it. Nothing writes theme files. Theme names are ASCII letters, digits, `-` and
`_`, at most 64 characters, so they cannot name a path.

Validation errors carry a field path and a fixed problem code; they never
repeat the rejected value. The diagnostics module (`morpheus_config::diagnostics`)
adds a line and column from the TOML spans, shows only plain identifier keys
(others become `<key>`) and never prints raw TOML.

## Color tokens

36 tokens, all consumed by the interface. **Base** tokens are the theme pack's
eleven required colors; **Role** tokens are optional theme roles. Fallbacks
apply when neither `[colors]` nor the theme sets the token.

| Group | Token | Source | Fallback |
| --- | --- | --- | --- |
| Surfaces | `rail` | Base | built-in palette |
| Surfaces | `sidebar` | Base | built-in palette |
| Surfaces | `canvas` | Base | built-in palette |
| Surfaces | `panel` | Base | built-in palette |
| Surfaces | `elevated` | Base | built-in palette |
| Surfaces | `hover` | Base | built-in palette |
| Surfaces | `selection` | Role | `hover` |
| Surfaces | `own_surface` | Role | row background |
| Surfaces | `scrim` | Role (new) | each dialog's built-in dimming, `#000000AA`–`#000000CC` |
| Surfaces | `shadow` | Role (new) | `#00000044` |
| Surfaces | `media_backdrop` | Role (new) | `#11131B` |
| Text and messages | `ink` | Base | built-in palette |
| Text and messages | `muted` | Base | built-in palette |
| Text and messages | `message` | Role | `ink` |
| Text and messages | `own_message` | Role | `sender`, then `message`, then `ink` |
| Text and messages | `sender` | Role | `ink` |
| Text and messages | `timestamp` | Role | `muted` |
| Text and messages | `link` | Role | `accent` |
| Accents and states | `accent` | Base | built-in palette |
| Accents and states | `accent_ink` | Base | built-in palette |
| Accents and states | `focus` | Role | each control's built-in focus color |
| Accents and states | `unread` | Role | `accent` |
| Accents and states | `unread_ink` | Role | `accent_ink` |
| Accents and states | `selection_ink` | Role | each control's text color |
| Accents and states | `error`, `error_surface`, `error_ink` | Role | each message's built-in color |
| Accents and states | `success`, `success_surface`, `success_ink` | Role | each message's built-in color |
| Accents and states | `destructive`, `destructive_ink` | Role | built-in |
| Accents and states | `presence_online` | Role (new) | `#64C9A7` |
| Inputs and controls | `divider` | Base | built-in palette |
| Calls | `voice` | Role | `#78C8B1` |
| Avatars | `avatar_ink` | Role (new) | `#FFFFFF` |

Custom colors are the owner's choice: syntactically valid low-contrast values
are applied as written. The shipped presets keep their separate contrast tests.

### Deliberately not themeable

- Message images, video, avatar pictures and sticker pixels.
- Operating-system chrome: title bars, native menus, file dialogs, desktop
  notifications and the system cursor.
- The startup error window shown before configuration loads.
- The base-palette preview swatches in Settings, which depict the built-in
  modes.

### Remaining literal defaults and planned tokens

- Avatar and account background palettes come from a fixed five-color list in
  the Rust models (`live.rs`, preview fixtures) and the call-stage placeholder
  tiles. Editing them live needs model indices instead of baked colors; an
  `[avatars]` palette is planned, not accepted yet.
- Inputs, buttons, the composer and icons use `elevated`, `hover`, `ink` and
  `muted`; separate `input_surface`, `button_surface`, `composer_surface` and
  `icon` tokens are planned.
- Call stage surfaces, in-app notification banners beyond the error/success
  roles, and unread badges other than the count badge use base tokens only.
- Error texts keep slightly different built-in defaults per control until
  `error` is set; setting it recolors all of them.
- Sender palette editing in the workspace is planned; it remains in theme files
  (`[senders]`, 1–16 colors).

## Native window transparency

### API choice

Pinned sources checked: Slint 1.18.1 (`i-slint-backend-winit`,
`i-slint-renderer-femtovg`) and winit 0.30.13.

- The winit backend creates every window with `with_transparent(true)` on
  Linux and Windows (`WinitWindowAdapter::window_attributes`). The default
  FemtoVG OpenGL renderer prefers a transparency-capable GL configuration and
  falls back to an opaque one when the system offers none
  (`renderer/femtovg/glcontext.rs`).
- On macOS the backend asks for transparency only while the `Window`
  background is not opaque, and follows a background that changes while the
  window is shown (`wants_transparent`, `set_transparent`).
- The renderer clears the surface with the `Window` background color, alpha
  included.

So the app needs no new dependency, Cargo feature, `unsafe` code or native
window accessor: it paints a translucent `Window` background and translucent
pane surfaces. `[window] opacity_percent` (0–100, default 100) scales the alpha
of the rail, room list, conversation canvas and details panel. Below 100 the
`Window` background is cleared and each of those panes paints its own scaled
color, so panes do not compound. Children that used to repaint their pane's
color are transparent, which looks identical at 100.

Text, icons, inputs, cards, popups, dialogs, the sign-in page and recovery keep
their own colors, so they stay opaque unless a color token itself is given
alpha. There is no blur and no whole-content fade. At 100 the window is
painted exactly as before, so macOS keeps its opaque window path.

### Platform status

Not verified on any platform yet; the status line in Settings says so.

| Platform | Expected | Limitation |
| --- | --- | --- |
| macOS | Live | AppKit draws the title bar; it is not made translucent. |
| Windows | Requested | Depends on DWM composition and the GL driver's pixel format. |
| Wayland | Requested | Depends on the compositor honoring alpha. |
| X11 | Requested | Needs a running compositing manager; otherwise the panes may look black or opaque. |

The app cannot observe whether a compositor actually blends the window, so it
never reports "supported"; it reports "requested" and "not yet verified".

### Verification plan (not run)

1. Build each target with no feature changes. Launch with
   `[window] opacity_percent = 40` and a distinctive desktop wallpaper.
2. Capture the window region and a reference capture of the same region with
   the window hidden. Pixels inside the rail, room list, canvas and details
   panel must differ from both an opaque capture and the reference, and text
   pixels must match the opaque capture.
3. Set 100 and confirm the capture equals the pre-change opaque baseline.
4. Drag the slider and confirm live updates without a restart on each target;
   record per-target results, including X11 with and without a compositor.
5. Open sign-in, account unlock and recovery: their surfaces must stay opaque
   at every value.

## Settings search

A pinned search field at the top of Settings indexes `SETTING_TARGETS`: label,
description, keywords and aliases (including "colour"), category and
breadcrumb. Queries are at most 64 characters and return at most 8 results;
every word must match. Up and Down move the highlighted result, Enter reveals
it and Escape clears the query. Choosing a result scrolls to the control,
computed from the live layout, and focuses it; the focus ring is the
highlight. Account results open Manage accounts and focus Add account, the app
password control or the dialog's close action; removal is never focused, and
every action still needs its own activation and confirmation. Search never
changes a setting. Security & recovery is offered only while signed in.

Targets (33): appearance presets, colors, window transparency, base palette,
reduce motion, fonts, message size, sender weight, room header, conversation
style, message details, timestamps, corner radius, spacing, rail width, font
scale, density; navigation, pane sizes, unread first; desktop notifications,
sounds, previews, highlight words; add/open accounts, remove an account, app
password; security & recovery; image previews, audio devices; shortcuts;
settings file; updates.

## Remaining in this checkpoint

- **Not wired yet:** the Rust controllers behind the color workspace and the
  search field. Their Slint callbacks exist and do nothing until the
  controllers land, so the workspace and search are inert in this checkpoint.
- **Not written yet:** the offline `config validate` / `config schema`
  command, which will dispatch before the preview, updater, startup session,
  window, account store, credential vault, sync or devices start.
- **Tests pending:** generated-window scenarios for hex apply/reset/invalid
  retention, search keyboard behavior and every target's reachability; the
  registry, diagnostics, bounded-read and config tests are written.
- Typography line height and body weight, icon sizing, avatar shape, pane
  order and visibility beyond the existing panel toggle, animation durations,
  and shortcut actions beyond the four existing ones are not editable yet.
- Formatting has not been run; expect `rustfmt` changes, especially in the
  long registry tables.

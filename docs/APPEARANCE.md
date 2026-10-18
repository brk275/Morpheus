# Appearance contract, color tokens and Settings search

**Status: source checkpoint 2, not built, formatted or executed.** This page
records the contract, the token and search-target inventory and the native
transparency choice that the code on this branch implements. The sections
marked *Remaining* list what is not done yet; nothing here claims coverage of
"all" appearance.

The single source of truth is `crates/morpheus-config/src/registry.rs`:
`COLOR_TOKENS`, `FIELDS` and `SETTING_TARGETS`. The validator, the Appearance
workspace, Settings search, the diagnostics allowlist and `morpheus config
schema` read those tables; the tables below are a copy for reading. A test
(`the_field_reference_agrees_with_the_real_schema`) requires a fully populated
configuration to use exactly the `FIELDS` keys, every default to match
`Config::default()`, and every documented range and option to agree with
validation, including the dynamic `colors`, `shortcuts` and
`account_overrides` keys.

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
reject the whole edit. Live reload, Settings saves and the offline validator
share one bounded reader:

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

Every error the app shows (initial load, theme load, live reload and Settings
saves) and every offline diagnostic uses the same sanitized form: a fixed code
and message, a field path and a line and column. The TOML parser's own message
can quote the file, so it is classified and dropped; validation errors carry a
field and a fixed problem and never repeat the rejected value; the schema
version message does not repeat the number. Field paths are built only from
registry names: a key the schema does not know becomes `<unknown>`, and every
local account ID becomes `<account>`. An error in an account list points at
the failing entry of the failing account, in the validator's account order.

### Offline validation

```text
morpheus config validate [--json] [--file CONFIG.toml]
morpheus config schema [--json]
```

These run first in `main`, before the screenshot preview, the updater, the
startup session, the window, the account store, the credential vault, sync and
media devices. They read with the bounded reader, create no folder or file and
write nothing. `validate` exits 0 when valid, 1 when not and 2 for a usage
error. A repeated option, an unknown word or more than four arguments is a
usage error; at most one argument beyond that limit is read, so a long
argument list is refused rather than cut short. Output never prints a path or
the configured theme name, which is file content: `--json` output has the
format `morpheus-config-diagnostics` (version 1) with `valid`, a fixed `theme`
category (`none`, `built-in`, `built-in-override` or `custom`) and `file`,
`code`, `message`, `field`, `line` and `column` per diagnostic, and the text
form names the same category. `schema --json` prints the field
reference, color tokens, theme-file keys, limits and every diagnostic code from
the registry. On Windows the release app is a GUI program: its output is
visible only when redirected to a file or pipe; the exit status is always set.

Safe incremental edits: change one key, run `config validate`, and let the
running app reload it (about once a second); an invalid edit keeps the last
valid appearance and shows a fixed error. Settings writes the same keys.

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
are applied as written. The shipped presets set every role explicitly and keep
their separate contrast tests, including 3:1 for the online indicator on every
surface. With a preset, one `scrim` value replaces the per-dialog built-in
dimming.

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
of the rail, room list, conversation canvas and details panel. Below 100, or
whenever one of those pane colors has its own alpha, the `Window` background is
cleared and each pane paints its own color once, so panes never compound, at
100 or 99 alike. Children that used to repaint their pane's color are
transparent, which looks identical when opaque.

Text, icons, inputs, cards and popups keep their own colors, so they stay
opaque unless a color token itself is given alpha. Dialogs, the sign-in page,
account unlock and recovery always paint opaque versions of their colors,
whatever alpha a token or the window setting has. There is no blur and no
whole-content fade. With opaque colors at 100 the window is painted exactly as
before, so macOS keeps its opaque window path.

The slider previews while it moves. A pointer or key release saves at once;
keyboard, accessibility and number edits also save once the value has settled
for 400 ms. A rejected save, for example because `config.toml` became invalid,
restores the last valid value in the window and in both controls.

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
breadcrumb. It also indexes every color token by its ID, the words of its ID,
label, group, keywords and description, so `presence_online`, "online" or
"links" reaches one color directly. Queries are at most 64 characters and
return at most 8 results across both kinds; every word must match. At equal
scores settings come first. A color result has the ID `color:<token>`, and
only a registry token ID resolves. Choosing it selects that token in the
workspace (its group, no filter, its shown color in the field) and focuses its
hex field, reported as `appearance.color_hex`. Nothing is saved until a value
is typed. Up and Down move the highlighted result, Enter reveals
it and Escape clears the query. Choosing a result scrolls to the control,
computed from the live layout, and focuses it; the focus ring is the
highlight. A reveal reports its target only when that control actually took
focus. When the control is unavailable (update checks without a configured
feed, audio devices that are off or busy, a busy security refresh) its
read-only status line is focused and reported as `<target>:status` instead.
Results scroll inside a bounded region below the pinned field; Up and Down
keep the highlighted row visible while the field keeps focus. Account results
open Manage accounts and focus Add account, the app password control or the
dialog's close action; removal is never focused, and every action still needs
its own activation and confirmation. Search never changes a setting or runs
an action. Security & recovery is offered only while signed in.

## Appearance workspace

Settings → Appearance → Colors and window holds one bounded, scrolling list of
groups and tokens beside (or, at narrow widths and large text, above) one
selected-token editor: swatch, label, description, where the color comes from,
the hex field, Reset and Undo, and the advanced `[colors]` key. Choosing a
token moves focus to its hex field; Slint scrolls a focused control into view
in the list and in the page. A complete valid value applies and saves at once;
partial input only shows an inline hint. Reset all asks for a second
activation and can then restore what it removed. The Window group holds the
transparency slider with its numeric entry.

The applied colors can lag the file by up to one reload poll, so every write
is decided on the file as re-read for that save. An undo step records the
color the file held, and Reset all removes and remembers the overrides the
file holds then; both are kept only once the save succeeded. Undo applies only
while the color still holds the value this window saved: after a later edit
made elsewhere it changes nothing and says so. A restore that cannot read or
write the file keeps its snapshot so it can be tried again; a color set again
since the removal is kept.

Targets (33): appearance presets, colors, window transparency, base palette,
reduce motion, fonts, message size, sender weight, room header, conversation
style, message details, timestamps, corner radius, spacing, rail width, font
scale, density; navigation, pane sizes, unread first; desktop notifications,
sounds, previews, highlight words; add/open accounts, remove an account, app
password; security & recovery; image previews, audio devices; shortcuts;
settings file; updates.

## Remaining

- Number settings show their unit: `px` for lengths, none for the sender font
  weight, `ms` for durations and `%` for percentages as those fields arrive.
- Typography line height and body weight, icon sizing and colors, avatar
  shape and the avatar/account palettes, pane order and visibility beyond the
  existing panel toggle, animation durations, the sender palette in the
  workspace, separate input, button, composer and icon tokens, per-account
  color overrides and shortcut actions beyond the four existing ones are part
  of the objective and not editable yet.
- Native transparency has not been verified on any platform; see the plan
  above.
- Formatting has not been run; expect `rustfmt` changes, especially in the
  long registry tables, and Slint indentation fixes.

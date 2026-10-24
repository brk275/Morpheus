# Theme studio

Status: **source only**. This has not been compiled, tested or run.

Settings → Appearance is the theme studio. It holds the color workspace, the
size and shape editors, the sender palette, the starting points and their
variations, style profiles and the live preview. This page describes how the
studio saves, and the settings it covers.

## Changes are a draft until you apply them

A look change made anywhere in Settings shows at once in the whole window,
not only in the preview card. It is not written to `config.toml` yet. A bar
at the top of every page counts the unsaved changes and offers:

- **Undo** and **Redo**, for up to 64 steps each;
- **Discard**, which shows your saved look again and writes nothing;
- **Apply**, which saves every drafted setting at once.

**Reset section** (below the editor links) puts one section back to its
built-in values: theme and motion, fonts and type sizes, shapes and spacing,
messages and header, panes and navigation, window transparency, colors for
all accounts, or sender name colors. The reset is a draft too, so it can be
undone or discarded.

Some settings always save at once:

- Behavior settings: notifications, shortcuts, call devices, image previews
  and updates.
- Text size, Reduce motion and the focus ring width. These are accessibility
  choices and are never held back.
- Room grouping and unread-first sorting.

While a draft exists, look changes made outside Settings join it too, such as
resizing a pane or showing the details panel, so the bar always describes
everything unsaved. With no draft, those changes save at once, as before.

An invalid value never becomes part of the draft. A partial or invalid color
or number stays in its field with an inline hint, and the last valid look
stays shown.

A missing or malformed theme file does not block a valid notification,
device, shortcut or experimental-setting change. Those changes still save
over the exact settings revision that was read, while the last valid look
and pending appearance draft stay unchanged. The theme error remains
visible. A callback that also changes appearance cannot use this fallback;
the settings file itself must remain valid too.

## When the settings file changes meanwhile

Apply re-reads `config.toml` first.

- A setting changed only in the file (by hand or by another editor) is kept.
- A setting you drafted and the file left alone is saved.
- A setting you drafted that the file changed to a different value is a
  conflict. Apply then names those settings by their `config.toml` keys and
  asks you to **Keep mine**, **Keep the file's** (everything else is still
  saved), or go **Back** to editing. Apply writes nothing until you choose;
  the section below says what a save checks at the last moment and what it
  cannot.

When the file is reloaded while you are drafting, the window shows your draft
over the new file, and says how many drafted settings it also changed.

A file that cannot be read or is invalid when Apply reads it is not written
over. Apply then saves nothing and keeps your draft shown.

### What a save checks at the last moment

Apply, and every setting saved at once while you draft, writes only over the
exact text of `config.toml` it read. The new file is prepared first, and the
file is compared with that text again immediately before it is replaced.

- If the file changed after it was read, nothing is written. The file keeps
  the other version, your draft stays shown, and Apply asks about any drafted
  setting that version also changed. Apply again saves your draft over it
  and keeps the rest of that version.
- Morpheus windows and background tasks never save between each other's
  check and write (a lock file, `config.toml.lock`, sits beside the settings
  file for this). A save that finds another one in progress writes nothing
  and never freezes the window: Apply tries again a few times on its own,
  and stops if you change the draft meanwhile; other settings say so and
  ask you to try again.
- Other programs (editors, sync tools, scripts) do not take that lock, and
  nothing can make them. A change they write in the instant between
  Morpheus's final check and its replacement of the file can still be lost,
  and that version is not kept anywhere. Morpheus narrows that window to
  one read and one rename. It cannot close it or keep the replaced version,
  because no portable file operation replaces a file only if it is
  unchanged. Apply is therefore not safe against every concurrent edit.

## Accounts

Colors drafted for "this account only" belong to the account that was active
when you drafted them. When another account becomes active, including after
removing or signing out of an account, those colors are discarded with a
notice, and your other drafted changes stay. They are never written to
another account.

## Starting points, variations and style profiles

- The **starting points** Nocturne, Terminal and Paper, and their
  **variations**, fill in ordinary editable settings. The variations are
  Nocturne Compact, Nocturne Large Print, Terminal Soft, Paper Wide and
  Contrast Reading, which uses the built-in high-contrast colors. In Settings
  a choice is drafted, so you see it before you apply it. Neither changes text
  size, Reduce motion, the focus ring, accounts, notifications, shortcuts or
  devices.
- **Style profiles** are named local looks you can save, preview, import and
  export as small TOML files; see [style profiles](STYLE_PROFILES.md).
  - A profile preview and a draft exclude each other: apply or discard the
    draft before previewing or applying a profile.
  - **Save current look** saves the look shown, including a draft.
  - Profiles hold appearance only: no account IDs, credentials, notification
    history, paths or audio preferences.

## Theme packs

A theme pack is a palette file (`themes/<name>.toml` beside `config.toml`).
It holds base colors, semantic roles, interaction states (including the
per-control icon colors) and sender colors, and nothing else. A pack never
holds account IDs, credentials, paths, notification history or audio
preferences, and nothing in it is executed or downloaded.

- **Import theme pack…** opens the system dialog. The chosen file is read
  only if it is a regular file of at most 64 KiB, opened without following a
  link. Unknown keys, another `schema_version` and invalid colors are refused
  with a message that never repeats the rejected value.
- A valid pack is then shown over your current look, with a bar on every
  page: **Apply** installs it into the themes folder and selects it, and
  **Cancel** restores your look. Existing installed packs are never replaced.
  If a pack of the same name is installed, the preview says so and cannot
  install. Change the `name` field in the imported TOML file, then import it
  again to keep both packs. A file appearing at the destination meanwhile
  is kept by the no-clobber installation too.
- An import or export belongs to the account and settings that were active
  when its file dialog opened. If the account changes before the dialog
  answers (even back to the same account), you sign out, or Settings starts
  over, the answer is ignored and the bar says so: nothing is previewed,
  installed or written. Morpheus cannot close a system file dialog; it only
  ignores the late answer. Apply also checks this itself before it writes
  anything.
- The installed file is named from the pack's own name: lowercase letters,
  digits, `-` and `_`, at most 40 characters, never a device name.
- The themes folder holds at most 64 packs. It must be a real folder, not a
  link, and every installation goes through a temporary file and a
  no-clobber move that refuses an existing target.
- A preview saves nothing. It ends without restoring when a settings change
  is accepted, and with restoring on an account switch. It excludes a draft
  and a style profile preview.
- **Export current pack…** saves the last accepted global pack palette,
  installed or built in, with your accepted global color and sender-color
  overrides. It excludes account and room overrides, drafts and previews;
  an invalid external edit does not replace the last accepted palette.
  It writes a new file you choose and never replaces an existing file, even
  when the system dialog asked about replacing it: choose a new name instead.
  The built-in colors without a pack have no file to export. For a portable
  look that also includes fonts, geometry and other appearance choices, use
  the existing [style profile export](STYLE_PROFILES.md).

Original examples are in [`examples/theme-packs`](../examples/theme-packs):
Tidepool (dark teal) and Ember (warm light).

## Typography: line height and weight

`typography.line_height_percent` (100–200) and `typography.body_weight`
(300–800) control composer line spacing and default body weight. The body
weight is inherited by rich messages, composers and other text without an
explicit weight; sender, heading and Markdown span weights stay explicit.
The preview's composer sample shows both settings. Rich messages keep their
natural line spacing. Omitting either key keeps the built-in look.

## Icon colors per control

`icon_navigation`, `icon_header`, `icon_composer` and `icon_message` color
the navigation rail, the conversation header actions, the composer tools and
the message quick actions. Each follows `icon` until set. Switched-on and
primary icons keep `icon_active`. Set them in the color workspace, in
`config.toml [colors]` or in a theme pack's `[states]`.

## Finding a setting and editing the file

- **Settings search** (`Mod+Shift+F`) reaches every color token, every size or
  shape field and every section. It shows the `config.toml` key each result
  edits, and choosing a result focuses that exact field.
- `config schema --json` lists every field, its default and limits, and every
  color token ID; see [editing appearance](EDITING_APPEARANCE.md). These IDs
  are stable and are the same keys the studio drafts.
- Drafted settings are the keys of:
  - `[appearance]` (except `scale_percent` and `reduce_motion`);
  - `[typography]`, `[chat]`, `[conversation]` and `[window]`;
  - `[geometry]` (except `focus_width`);
  - `[layout]` (except `group_direct_messages` and `sort_unread_first`);
  - `[colors]`, `[senders]`;
  - each account's `theme` and `colors` in `[account_overrides.<id>]`;
  - each of its rooms' `theme` and `colors` in
    `[account_overrides.<id>.room_appearance.<room>]` (dropped with their
    account like the account's own colors).

## Transparency and contrast

Window transparency scales pane backgrounds only. There is no blur or
acrylic effect, and native compositing is still subject to platform
verification ([appearance contract](APPEARANCE.md)). Contrast notices in the
editors are advice. With transparency, ratios assume an opaque background;
actual contrast also depends on what is behind the window. The built-in
high-contrast theme and Contrast Reading variation provide accessible
starting points; custom color overrides can change their contrast.

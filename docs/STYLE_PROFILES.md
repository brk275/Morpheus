# Style profiles

A style profile is a named look saved as a small TOML file. Profiles let you
keep several looks, try one before using it, and share looks with other people
or edit them with an assistant. They are managed in **Settings → Appearance →
Style profiles**.

## What a profile holds

Only appearance:

- the theme mode and the theme pack name;
- fonts and type sizes;
- shapes and spacing (geometry), except the focus ring width;
- message and conversation presentation (layout, avatars, timestamps,
  nicknames, header);
- pane layout: navigation style, pane widths, member panel, Space labels,
  details-panel side, direct-messages order and pane densities;
- window opacity;
- global color tokens and the sender-name palette.

A profile never holds, and applying one never changes: accounts or local
account IDs, sign-in or passwords, notifications or highlight words,
shortcuts, media devices, drafts, call modes, security or update preferences,
per-account themes and colors, or the three accessibility choices that stay
with you: text size, Reduce motion and the focus ring width. Grouping direct
messages and sorting unread rooms first organize rooms and stay too.

Applying a profile replaces your global look, including the global `[colors]`
table. An account's own theme and colors still draw over it while that
account is active.

## Using profiles

- **Save current look** saves the look you see now under the name in the name
  field. A name already used asks before replacing that profile.
- **Preview** shows a profile without saving anything. A bar at the top of
  the window stays until you choose **Apply** or **Cancel**. Cancel restores
  exactly the look you had. Switching accounts, a change to your look in the
  settings file from elsewhere, or saving any setting ends the preview first; a setting
  that would change the look is not saved during a preview, so make it again
  afterwards.
- **Apply** saves the profile's look into `config.toml`. The file is re-read
  first: when it has an error, nothing is changed and the error is shown.
- **Rename**, **Duplicate** and **Delete** act on the selected profile.
  Delete asks first and never changes your current look.
- **Import…** reads a profile file you choose and adds it to the list. It is
  not applied; preview it first. **Export…** writes the selected profile to a
  file you choose; the system dialog asks before replacing a file.

## Where profiles are stored

In the `style-profiles` folder beside `config.toml` (see the Advanced
customization section of Settings for that location). Morpheus lists at most
64 profiles and examines at most 512 entries of that folder; it never looks
in other folders. Each profile file is at most 64 KiB. File names are chosen
from the profile name when it is created (lowercase letters, digits and
hyphens) and do not change when the profile is renamed.

Only regular files are read. Symbolic links, folders, sockets, FIFOs and
devices in the profiles folder are skipped, and a link (or Windows junction)
in place of the folder itself is never used: the folder is checked before and
after each file in it is opened, so one replaced meanwhile is noticed. Files
are opened without following a link and without waiting on a FIFO or device
that replaced a checked file; on Windows the profiles folder refuses any
reparse point. File names never use Windows device names such as `con` or
`com1`. Imports and exports refuse links, junctions, folders and devices too;
files in a cloud-synced Documents folder can still be imported and exported.
Every write goes to a temporary file in the same folder that then replaces
the target, so a failed write keeps the previous file.

## File format

Profiles are versioned, sparse TOML. Every key is optional; a key a profile
omits is the built-in default when the profile is applied, so a profile
always gives the same look. Keys use the same names, values and limits as
`config.toml`.

```toml
# Morpheus style profile.
profile_version = 1
name = "Reading room"

[appearance]
theme = "light"
theme_pack = "paper"

[typography]
message_font_family = "serif"
message_size = 16

[geometry]
corner_radius = 4
avatar_roundness = 50

[layout]
details_side = "left"

[colors]
link = "#335F86"

[senders]
colors = ["#2E684A", "#80552C", "#625684"]
```

Ready-to-import examples are in
[`examples/style-profiles`](../examples/style-profiles): Harbor, Ledger and
Lantern, checked by the configuration tests.

`profile_version = 1` and `name` (1 to 48 characters, without line breaks or
control or bidirectional formatting characters) are required. The accepted
keys are listed by `morpheus-app config schema` (the `style_profile` section
of `--json`). Unknown keys, another `profile_version`, values outside a key's
limits, unknown color tokens and theme-pack names that are not plain names are
rejected with a message that names the key, never the rejected value; the
list shows such a file as invalid, and it cannot be previewed or applied
until it is fixed or deleted.

A profile names a theme pack but does not contain it. The built-in packs
(`nocturne`, `terminal`, `paper`) are always available; any other pack must be
in your `themes` folder, or preview and Apply report that it is missing.

Morpheus writes only keys that differ from the defaults, after a short header
comment. Comments you add by hand are kept until Morpheus rewrites the file,
which happens when you rename it, replace it or import it; an imported file is
stored as Morpheus writes it, not as a copy of the original text.

## Privacy

Profile files contain no account, device or message information, so they can
be shared. Profile names and file names appear only in this window; Settings
search finds the Style profiles section, never a profile. Messages about
profiles never include a file path or a rejected value. Nothing in a profile
is executed, downloaded or opened automatically: fonts are names resolved on
your computer, and a theme pack is a name in your own themes folder.

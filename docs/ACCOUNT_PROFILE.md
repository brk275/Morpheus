# Public profile editor

**Status: source only. Not built, run or tested yet.**

The public profile editor changes the display name and avatar that other people see for the **current** Matrix account. You can open it from three places:

- **Public profile** in the bottom-left account menu.
- **Edit public profile of the current account** in Manage accounts.
- Settings search ("profile", "display name", "avatar"). This focuses the Manage accounts entry and opens nothing on its own.

## What it changes, and what it doesn't

The editor names the Matrix user ID and homeserver it is changing. Only the public `displayname` and `avatar_url` profile fields change. It never changes:

- the local account entry;
- the Matrix user ID;
- the device display name;
- sign-in credentials;
- the local app password;
- any other account.

It never switches, logs out or removes an account. Ordinary edits use the session's existing access token, so password and SSO accounts are never asked for a password.

## Draft, saving and outcomes

- The editor keeps three things separate:
  - what the server last confirmed;
  - your local draft (a typed name, a chosen image, or a staged removal);
  - the request in flight.
- **Choose image...** previews the image locally. It isn't public until you Save.
- **Remove avatar** stages a removal, and **Keep current** drops the avatar change.
- **Save to public profile** sends each changed field as its own request. Each request is sent exactly once (SDK retries off), and then the profile is read back:
  - A field counts as saved only when the read-back shows exactly the change. The name is compared on the exact value the server returned (within the name bound), never on the shortened, normalized copy shown on screen, so a cut-off prefix or changed spacing is not reported as saved. A removed name counts as removed when the server returns none or an empty one.
  - Profile reads and writes are single attempts, authenticated, and capped at a 64 KiB response; a larger response is refused with a fixed message.
  - If a send fails, or the editor closes while it is in flight, the outcome is *unknown*. The editor reads the profile back once, says what the server shows, and never resends on its own.
  - Each outcome is reported separately, followed by what the server shows. For example: "Display name saved. Avatar not confirmed; the server may or may not have it. The server now shows the name "..." and no avatar. Save again to retry only what still differs."
  - The chosen image is kept after a failed upload, so a retry doesn't need the file chosen again.
  - A request that already reached the server is never described as cancelled or undone. After a timeout, **Refresh** shows what the server holds.
- Display names are trimmed, and runs of spaces collapse to one. They must be 1 to 256 characters and at most 1 KiB, on a single line, without control or direction-override characters.
- **Remove display name...** asks first. The confirmation names the exact Matrix account and says its public profile will have no display name. Removal is its own single-send request, and its outcome is reported the same way as for Save.
- Images go through the existing room-avatar path:
  - one guarded native chooser, reading a regular file of at most 8 MiB;
  - the pinned PNG or baseline-JPEG decoder, at most 4096 px per side and 4 megapixels;
  - a stripped PNG of at most 512 px, uploaded with no retries.
- There is no URL fetching, SVG, clipboard or folder scanning.

## Keyboard and layout

The editor's content scrolls inside a dialog bounded by the window, with Refresh, Close and Save fixed below it, so every control stays reachable in the smallest window at large text sizes. Tab and Shift+Tab move between the editor's own controls only, wrapping from the last enabled control to the first and back. Escape closes an open confirmation first, then the editor.

## Account switcher

After the server confirms a profile, the account switcher shows the confirmed display name and its initials, for that account only.

The switcher also refreshes its label without opening the editor. It makes one bounded read of the **active** account's own profile in two cases:

- after an account snapshot is accepted, once per account selection, which covers protected activation and restore;
- each time Manage accounts opens.

Only one read runs at a time. Each read is fenced by the exact selection, so A -> B -> A never lets an old read relabel anything. Inactive accounts are never read, and nothing syncs in the background. These names are kept in memory only; nothing goes to configuration or any other store. A removed account's name is forgotten the next time the account list is shown.

## Account pictures

The bottom-left active profile, the account switcher and Manage accounts show each account's own public avatar in place of its initials.

- **Active account only.** The picture is read only for the active account, and only in the same bounded, once-per-selection profile read that keeps the switcher label current, or when the editor reads or saves.
  - The profile response is capped at 64 KiB, with one attempt and an authenticated request.
  - The media fetch uses the existing bounded, authenticated media read, then the shared decoder.
  - The image is reduced to at most 64 px with alpha-weighted averaging and kept in memory only. Nothing is written to disk.
  - If the profile still names the same avatar, nothing is fetched again.
- **Other signed-in accounts** keep a picture only if one was read while they were active, and only in rows that name exactly their local account. Inactive accounts are never fetched.
- **Clearing.** A removed or logged-out account's picture is dropped with it. When the app locks or the account detaches, every picture is dropped from memory and from view at once, and any read in progress is cancelled.
- **Fallback.** While a picture loads, or if it fails, the initials show. A late result from an earlier selection (A -> B -> A) is dropped.
- **No extra actions.** Pictures add no hit target and no action; every account action stays where it was, behind the same app-password and confirmation flows.

## Fences

- Every read, choice and save captures four things:
  - the exact account selection;
  - a cancellation token;
  - the editor's open and operation generations;
  - the picker generation.
- Closing the editor, switching accounts (including A -> B -> A), locking, removal or detach does three things:
  - cancels the token;
  - stops any later step: an upload that finished never becomes the avatar;
  - clears drafts, previews and errors synchronously.
- Late results are dropped.

## Code

| Part | File |
| --- | --- |
| Core: read, set name, set or remove avatar | `crates/morpheus-matrix/src/own_profile.rs` |
| Controller | `crates/morpheus-app/src/account_identity.rs` |
| Editor and entry points | `crates/morpheus-app/ui/app-window.slint` |
| Shared chooser | `room_settings::choose_avatar_file` |
| Image preparation | `image_previews/avatar.rs` (`upload_copy`) |
| Opt-in regression (not run) | `crates/morpheus-matrix/tests/own_profile_local.rs` |
| UI regression (phase 28, not run) | `crates/morpheus-app/src/account_access_ui_tests.rs` |

## Known limits

- **No device or remote preview of other aliases.** The editor edits the current account only.
- **Avatar placement.** Pictures only fill the existing badges (and one 24 px badge in each Manage accounts row); badge positions and sizes are unchanged. Root owns that geometry.
- **Version lookup.** Removing a field still asks the SDK which endpoint the server supports, using its own (cached) versions request.
- **Shared cancellation token type.** The cancellation token is the existing `RoomSettingsCancellation`, reused as a generic token.
- **Shared chooser and decoder.** Only one image chooser and one decoder run at a time, process-wide; they are shared with the room settings form. When Room settings holds either one, the editor says so ("The image picker is in use by Room settings...") instead of waiting, and Close stays available.
- **Error wording.** Some decoder error texts are worded generically ("Choose an image...").

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
- **Save to public profile** sends each changed field as its own request:
  - Each outcome is reported separately. For example: "Display name saved. The avatar was not confirmed; Save again to retry only the avatar."
  - The chosen image is kept after a failed upload, so a retry doesn't need the file chosen again.
  - A request that already reached the server is never described as cancelled or undone. After a timeout, **Refresh** shows what the server holds.
- Display names are trimmed and must be 1 to 256 characters and at most 1 KiB, on a single line, without control or direction-override characters. Clearing a name isn't supported.
- Images go through the existing room-avatar path:
  - one guarded native chooser, reading a regular file of at most 8 MiB;
  - the pinned PNG or baseline-JPEG decoder, at most 4096 px per side and 4 megapixels;
  - a stripped PNG of at most 512 px, uploaded with no retries.
- There is no URL fetching, SVG, clipboard or folder scanning.

## Account switcher

After the server confirms a profile, the account switcher shows the confirmed display name and its initials, for that account only. These names are kept in memory only; nothing goes to configuration or any other store. A removed account's name is forgotten the next time the account list is shown.

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
- **No profile read on switch.** The switcher shows a confirmed name only after the editor has read or saved in this session. Otherwise it shows the user ID's localpart, as before.
- **Shared cancellation token type.** The cancellation token is the existing `RoomSettingsCancellation`, reused as a generic token.
- **Shared chooser and decoder.** Only one image chooser and one decoder run at a time, process-wide; they are shared with the room settings form.
- **Error wording.** Some decoder error texts are worded generically ("Choose an image...").

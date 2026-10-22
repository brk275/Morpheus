# Room media browser

Status: **in development**. Source only: nothing of it has been built or run
yet. Hosted core and native UI checks, and real encrypted-room acceptance,
are pending.

The media browser lists the images, videos, audio and files sent in the
visible room, newest first. Open it with the media button in the room header;
it is the Media tab of the details panel. The core is
`crates/morpheus-matrix/src/room_media_browser.rs`, the app controller is
`crates/morpheus-app/src/room_media_browser.rs` and the pane is
`crates/morpheus-app/ui/room-media.slint`.

## What it reads

- Opening the tab reads the newest part of the room's history; **Load older**
  reads further back. No history load starts while the tab is closed, and
  nothing is read for a room you did not open it in.
- Each load sends at most 6 history requests for up to 50 events each to your own
  homeserver, and stops after the request in which it found 40 items. Each
  history request is sent once (no retry or token-refresh replay), with a 20 s timeout and a 4 MiB answer
  limit; the whole load is bounded at 60 s. One list keeps at most 1,000 items
  and looks at most 6,000 events; the last request asks for only the remaining
  budget. Failed loads also count toward the read budget, because they may
  have read earlier pages before failing. Close and reopen it to start from
  the newest again.
- In a room this device knows to be unencrypted, the homeserver is asked only
  for messages that carry a media address. Otherwise messages and encrypted
  messages are read and decrypted with this device's keys, as the timeline
  does.
- Of each media message only what the sender declared is kept: the file name
  (as a display-only basename), the type, the size, the sender, the time and
  its thread. Captions, message text and media addresses are never kept or
  shown, and nothing is downloaded for a row. Rows live in memory only while
  the tab is open for its account and room.

## What it says

- The list says how many messages it has read so far. It says that it reached
  the start only when the homeserver reported the start of the history this
  account can see. When paging cannot continue, it says so instead.
- An encrypted message this device cannot decrypt is counted, never shown:
  whether it held media is unknown, and the list says that any media in such
  messages is not listed. Key recovery and backup work as for the timeline.
- Messages too large or malformed to read are counted too: the list says that
  any media in them is not listed. A page longer than requested stops the
  list as incomplete; it never skips past an unread tail and claims the
  history was complete.
- Search and the kind filter look at loaded items only, and say so. Load older
  media to search further back.
- A message whose encryption does not establish its sender is marked, as in
  the timeline.

## Actions

Every action acts on one listed row and only when you choose it.

- **Open** shows the message in the conversation (or its thread) through the
  same protected navigation as search results; an older message opens the
  conversation around it.
- **View** opens an image in the existing image viewer, which reads it again
  under its own limits. In an encrypted room it follows the Appearance setting
  for image previews in encrypted rooms.
- **Save…** uses the existing attachment save, which asks where to save it.

## Fences

- A load runs under the exact account selection that was shown when it
  started, and its result is shown only while that account, the room, the tab
  and the navigation are still current and no account or security step has
  begun since (even one that already ended). A security step in progress
  refuses a load until it ends; **Try again** then reads again.
- Closing the tab, changing room or account, a lock or a detach clears every
  row at once and cancels the load. A request already with the homeserver is
  dropped unread.
- The manager's lock is held only to pick the account's client, never across a
  request.

## Limits

- Only messages are listed: stickers, edits, deleted messages and media in
  state events (such as the room avatar) are not.
- In a room known to be unencrypted, an encrypted attachment sent without a
  plain media address is not returned by the homeserver's filter.
- History the server does not show this account (for example before it
  joined, depending on the room's history visibility) cannot be listed.
- Opening a message in a thread switches the details panel to the thread,
  which closes the media list; open it again to continue.
- Video and audio have no preview here; use Save or Open.
- The history-request limit excludes the SDK's existing server-version
  lookup and missing-key backup work. A missing-key backup already scheduled
  by the SDK can finish after the tab closes; closing stops this browser's
  history load and clears its rows.

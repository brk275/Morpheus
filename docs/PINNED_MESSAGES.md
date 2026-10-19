# Pinned messages

Status: **source only**. This has not been compiled, tested or run.

Morpheus reads and changes the Matrix `m.room.pinned_events` state of the
visible room. The core is `crates/morpheus-matrix/src/pinned_messages.rs`. The
app controller is `crates/morpheus-app/src/pinned_messages/`, and the pane is
`crates/morpheus-app/ui/pinned-messages.slint`.

## Reading

- The list, this account's membership, the room's create event and its power
  levels come from server GETs, never from the SDK's cached state. Sliding sync
  does not request pinned state, so the local copy can be stale or missing.
- Each GET is a single attempt with a 10 s timeout. The four run concurrently
  inside a 30 s bound, and a state response is capped at 160 KiB.
- Permission comes from those authoritative power levels and the room
  version's rules, and fails closed for an unknown room version. Ruma treats a
  room with no power-levels event as `state_default` 50, so only creators can
  change pins there.
- The pane shows the newest 50 pins. They are fetched through the account's
  SDK client (`Room::event`, which decrypts), at most 4 at a time. Each fetch
  is one attempt with an 8 s timeout, the whole load is bounded at 40 s, and
  each event is capped at 512 KiB.
- An event that fails or times out becomes an "unavailable" row. Redacted,
  undecryptable and unsupported events (state, polls, pinned edits) each get
  their own row, so the list never looks shorter than it is.
- Rows show the sender (cached display name or user ID), time, and text
  bounded to 500 then 300 characters. Control, bidirectional-override and
  zero-width characters are removed. An attachment is described by kind and a
  display-only basename; nothing is downloaded and no preview is shown.
- An edited message shows its original text with a note. A message whose
  encryption does not tie it to its sender has a warning: sent without
  encryption in an encrypted room, from an unsigned or unknown device, after
  an identity change, or with a mismatched sender. An unverified identity alone
  carries no warning, matching the timeline.

## Changing

- Pin and Unpin are explicit actions: the timeline row action, or Unpin in the
  pane. Only one change runs at a time.
- Each change rereads the list first. It edits only the `pinned` array. Other
  content fields and list entries (invalid entries and duplicates included) are
  kept byte for byte. Unpinning removes every occurrence of the event.
- Nothing is sent if the list is already in the requested state. A list whose
  `pinned` value is not an array, or that has more than 2048 entries, is read
  only.
- Adding a pin is refused at 256 pins, and a body over 60 KiB is refused.
  Removing a pin is always allowed.
- The write is one unretried request. After it, the list is reread. Matrix
  state has no compare-and-swap, so a concurrent change between that reread
  and the write can still be overwritten.
- A failed or timed-out write is reported with the warning that it may still
  have applied, and the list is reread. Morpheus never retries a write on its
  own.

## Fencing and privacy

- One scope exists per account selection and room. Results publish only when
  the scope token is current, the account selection is current (A-B-A safe),
  and the window still shows that account and room.
- When the window's visible account or room actually changes (a snapshot
  selection change, an account context clear or an account detach), all pinned
  rows, flags, filter text and notices are cleared synchronously on the UI
  thread, and the scope's requests and sync watcher are cancelled. A switch
  that is blocked or cancelled keeps or rebuilds the scope for the room still
  shown.
- Decrypted rows are kept only in memory while the pane is open. Closing the
  pane drops them, even mid-switch. Nothing is logged or written to disk.
- A sync batch that touches the pinned list, the power levels or this
  account's own membership, or that has a timeline gap, rereads the list
  once per 750 ms burst. Sliding-sync room lists do not request the pinned
  list, so a gap may hide a change. Any redaction rereads the list, because
  redacting the list itself empties it. An open pane refetches rows only for
  newly shown pins, a redaction of a shown pin, or a gap.
- A read that began before a change that may have applied is discarded and
  rerun, so it cannot bring back an unpinned row.
- The pane's rows update in place, so keyboard focus stays on a row that is
  still listed. After Unpin from the pane removes the focused row, focus moves
  to the search field. Only an explicit open (header button, Pins tab,
  `show-pinned-panel()`) moves focus into the pane.

## Actions

- The timeline row action and the thread reply button show once the list is
  known, for anyone allowed to change it. Without that permission they appear
  only as disabled markers on pinned messages. Both pass the account and room
  they showed, and Rust checks them against the current scope.

## Integration dependency

Opening a pinned message calls the window callback
`open-conversation(account, room, event, thread)`, declared exactly as on the
messaging branch. That branch provides the handler. Without it, Open does
nothing.

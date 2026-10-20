# Typing, read markers and read receipts

Morpheus shows who is typing in the open room and who has read a message, and
can send your own typing notices and read receipts. Everything goes through the
pinned Matrix SDK on the active account's own client and sync; Morpheus builds
no requests of its own.

## What you see

- **Typing footer.** Under the conversation, "Alice is typing…", "Alice and
  Bob are typing…" or "Alice, Bob and 4 others are typing…". Names are the
  room's display names as the server reports them, sanitized and shortened;
  they are not verified identities. At most 32 people are kept from one update
  and at most 4 names are looked up. The footer clears on any account, room or
  page change, and an update not renewed within 30 seconds is dropped.
- **Seen by** (the ✓✓ action on a message, or on a thread reply). Lists
  people whose *public* read receipt is on that message or on a later one in
  the same scope, from this device's store, at most 64 events and 50 people.
  For a room message only unthreaded and main-timeline receipts count; for a
  thread reply only that thread's receipts and unthreaded ones on the thread's
  own events count, so a thread receipt never claims a room message was seen.
  People who read privately, or whose receipts have not synced, are not listed,
  and the view says so. Your own receipt is never listed.

## What you send

All three choices are off by default (`[activity]` in `config.toml`, or
**Settings › Typing and read receipts**).

- **Send typing indicators.** While a composer has focus, holds text and was
  edited in the last 5 seconds, the room is told you are typing; the SDK sends
  at most one notice per 3 seconds and each lasts 4. It stops on idle, blur
  (including the window becoming inactive), an empty composer, sending, a room,
  thread, page or account change, account lock, and turning the setting off.
  Blur, an emptied or sent composer, another or a closed thread, a hidden
  conversation, a room or account change and turning the setting off also
  drop a notice not yet sent, and so does its own idle deadline: a notice
  still waiting (for example on the account lock) when the last edit is 5
  seconds old never goes out. One
  worker sends them: keystrokes only update its state. A stop goes only to the
  account and room the notice was sent for. It is not sent while account
  access is locked or once that account is no longer selected; the server then
  ends the notice after its 4 seconds.
- **Send public read receipts.** When on, receipts are `m.read`, visible to the
  room. When off, every receipt Morpheus sends is `m.read.private`.
- **Mark messages read as I view them.** This is the only way Morpheus moves
  your read position on its own. A receipt moves only to the event a
  live conversation (or the open thread) shows as its last row while scrolled
  to the bottom, after it stayed there for 1.5 seconds, while the window is
  shown, not minimized and a composer has keyboard focus. Merely opening or
  caching a room, scrolling through older history and a view focused on an
  older event never move it. At most one request per view at a time and one
  every 3 seconds; the SDK never moves a receipt backwards. Thread views send
  thread receipts; the conversation sends main-timeline receipts. A newer
  bottom message, leaving the bottom, losing focus, hiding the conversation,
  another room, thread or account, and changing either receipt setting
  withdraw a receipt that is still waiting: nothing is sent for it. A receipt
  already handed to the Matrix SDK cannot be recalled. A withdrawn receipt
  still counts for the 3-second interval, so refocusing never sends the same
  receipt again sooner.
- **Mark as read** (room menu, or Menu / Shift+F10 on a room row). Moves the
  fully-read marker and a read receipt (private unless public receipts are on)
  to the room's latest event. Turning public read receipts off, or quitting,
  withdraws it while it is still waiting; leaving the room does not. Unread
  counts change when sync brings the server's result; Morpheus never clears a
  badge on its own.

## Limits

- Thread replies newer than the room's main-timeline receipt can stay unread
  after Mark as read; open the thread to read them.
- Seen by covers only loaded events and receipts this device has synced.
- Settings are global; per-room and per-account overrides do not exist.

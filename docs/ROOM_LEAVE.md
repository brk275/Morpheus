# Leaving a room

**Leave this room…** in the room header opens a dialog for the room you are
in. It names the room by its name on this device and by its Matrix room ID,
says what leaving does, and leaves only after you choose **Leave**. Cancel
has focus first; Escape and Close can withdraw the dialog while it is busy.
Withdrawing before admission sends nothing. After admission the result may
be unknown: closing never claims to undo a request already handed off.

## What leaving does

- You stop receiving the room's messages and calls on every device of this
  account. Its members can see that you left, and your messages stay in the
  room.
- To come back you may need a new invitation, unless anyone can join.
- Only the room you chose is left. When it replaced an earlier room, or was
  replaced by a newer one, that other room stays as it is. Leaving a Space
  does not leave the rooms in it.
- When this device knows you are the room's only member, the dialog says
  that nobody may be able to invite you back.

The room's name is other people's text: it is shown as plain, shortened text
and is not a verified identity. The Matrix room ID is the exact room that is
left.

## How the leave is sent

- One leave request for that room ID alone. Morpheus does not retry it and
  does not renew an expired sign-in to send it again; an expired sign-in is
  reported, and the homeserver did not apply that request. A dialog sends at
  most one leave; after a definite refusal, open it again to try again.
- The dialog belongs to the account it was opened for. It closes when that
  account changes, signs out or is detached; a request it had not yet sent
  is then never sent.
- Leaving is refused while a call of this account is starting, joined,
  reconnecting or ending in that room, or while a call rings there: leave or
  answer the call first. Leaving a room never ends a call. It is also refused
  while an account or security step is in progress. The exact selection,
  navigation, security generation and call state are checked at admission,
  after client acquisition, which waits at most ten seconds.
- If no answer comes back, or the dialog closes while the request is on its
  way, the result is unknown: you may or may not have left. It is not sent
  again in this app session; closing or switching away and back does not
  discard that exclusion. A new persisted sync update showing that this
  account left the exact room settles it. An HTTP acknowledgment, a missing
  room row, a local membership mark or a timer does not settle it.

At most 64 unresolved leaves are retained across accounts in this process.
When full, another leave is refused until existing ones settle; unresolved
records are never evicted to make room. Complete local account removal or a
successful logout forgets that account's records. A fresh saved-account
listing that no longer contains it also retires records captured before that
listing; a switch, a lock, a closed dialog or a partial removal keeps them.
These in-memory exclusions are not a persistent request journal: after a
restart, check the latest room membership before choosing Leave again.

## When the room disappears

Nothing changes on this device when the homeserver accepts the leave. The
room leaves your list when this device's sync shows that you left; until
then it stays, and the dialog says the leave was accepted and is waiting for
sync.

## Limits

- What the dialog says about earlier and newer rooms, Spaces, direct chats
  and members comes from this device's copy of the room as of the last sync.
- Messages are not deleted, and other members' copies of the room are not
  changed.

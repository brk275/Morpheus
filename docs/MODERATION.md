# Member moderation

The member list offers invite, kick, ban, unban and power-level changes, and a
message's actions offer redaction. Each belongs to the account and room that
were shown when you chose it.

## How an action is sent

- Morpheus first reads the room's current power levels from the homeserver
  (and, for a redaction, who sent the message) and checks that this account
  may act. Kicking or banning needs a higher level than the member's own; a
  room creator in rooms that give creators unlimited power cannot be kicked,
  banned or changed; a new power level may not exceed your own.
- Right before sending, it checks again that the same account is still
  selected, that you are still in the room and that no account or security
  step is in progress. If any check fails, nothing is sent.
- An action ends with what it was chosen in: closing the redaction dialog,
  switching account or room (even to return to the same room), a newer action
  of the same kind or an account or security step that began after it, even
  one that has already finished, stops it. Before it is sent this means
  nothing is sent; once sent, it is not sent again and its result is unknown.
- Each action is one request. Morpheus does not retry it and does not renew
  an expired sign-in to send it again; it waits a bounded time and accepts
  only a small answer.

## What the result means

- **Done**: the homeserver applied it.
- **Refused or limited**: the homeserver answered that it did not apply it.
- **Unknown**: no answer came back, or the account changed while the request
  was on its way. The action may or may not have been applied, and it is not
  sent again. Refresh the member list to see the room's state.

## Power levels

A power-level change rewrites only that member's entry. Every other setting,
including fields Morpheus does not know and the exact spelling of numbers, is
kept as stored. The power levels are read again immediately before the change;
if they differ in any way, nothing is sent and you are asked to refresh. Matrix
has no conditional write, so a change made elsewhere in the moment between that
read and the write can still be overwritten. After the write the power levels
are read back; if they do not show the change yet, Morpheus says so.

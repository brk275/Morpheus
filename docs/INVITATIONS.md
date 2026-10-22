# Room and Space invitations

Invitations for the account on screen appear in two places that share one
answer path:

- **Home cards.** The sidebar shows the first three invitations as cards with
  Accept and Decline, as before. Accepting from a card opens the room or Space.
  The section header shows how many there are and opens the center.
- **The invitations center.** "All N" (or "Manage") in that header, or
  Settings search ("invitations", "invited", "spam invites"), opens a dialog
  that lists every invitation of the account. Settings search only opens it on
  Home and focuses its search field; nothing is accepted or declined.

## What the center shows

The list is read from the invite state the Matrix SDK stored from sync; no
request is sent to read it, and only the account on screen is read. Each entry
shows only what that state supplied: the room's name (else its alias, else
"Unnamed room"), whether it is a room, a Space or a direct message, who invited
the account, its alias and topic, and whether it is encrypted when the state
says so. An inviter's display name is shown only beside the Matrix ID that sent
the invitation, so a name cannot pose as someone else. An invitation from a
user on the account's ignore list says so ("whom you ignore") and can still be
declined; nothing is answered for it automatically. The list is read strictly
once per listing; when it cannot be read, no invitation is marked (its banner
is held back, as described in [notifications](NOTIFICATIONS.md)). All text is shown as
plain text with control and direction characters removed. At most 500
invitations are read, the first ones by name, and the count says when there
are more. The list is virtualized and searchable by name, alias and inviter.

Keyboard: the search field has focus when the center opens; the arrow keys,
Page Up, Page Down, Home and End choose an invitation, and Tab reaches Accept
and Decline for the chosen one, after the rows' own buttons. The choice follows
its invitation when the list is read again; if that invitation is gone, the one
now in its place is chosen. Rows that stay update in place, so keyboard focus
stays on them; when focus was on a row that the new list removed, it moves to
the search field rather than being lost. The center is modal: its scrim takes clicks and scrolling, no key reaches the
window's shortcuts behind it, and shortcuts pause while it is open. Tab stays
inside the center,
Escape or × closes it and focus returns to the Home entry. It opens only
under the same modal admission as a Matrix link (otherwise the attempt is
refused, never kept for later), and closes by itself, its
rows cleared, when Home is left, another account is shown, the session ends,
or anything that withdraws a link takes over: sign-in, the account menu,
Manage accounts, the app password, a pending switch, security work, a
recovery key or a verification, your profile, People, member details, the
Space explorer or a Space's rooms, room addresses, room notifications, Seen
by, the quick switcher, room actions or administration, a member action's
confirmation, the capture picker, leaving a call, the image preview, a poll,
the thread browser, identity change details, the Leave dialog, the room
header's More menu or a link card. Opening it first withdraws a call bar
click still waiting, a shown link, a joined link's message still to come, an
accepted invitation's room still to open, a jump to the first unread message
in progress, identity change details and the thread browser. While it is open
a link does nothing, no link navigates, a jump waits and the call bar is
hidden. The thread browser, identity change details, a member's details, the
More menu and the Leave dialog each exclude the center both ways: while one
is open the center refuses to open, while the center is open none of them
opens, and a refused attempt withdraws nothing.
Settings search refuses the same way before it leaves Settings. A Home card's
Accept or Decline and the center's Open do nothing while such a surface is
open; an Accept that opens, and Open, first end the same waiting work.

## Answering

Nothing is joined or declined without a deliberate Accept or Decline.

- Each answer is one join or leave request for exactly one invitation, bound to
  the account selection that was on screen when it was chosen, with a 30 s
  bound. A row answers only while it is still listed with the generation it
  was shown with, so a row from an older listing, or from before a switch from
  A to B and back to A, answers nothing.
- One answer per invitation until its effect shows here. Each answer holds a
  guard for its account and room (never the room alone), recorded with the
  exact answer it belongs to. While the guard stands, no other answer for that
  account's room is sent from the row, the action bar, a Home card or a bulk
  decline, also after a refresh, after closing and reopening the center, and
  after a switch from A to B and back to A. The row says which stage it is in:
  "Joining…" or "Declining…" while the request is out; "Answered; waiting for
  sync…" once the homeserver confirmed it but this device does not show it
  yet; and "Not confirmed; it may still reach the homeserver, so it is not
  sent again." when no answer came back (no reply, a server error, the 30 s
  bound, or an account change while it was out). Only a read of that
  account's own local store showing the membership the answer asked for
  (joined after Accept, left after Decline) retires the guard, on the next
  Refresh or a Check and open. An answer that was not applied at all (nothing
  sent, or refused by the homeserver) frees the room for a new deliberate
  attempt. A room missing from a listing proves nothing, since a listing is
  bounded and another account's never names it, and no timer retires a
  guard. At most 64 answers are held unresolved; past that a new answer is
  refused with a notice rather than an unresolved one forgotten.
- An answer is sent only while what it was chosen under still holds: the
  session is live, no security step is in progress or has begun since it
  was chosen, and, for the center, the center has not been closed since.
  Otherwise nothing is sent, the room is free for a new deliberate answer,
  and the notice says why. Morpheus waits at most 10 s for another account
  step (an account switch, say) before sending, and at most 10 s for the
  account and the invitation's own details; closing the center ends that
  wait too. It does not hold another step up while the answer's own request
  and sync wait run. Past either wait nothing is sent and the notice says so.
  Right before sending, the room must still be an invitation here: if sync
  meanwhile shows it declined, withdrawn or joined, nothing is sent and the
  notice says it is no longer an invitation. Accepting from a Home card opens
  the room only if you have not moved elsewhere since choosing it, no
  security step has begun since and no dialog or other protected surface was
  opened since (opening one, such as the invitations center or the account
  menu, ends that open for good, even if it is closed again at once);
  recording an answer never changes where you are, and an open that ended
  changes nothing about the answer. Those conditions are checked again at the
  moment the room or Space is selected and when it is shown, so a security
  step started (or even finished) in between stops the open there and the
  previous room comes back. Only the last such open is kept.
- After every answer the list is read again, so it shows the real state. An
  answer shows "Joined." (with Open) or "Declined." only once the homeserver
  confirmed it and sync brought exactly that membership here. Confirmed but
  not here yet, a join offers a manual **Check and open**, which opens the
  room only if this device then has it joined and otherwise says so; nothing
  opens or retries by itself. **Open** and **Check and open** open the room
  the same way as a Home card's accept: for the account, security state and
  place you clicked from, in the Space that lists the room or in Home, never
  through whatever is selected later. Otherwise the row says, in fixed wording, that
  the homeserver refused it or it was not sent and nothing changed, that it
  was no longer an invitation and nothing was sent, that the account changed
  before the answer was sent, or that no answer came back and it may still
  apply. An answer already sent is never reported as undone; homeserver text
  is never shown. Open after accepting a Space selects the Space, as accepting
  it from Home does; whether it is a Space is read from the invitation when
  the answer is admitted, not from a row a later refresh may have removed. A
  room opens as a conversation. Once an answer is confirmed, the invitation's
  desktop banners are withdrawn where the platform allows.
- **Decline all shown** prepares a decline of the invitations the search
  shows, at most 25 and none with an unresolved answer, and asks for a
  separate confirmation naming how many. An unanswered confirmation expires
  after a minute, and a late one declines nothing. Each one is then declined
  on its own, after checking again that its row is still listed and the
  account selection is current; closing the center or a switch stops the
  rest, reaching the unresolved-answer bound stops it too, and requests
  already sent are not recalled. The summary counts the declined, the
  unconfirmed and the not applied.
- The center's body scrolls once the list is at its smallest. At the smallest
  window and 200% text a long notice, the bulk confirmation's text and every
  action stay whole and reachable: focusing an action scrolls it into view,
  and the confirmation's buttons bring its text with them when it fits.

Each answer is one typed join or leave request for exactly the invitation's
room, on the account's client (no redirects, TLS 1.2 or later), with no SDK
retry and a 30 s bound. A join counts as confirmed only when the homeserver's
answer names exactly that room; an answer naming any other room is reported
as not confirmed, never as a join, and nothing is marked or opened for it.
Declining never leaves any other room: the SDK's own
leave would also leave rooms the invitation names as predecessors, including
ones already joined. The account's sync then brings the new membership:
Morpheus waits for it at most 30 s and opens an accepted room only once it
is joined here. Each request is sent at most once: no HTTP retry, and no
automatic refresh of an expired access token followed by a resend (an
expired token is a refusal that needs a new deliberate attempt), with a
64 KiB response cap.
Both request options are the vendored SDK's; retry 0 alone would not prevent
a resend after a token refresh. Accepting an invitation keeps
the pinned policy of not receiving encrypted-history bundles; see
[invitations and encrypted history](INVITATION_HISTORY_POLICY.md). Desktop
banners for new invitations keep the notification privacy settings; see
[notifications](NOTIFICATIONS.md).

Once the homeserver confirms the join of a direct-message invitation,
Morpheus lists the room under its inviter in the account's `m.direct` data,
as the SDK's own join does, with the same lossless writer People uses. The
stored list is read fresh and merged as raw JSON, so every other person,
unknown field and number keeps its exact text. It is read again right before
the single write, and written only if that second read is byte for byte the
first. Content it cannot change without loss (not an object, a name given
twice, or the inviter's entry not a list of room IDs) is left alone. Each
request is sent at most once, within 30 s in all and alongside the sync
wait, and only while the same account selection is current, the session is
live and no security step has begun; closing the center does not stop it.
One listing runs at a time in the app, People's included: each waits its
turn before its first read (at most a minute, and not once its own
conditions end) and keeps it until its write is answered, so two of them
never both read the same list and then write over each other's room. The
notice says when the room was not marked as a direct message and why, or
that marking it got no answer and may have applied; it is never sent again
by itself. Such a possibly applied marking is kept while Morpheus runs,
whatever you closed or switched meanwhile, and the center names it again
whenever it opens for that account; no later sync or read counts as its
answer. Removing the account from this app completely, or logging it out,
forgets its unresolved answers and markings and every outcome shown for it,
in the center or on the Home cards, also when the center was never opened:
its notice, the room that notice opens, a Home card's error and its rows'
errors. Each shown outcome names the account it was written for, so an
outcome another account showed meanwhile stays. An answer of that account
that finishes afterwards changes nothing. A switch to another account, or a
removal that did not complete, forgets nothing. An unconfirmed join is never
marked. Whatever happens to the listing, the confirmed join stays confirmed.
Matrix account data has no conditional write, so a change made elsewhere
between that second read and the write can still be overwritten.

These behaviors were written against the pinned SDK sources and have not been
run; the center, its keyboard path and answers against a real homeserver
remain to be accepted.

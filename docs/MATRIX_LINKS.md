# Matrix links in messages

Status: **in development**. Source only: nothing of it has been built or run
yet. Hosted core and native UI checks, and acceptance against real
homeservers, are pending.

A Matrix link in a message body, a thread root or a thread reply opens
inside Morpheus. It is never sent to matrix.to, and it never joins, creates
a direct message or sends anything just because it was clicked.

The core is `crates/morpheus-matrix/src/matrix_links.rs`. The app controller
is `crates/morpheus-app/src/matrix_links.rs`, and the card is
`crates/morpheus-app/ui/matrix-link.slint`.

## Which links

Only two exact shapes count as Matrix links:

- `https://matrix.to/#/` followed by a room ID, a room address, a room and a
  message, or a person's ID. Only `via` may follow.
- A `matrix:` URI (`matrix:roomid/…`, `matrix:r/…`, `matrix:u/…`, with
  `/e/…` for a message). It must have no `//` host, user name, port or `#`
  part. Its `action` (join, chat) is ignored.

Each ID must be at most 255 bytes, a message ID must start with `$`, and a
link may name at most 10 routing servers. The whole link must be at most 2048
bytes without control characters. Lookalike hosts (`matrix.to.example`,
`www.matrix.to`, `http://matrix.to`) are not Matrix links. Any other link
keeps the usual **Open external link?** confirmation.

Messages could not link `matrix:` URIs before; now a valid one shows as a
link, and its click only reaches Morpheus.

## What a click does

The link opens for the account on screen when it was clicked. It never
switches accounts.

- **A room you have joined** opens like a room-list click. A message link
  scrolls to the message, or shows the conversation around it with Return to
  live. When this device's event cache knows the message is in a thread, the
  thread opens on it. Drafts are kept. An edit being typed in another room
  stops the open, as for search results.
- **A Space you have joined** opens like a sidebar click.
- **Anything else** shows a small card with the room ID or address and what
  this device knows:
  - not joined, or left: **Join room** (or **Join Space**);
  - invited: answer the invitation in the room list;
  - asked to join (knock), or banned: nothing to do from here.
- **A person's link** shows the ID only. People are not opened from links in
  this build yet. Nothing is sent.
- **A matrix.to link Morpheus cannot open** (for example a group link)
  offers **Open in browser…**, which goes through the same external-link
  confirmation. A `matrix:` URI Morpheus cannot open offers nothing and is
  never handed to another app.

Joining is always its own click. It is the same bounded join as **Join a
room**:

- one request for this account, with the link's routes;
- refused while a security step runs, or if one began since the click;
- never sent again after no reply. An unknown outcome is remembered for the
  session, whatever the card does.

Once sync shows the room joined, it opens, and a message link then shows its
message in that room. The message is shown only while the join's own open
still is what you see: if you chose another room or Space since (the same
room again included), switched accounts, or a security step began since the
click, nothing more is shown, though the join is still reported as joined.
That is checked again at every later step of showing the message: each wait
for its row to arrive, the conversation opened around an older message, its
first rows and the scroll to it. Anything else taking over also ends it at
once, even after the card closed, and closing that again never brings it
back: a newer link, an account switch, sign-in or the session ending, another
page (Settings, the call page, the room directory), and every account,
security or modal surface listed under Closing and guards. If sync has not
brought the room yet, the card says so.

## Reading a link

- A room ID is read from this device only.
- A room address costs one request to your own homeserver, which looks it
  up. That request:
  - carries no access token (the endpoint takes none);
  - has no retry and no automatic sign-in renewal and resend;
  - waits at most 15 s for a reply of at most 16 KiB.

  The servers it names are ignored; a join uses the link's own routes. Setup
  the Matrix SDK does before that request (such as asking for the server's
  versions) keeps its own behavior.
- A message's thread comes from this device's event cache only.

While a read takes longer than a moment, the card says so. **Cancel** (or
Escape) withdraws it. The card appears then only if the link still is where
you left it: if you chose another room or Space, switched accounts or another
surface took over first, the read is withdrawn, and the card never appears or
takes focus.

If an address cannot be read, the card says why in a fixed sentence: not
known to your homeserver, refused, limited, or no answer. Join stays
available where the join itself may still find the room.

## Closing and guards

- Close and Escape withdraw the link before the card is hidden:
  - its read is cancelled;
  - a join not yet sent is not sent;
  - a join already sent opens nothing more, and its outcome is kept.

  Focus then goes back to the composer.
- The same withdrawal happens without moving focus when anything else takes
  over, at that surface's own entry:
  - a newer link, an account switch or lock, sign-in, the session ending;
  - another page: Settings, the call page or the room directory;
  - the account menu, Manage accounts, account access, a pending switch;
  - security work, a recovery key or a verification;
  - your profile, People, member details, the Space explorer, Space rooms,
    room addresses, room notifications or Seen by;
  - the quick switcher, room actions, room administration (details,
    permissions, redaction), a member action's confirmation, the capture
    picker, leaving a call, the image preview, a poll or the invitations
    center.
- Nothing a link started navigates after it was withdrawn, or after you went
  elsewhere. If the card is up and something else navigated meanwhile, the
  card asks before opening (**Open**).
- A link does nothing while another dialog is open.
- While the card is open:
  - shortcuts pause and mention popups close;
  - the scrim takes clicks and the wheel;
  - Tab stays on the card's buttons.
- The card scrolls its text above a fixed footer, so its buttons stay
  reachable at large text sizes.

## Privacy

Links, IDs and names are shown as bounded plain text, with control and
direction characters removed. Routing servers are counted, not listed.
Nothing is logged or stored; the card's values are cleared when it closes.

## Limits

- Only links in message bodies, thread roots and thread replies. Room topics
  and plain-text URLs that are not rendered as links are unchanged.
- Membership comes from this device's store and can lag sync.
- An address lookup is a point-in-time answer. A join by address is looked
  up again by the server.
- A thread is opened on the message only when this device's event cache
  already has it. Otherwise the conversation view around the message shows
  its thread.

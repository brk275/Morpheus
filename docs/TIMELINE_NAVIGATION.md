# Timeline navigation

How Morpheus brings one loaded message into view in a long conversation or
thread, and keeps the reader's place when rows change above it. The code is
`crates/morpheus-app/src/timeline_viewport.rs` and the conversation and thread
scroll glue in `ui/app-window.slint`.

Status: source only. This has not been built, run or measured on a device.

## What uses it

Every route to an exact message goes through the existing navigation seam
(`message_search::open_conversation` and `continue_conversation`): search
hits, notification clicks, Matrix permalinks, pinned messages and the thread
browser. The unread jump uses the same reveal under its own click-time fence.
A message that is already loaded is revealed in place. An older one still
opens the conversation around it, with Return to live, through the existing
bounded context load. A thread target opens its thread, pages back a bounded
number of times, and then reveals the reply itself, never the thread's root.

## Revealing a loaded message

Slint 1.18's ListView draws only the rows on screen. Its content height is
the average height of those rows times the row count, and it has no indexed
scroll for rows of different heights. A reveal (a seek) therefore works from
measured geometry, in two steps:

1. While the row is not drawn, the list moves towards it. A row more than two
   screens away goes straight to its estimated position, which the list then
   lays out starting from exactly that row. A nearer row is approached by
   most of a screen at a time, in the direction of the row reported at the
   top of the viewport.
2. Once the row is drawn, it places itself from its own laid-out position
   and height: a third of the way down the viewport, or 8 px from the top if
   it is taller than that allows, clamped at the ends of the conversation. It
   does this whenever the list lays it out, before that frame is drawn, and
   at least every 16 ms.

The revealed message is highlighted until another jump or a room change. No
row is rebuilt, no history is reloaded, the full loaded history and its
virtualization are kept, and nothing is animated. A thread's replies are all
laid out, so a reply only ever places itself.

## Bounds and failure

- At most one seek per pane (conversation, thread); a newer one replaces it.
- Each seek ends after 2 s of event-loop time or 240 moves, whichever is first.
- The list moves towards an undrawn row at most once every 2 passes of 16 ms.
- Per frame, the work is bounded by the rows on screen. A row's index is
  looked up again in the loaded history only when the rows changed under it.
- A seek that runs out, or whose message is gone, says so instead of counting
  as shown. Search shows "This message could not be shown in the conversation;
  it is shown here." in its panel, or the thread form for a reply. An
  activation shows a session error. The unread jump uses its own notice.
- The person's scrolling ends a seek where they took over. A wheel or drag
  over the rows, or a scrollbar thumb drag, reports as scrolled and ends it
  at once.
- The scrollbar's own wheel and arrow buttons (Fluent, the default style; the
  other styles' bar wheel too) move the list without reporting. So a seek
  also watches what it did not do:
  - in the conversation, the top row it accepted at its start or after its
    own move, jump or a change of rows (the list's own re-anchoring keeps
    that row in place);
  - in the thread, the content-y it last saw or set.
  Any other change ends it there. A conversation seek also never moves its
  drawn row a second time once its placement move was laid out.
- A pane created after its seek began has nothing reported yet. This happens
  when the conversation is shown again and a seek starts before its list
  exists. Until the seek's own first move, the list watches its content-y as
  created (the thread watches its content-y anyway). For the list, its first
  top-row report also ends this, but only if it is still where it was
  created. A scrollbar wheel or arrow in that moment ends the seek too.
- The timeline has no keyboard scrolling of its own to compete with.

## What ends a seek

A seek's start, every move and its end are admitted by Rust first. A seek is
refused at its start, or ends silently before it moves anything, when any of
these no longer holds:

- no protected surface is up. This is `viewport-seek-allowed`: the live
  session, with nothing from the inventory links and the unread jump use
  (`matrix-link-blocked`: Leave's list, the Leave dialog and the header's More
  menu) and no link card open;
- no security step began since the security generation it took. That is
  taken at its click or activation, or for an anchor at its publication;
- the account selection it started under (an A to B to A switch ends it);
- the visible account and room, the live session, and its navigation epoch;
- the conversation's feed generation and its focused or live view, or the
  thread's root and request generation;
- its owner's own conditions. For a search hit or activation this is
  `Opening::admitted`: the owner is current, its NavigationAuthority admits
  it, and no surface it names is up. For an unread jump it is the click-time
  fence.

Anchors have no owner but obey the same policy. Every admitted route that
takes over the window first calls `matrix_links::clear`, before its own
surface flag changes. That call synchronously ends every seek, withdraws the
held open's authority (so an open still waiting for its row starts nothing
later), and stops the thread keeper until the thread's next publication. This
holds even when the surface opens and closes before any change handler runs.
An admitted link click does the same. A link's own ends (Close, its Open,
Join or browser hand-off) and its stale cleanup (its first-show timer or
read finding that something else navigated, its account gone or another
surface up) end only that link: a newer open, its seek and the thread keeper
carry on.
When `viewport-seek-allowed` falls, it retires them again; nothing resumes
when the surface closes. A room change, an account change, removal, lock or
sign-out, leaving a focused view and Return to live also retire seeks
directly. A report from a replaced or ended seek is ignored.

## Keeping the reader's place

The row drawn across the viewport's top reports itself as the reader's anchor
(its event and how far its top is above the viewport). When the conversation's
or thread's rows are published again under the same account, room, feed and
view, and that row moved because rows were inserted or removed above it (Load
earlier, for example), an anchor seek puts its top back where it was drawn.
The first rows of another feed, view or thread start without an anchor.

- In the conversation, height changes from fonts, layout or width need no
  seek: the ListView keeps its first drawn row in place.
- The thread's replies are a plain layout, so there a keeper does it. Only
  scrolling (the person's or a seek's) takes the thread's anchor; a layout
  change does not. When the anchor reply's laid-out position changes (the
  root or an earlier reply reflows, a font or width change), the keeper puts
  it back at its measured offset in one move, clamped at the ends. It acts
  only under the thread's latest publication, its security generation and no
  protected surface. Refused, the place moves with the layout and is taken
  again. It reacts to layouts only, so it never fights the person's scrolling
  and runs no timer.
- New messages below never move the reader. The conversation does not follow
  new messages to the bottom; it never did.
- A thread reader looking at the root, above every reply, has no anchor: the
  root stays where it is and earlier replies appear below it.
- Automatic read receipts keep their meaning: they follow the bottom of the
  live conversation or the open thread, wherever the person or a seek put it.

## Known limits

- When rows are prepended inside the drawn rows (the reader within a page of
  the history's start), the list would draw the shifted rows. The anchor
  seek's jump and placement are made to run in the same layout pass, before
  that frame is drawn; if a layout comes late, the row may show at the
  viewport's top for one 16 ms pass first. Deeper in the history the list
  keeps the rows in place itself. None of this has been observed on screen.
- A new thread opened while another thread was open keeps that panel's
  scroll position until the person scrolls; its anchor is reported again by
  the reply at the top.
- A thread layout change in the middle of a drag gets the keeper's one move
  too; the drag then carries on from there.
- A conversation seek's row that a layout above it moves after its placement
  move (an image loading, for example) is left there: the seek ends with the
  row near its place rather than risk fighting the person.
- A scrollbar wheel or arrow in the same instant as a seek's own move could be
  taken as that move's result once.
- A conversation list that moved by itself between its creation and its first
  report would end a seek that started before it. It would never pull back.
  No such move is known at a new list's starting position.
- Under a protected surface, nothing restores a thread place shifted by a
  layout; its anchor is taken again where it is.
- Exact timing, frame behaviour, screen readers and touch input have not been
  checked on any platform.

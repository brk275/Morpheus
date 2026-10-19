# Room organization

Morpheus organizes rooms with the standard Matrix room tags `m.favourite` and
`m.lowpriority`. They are the account's per-room account data on its
homeserver, so the same Favorites and Low priority rooms appear in other Matrix
clients and on the account's other devices. Morpheus keeps no local copy.

## Sections

The room list shows a **Favorites** section above the rooms and direct
messages and a **Low priority** section below them. Sections only regroup the
rooms the current view already shows: Home and each Space keep their own room
membership, a room in several Spaces appears in each of them, and grouping
direct messages separately or together is unchanged. Each section header says
how many rooms it holds and collapses or expands its section with a click,
Return or Space; the choice lasts for the session and is not saved.

Favorites keep their tag order, and Low priority rooms theirs; rooms without an
order follow by name. **Unread rooms first** reorders only the room and
direct-message lists, never a tagged section. The quick switcher and the
next/previous unread shortcuts follow the displayed order, Favorites first and
Low priority last, including rooms in a collapsed section.

## Changing a room

Right-click a room in the list, press the Menu key or Shift+F10 on a focused
room, or choose the star button in the open room's header (Return, Space, the
Menu key or Shift+F10 open it from the keyboard):

- **Add to Favorites / Remove from Favorites**
- **Mark as low priority / Normal priority**
- **Move up / Move down in Favorites**, for favorites only

Favorites and Low priority are exclusive: adding a room to one removes it from
the other. A room another client tagged both ways is shown in Favorites;
removing it from Favorites leaves it in Low priority. Other tags, such as
personal `u.*` tags or another app's tags, are never changed.

The order is the account's, shared by every view: a Space shows only some
favorites, so moving one places it just before or after its visible neighbour
in the account's whole Favorites order, and favorites the current view hides
keep their place. Morpheus writes one standard `order` value between the new
neighbours when there is room. Otherwise, or when favorites have no order yet,
it renumbers the account's Favorites evenly in the new order and writes only the
rooms whose order changes, at most 100 at a time. A new favorite is placed after
the account's ordered favorites.

Adding a tag a room already has leaves it as stored, order included; for a room
tagged both ways it only removes the other tag. A favorite tag whose stored data
has fields other than `order` was written by another app; Morpheus never
reorders it and reports that the room was left unchanged. Removing a tag
removes it with its data, as the user asked.

## Server state and errors

Each change is one explicit choice for one room, bound to the account that was
visible when it was made; switching accounts discards its result. One change
runs at a time per account, and the organization menus wait for it. Once the
homeserver accepts it, the room list shows the change; until sync returns the
stored tags, for at most 30 seconds, Morpheus applies the accepted change over
its last copy, so a following choice or move starts from it and is decided by
what the homeserver holds. Sync reports changes in order, so an older report
that arrives late (for example, from before a favorite was added and removed
again) never undoes a newer change. Nothing is shown as saved before the
homeserver accepted it.

A refusal made before anything was sent leaves the room where it was with a
fixed message. When a change fails after a request was sent, or was saved only
in part (for example, a favorite added but the low-priority tag not removed, or
a reorder that timed out after some writes), the room also keeps its place
with a fixed message until sync shows what the homeserver stored. Until then a
following choice treats the tags that change may have written as unknown: it
writes a tag it needs and removes one it needs gone even if the last sync
showed otherwise (removing an absent tag changes nothing; an added tag gets the
order the room list shows). A later accepted change decides only the tags it
writes.

Room rows stay in place while only their unread counts or tags change. Menu
choices are fixed when a menu opens and each sends only the action it names.
When sync changes the room's tags while its menu is open, an outdated choice
still applies the change it names: it does nothing when the room already has
that result, and on a room another client tagged both ways it removes the other
tag. When rooms are added, removed or reordered while a menu is open, its row
is replaced and the choice does nothing rather than act on another room. The
room header's menu acts on the room it was opened for.

Only joined rooms can be changed. A room left or re-joined keeps whatever tags
its account data holds on the homeserver.

## Verification

This feature was written against the pinned Matrix SDK and Slint sources and
has not been built or run. Tag writes against real homeservers, interoperability
with other clients' orders, context-menu behavior on each platform, keyboard and
screen-reader use, and long Favorites lists remain to be accepted.

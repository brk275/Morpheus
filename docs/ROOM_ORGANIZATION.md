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

Right-click a room in the list, or open the room and choose the star button in
the room header (Return, Space or the Menu key open the same menu from the
keyboard):

- **Add to Favorites / Remove from Favorites**
- **Mark as low priority / Normal priority**
- **Move up / Move down in Favorites**, for favorites only

Favorites and Low priority are exclusive: adding a room to one removes it from
the other. A room another client tagged both ways is shown in Favorites;
removing it from Favorites leaves it in Low priority. Other tags, such as
personal `u.*` tags or another app's tags, are never changed.

Moving a favorite writes one standard `order` value between its new neighbours
when there is room. Otherwise, or when favorites have no order yet, Morpheus
renumbers the Favorites evenly in the displayed order and writes only the rooms
whose order changes, at most 100 at a time. A new favorite is placed after the
ordered ones.

A tag whose stored data has fields other than `order` was written by another
app; Morpheus never rewrites it and reports that the room was left unchanged.
Removing a tag removes it with its data, as the user asked.

## Server state and errors

Each change is one explicit choice for one room, bound to the account that was
visible when it was made; switching accounts discards its result. While it
waits for the homeserver, that room's menu is disabled. The room list shows
the stored tags as sync returns them: nothing is shown as saved before the
homeserver accepted it, and a failure leaves the room where it was with a fixed
message. A change the homeserver accepted only in part (for example, a
favorite added but the low-priority tag not removed) is reported as such, and
the list shows what was stored.

Only joined rooms can be changed. A room left or re-joined keeps whatever tags
its account data holds on the homeserver.

## Verification

This feature was written against the pinned Matrix SDK and Slint sources and
has not been built or run. Tag writes against real homeservers, interoperability
with other clients' orders, context-menu behavior on each platform, keyboard and
screen-reader use, and long Favorites lists remain to be accepted.

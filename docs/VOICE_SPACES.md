# Voice spaces

Morpheus displays an ordinary joined Matrix room as a voice space when either
the room has the shared `io.github.brk275.morpheus.voice` state event with
`{"v":1,"enabled":true}`, or the current account has marked it locally in
`config.toml`. A local label is scoped to that signed-in account. Removing an
account removes its local labels, while preserving any other account settings.

The room's chat remains available from the call stage's **Return to chat**
control. Selecting a marked room enters its call stage and attempts to join the
standard MatrixRTC room slot. Joining publishes this device's call membership;
it does not ring every room member. The call requires a compatible homeserver
MatrixRTC service and the standard encrypted-room call path. A shared label
does not grant permission to open a call slot or join media.

Use **Mark for me** in the room header when you cannot write room state. Use
**Share with room** only if you have permission to send room state events. A
failed shared write leaves the local option available. Morpheus reads shared
markers in the background and caches the result per account. Background read
failures retain the last known label and retry later.

## Who is here

A voice room in the room list shows a small count of the people in its call,
without joining. Selecting the count (or pressing Enter on it) lists them, with
their display name, Matrix ID, initials and how many devices joined, and
offers **Join quietly**, which uses the same voice-space start as selecting the
room: it publishes this device's membership and rings nobody. A call in another
room must be left first, as before.

The roster comes only from the current account's own sync. The pinned MatrixRTC
core's passive observer validates the room's encrypted call slot and checks
every listed membership against live, decrypted sticky events; nothing joins,
rings, captures, publishes or contacts a media server, and other accounts are
not synced. It is refreshed when memberships change and at least every 30
seconds, so expired and departed members drop out. The count shows `…` while
checking, `3?` when the last check is more than 90 seconds old or failed, and
`–` when no open call could be confirmed or presence is unsupported (legacy
room-state calls, unencrypted rooms, or a homeserver or sync without sticky
call memberships). A missing slot and a failed read look the same, so no zero
is ever claimed for them. The list notes that it may not include everyone.

Only the open voice room, one opened list and the first six voice rooms of the
shown room list are watched (at most eight rooms, one roster per room however
many Spaces list it). Hiding or collapsing the room list releases the listed
rooms; an account change, lock, sign-out or detach drops every roster at once.
Names and IDs are untrusted, sanitized and bounded; avatars are initials only.

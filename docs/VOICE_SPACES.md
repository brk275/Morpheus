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

The participant roster currently appears after joining. The pinned native RTC
API provides a participant list from a joined call handle; Morpheus does not
infer a prejoin roster from incomplete room state.

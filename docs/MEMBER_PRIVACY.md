# Member details and ignoring people

**Details** on a row of a room's member list opens that person's details and
lets you ignore them, or stop ignoring them. **Settings › Security & recovery ›
Ignored users** lists everyone this account ignores (searchable; at most 100
rows at once) and lets you stop ignoring someone.

## What ignoring does

Ignoring is Matrix account data (`m.ignored_user_list`) saved on your
homeserver for this account. Your Matrix apps then hide that person's
messages, invitations and calls, and you get no notifications from them.

It does **not** kick or ban them, does not delete or redact past messages, and
everyone else in the room still sees them. A call already in progress is not
ended; new calls from them are not offered once your apps have synced the
change. Stopping ignoring lets their messages, invitations and calls reach
the account again; messages hidden while they were ignored may not come back
until the homeserver sends them again.

## How a change is made

- The details come from this device's copy of the room's member list; no
  request is made to open them. Names are the room's display names, cleaned of
  control and direction characters and shortened. They are not verified
  identities; the Matrix ID is the stable identifier.
- Choosing **Ignore…** or **Stop ignoring…** first reads the account's list
  from the homeserver (a bounded read). You then confirm the exact Matrix ID.
- Right before the write, Morpheus reads the list again. If it changed
  since you reviewed it (for example from another device), nothing is sent and
  you are asked to review it again. Fields and entries Morpheus does not
  understand are kept exactly as they were.
- The write is sent at most once: Morpheus does not retry it, and an expired
  sign-in is reported ("this sign-in needs renewing; the homeserver did not
  apply the change") instead of being renewed and the write sent again. If no
  answer comes back, or the account changes or the dialog closes while it is
  on its way, Morpheus says the result is unknown (never "not sent"); the next
  read shows whether it applied.
- Your own account cannot be ignored. Changes wait while account access or a
  security step is in progress. Everything shown belongs to the account (and,
  for details, the room) it was opened for, and is cleared on account change,
  sign-out or detach.

## Limits

- Between the second read and the write another device could still change
  the list; Matrix offers no conditional write for account data.
- A list with more than 10,000 entries, or one in a shape Morpheus cannot
  change safely, is refused rather than rewritten.
- Messages already shown before the change stay on screen until the timeline
  reloads.

# Room roles and permissions

Status: **in development**. Source only: nothing of it has been built or run
yet. Hosted core and native UI checks, and acceptance against real
homeservers, are pending.

Room settings has a **Roles and permissions…** button. It opens the room's
power levels (`m.room.power_levels`) in the same dialog:
- **People**: who has a level of their own, and room creators.
- **Permissions**: what each action needs.

The core is `crates/morpheus-matrix/src/room_settings/room_permissions.rs`.
The app controller is `crates/morpheus-app/src/room_permissions.rs`, and the
view is `crates/morpheus-app/ui/room-permissions.slint`.

## What it shows

- Everything is read fresh from your homeserver when the view opens and on
  **Refresh**: the room's create event, your membership and the power levels
  as stored. Each read is one bounded request.
- Levels follow the room version's rules. From room version 12, creators have
  unlimited power and are never changed here.
- Each person shows a role and a number:
  - Administrator for 100, Moderator for 50, Member for the default level.
  - Custom for anything else.
- Each permission shows who may act. A permission the room does not set says
  that its default applies.
- At most 500 people and 100 extra event permissions are listed. The rest are
  counted, never hidden silently.
- The view is read-only in these cases, and says why:
  - The room has no permissions event. Morpheus does not create one, because
    a first event changes several defaults at once.
  - Morpheus does not know the room version's rules.
  - The stored content is something it cannot change without loss. For
    example, it names a field twice, or holds `users`, `events` or
    `notifications` as something other than an object.

## Changing levels

- Select a row, type a level and press **Set**. **Default** removes a
  person's own level or an event permission, so the default applies again.
  **Undo** drops the staged change.
- To give someone a level who is not listed, type their full Matrix ID
  (`@name:server`) in the search field.
- Search filters loaded rows only.
- Nothing is sent while you stage. **Review** lists each change with its
  level before and after. **Save changes** sends them all as one change.
- Matrix's rules are checked when you review, and checked again against a
  fresh read right before sending. You cannot:
  - give a level above your own;
  - change someone at or above your level;
  - change a permission that needs more than your level.
- Lowering your own level needs an explicit confirmation. You cannot raise it
  again yourself; only someone with a higher level can. The review also says
  when no one else listed could change permissions afterwards.
- Nobody's level changes unless you staged it.
- Changing the default level changes it for everyone without a level of
  their own. The review says so.

## How a save is sent

- Right before sending, the stored permissions must still be exactly the text
  you reviewed; otherwise nothing is sent and you are asked to refresh.
- Only the changed entries are rewritten. Other fields, people, event entries,
  their order and how their numbers are written all stay as stored. In a map
  that changes (`users`, `events` or `notifications`), spaces between entries
  are not kept, and escapes inside names are normalised.
- The change is one request. It is sent once, with no retry and no renewed
  sign-in, and waits a bounded time for a small answer.
- Right before it is handed over, the view must still be open, for the same
  account and room. No account or security step may be in progress or have
  begun since the view opened, and you must not have left the room and come
  back. Otherwise nothing is sent.
- Matrix has no conditional write: a change made elsewhere at the same moment
  can be overwritten.
- After a save the permissions are read back. The view says whether each
  change shows.
  - If no answer came back, the change may or may not have applied. It is not
    sent again, and may still arrive. Changes to this room's permissions stay
    paused in this process, including after Refresh or closing and reopening
    the view.
  - An accepted save not yet shown stays paused until a complete read begun
    after its answer shows every change. Missing or unsupported permissions
    cannot prove that an entry was removed.
  - If the homeserver refused it, nothing changed.

## Privacy

Rows exist only while the view is open for its account and room. Closing it,
changing room or account, a lock and a detach clear them at once. Up to 32
unresolved save receipts remain in memory, containing the account, room and
staged permission changes; they hold no client or credential store. Complete
account removal or successful logout forgets that account's receipts. An
incomplete removal or ordinary draft cleanup retains them. Receipts are not
written to disk or logs, and do not provide protection across an app restart.

# Desktop notifications

Morpheus receives notifications through the Matrix SDK's push-rule handler for
the selected account. This covers both native sliding sync and classic sync.
Room mutes and other Matrix push rules are evaluated by the SDK before an event
reaches Morpheus. The app then suppresses its own messages, the room currently
visible in the conversation pane of the active window, duplicate events, and
message events more than five minutes old. A room left open in a window the
user is not using still notifies. A switch permanently invalidates queued notifications
from the old selection, including a quick switch from A to B and back to A.
Each SDK callback captures a revocable selection token before its first await;
logout, sync shutdown and reauthentication also revoke old tokens. The worker
checks the token again after asynchronous lookups and before native delivery.
These additional regressions await hosted native verification.

Successful in-app notification setting changes update the presenter before the
callback returns. External TOML changes are applied only after validation by
the config watcher. The delivery worker checks the active account, visible
room, and current policy again immediately before submitting a banner to the
operating system. A banner already accepted by the operating system cannot be
recalled if the account or preview setting changes at that instant.

`config.toml` controls the local presentation:

```toml
[notifications]
enabled = true
sounds = true
show_message_preview = false
show_room_and_sender = true
highlight_words = ["release", "urgent"]

[account_overrides."<local account ID>"]
notifications_enabled = false

# This device only, per room of that account; see "One room" below.
[account_overrides."<local account ID>".room_notifications."!room:example.org"]
quiet = true
hide_preview = true
no_highlights = true
```

Message previews are off by default. With previews off, a banner contains the
room, account, sender, and a generic “New message” label. With previews on, it
may include decrypted message text; unreadable encrypted events remain generic.
With `show_room_and_sender = false` (Settings › Notifications › Room and
sender names), a banner names only the account and says “New message” or “New
invitation”; it never previews text, whatever `show_message_preview` says.
Local highlight words mark an SDK-approved notification as a highlight; they
do not override a muted room. Sounds play only when both the local switch and
the Matrix push action request a sound, and the room is not quiet on this
device. The operating system's notification and Focus settings still control
presentation.

While sign-in, the app password, Manage accounts or a recovery key is on
screen, a banner names only its account and never previews text, whatever the
settings say. A detached or locked account is not live, so nothing is shown for
it.

Settings › Notifications › This account turns desktop notifications and
incoming-call alerts off for the visible account only, stored as that account's
`notifications_enabled = false` override; turning it on again removes the
override so the account follows the global switch.

## Room rules and default levels

Settings › Notifications lists the visible account's joined rooms (Spaces
excluded), searchable by name, with each room's level: follow the account
default, all messages, mentions and keywords, or mute. The account defaults for
group rooms and for rooms with exactly two members are either all messages or
mentions and keywords; Matrix keeps separate encrypted and unencrypted default
rules, and a choice here sets both (the list notes when they differ). These are
the account's Matrix push rules on its homeserver, written through the SDK's
notification-settings API, so they apply on every device of the account and are
what the SDK evaluates before an event reaches Morpheus. A change is shown as
saved only after the homeserver accepts it, and is applied to local delivery at
once, until sync brings the updated rules to the SDK (at most two minutes);
from then on the SDK's push actions decide, so keyword matches in a room set to
mentions and keywords notify. Saving a default updates the rooms that follow it
without clearing the search or a room change still waiting for the homeserver.
The list shows at most 100 matching rooms at a time and reads at most 2,000
rooms, the first ones by name. Reopening the section, or showing another
account, clears the search.

Keywords in the same section are Matrix keyword push rules, unlike local
highlight words: a message containing one notifies in every room that is not
muted, including rooms set to mentions and keywords, and the keywords apply on
all of the account's devices. Morpheus adds at most 32 keywords of at most 64
characters, plain text only (no `*`, `?`, `[` or `]`, which push rules would
read as patterns), and lists keywords added elsewhere as plain text. Requests and
results are bound to the account selection that made them; a switch discards
their results. Errors use fixed wording.

## One room: the header bell

The bell in the room header, beside room settings, opens a panel for the room
open in the conversation; it is struck through while that room is muted. The
same panel opens from Settings › Notifications › Rooms › This room, which
Settings search reaches with words such as “bell”, “this room” or “quiet room”.
A search result only reveals that entry, or the room search when no room is
open; nothing is written until a level or switch is chosen in the panel.

The panel reads the room's level from the push rules the SDK holds from sync
(no request is sent), with a loading state, and an error with Read again when
the room or its rules cannot be read. The level is one of the room-rule
choices of the Settings list: the account default (named, for rooms or direct
messages), all messages, mentions and keywords, or muted. It is the same
Matrix push rule, so it applies on all of the account's devices. Choosing the
account default removes only that room's own rules; every other rule, the
account defaults and the keywords stay. One change runs at a time, for exactly
the account and room shown when it was chosen. It is shown as saved only after
the homeserver accepts it; a change it did not confirm is reported as such
with a reread offered, never as undone. A change sent before the panel closed
still completes, and the Settings list, the bell and local delivery follow the
accepted rule.

“On this device” has three switches for that room, each of which can only
remove something from its banners: Sound (`quiet`), Message preview
(`hide_preview`) and Highlight words (`no_highlights`; Matrix mentions and
keywords still mark a banner). A switch is unavailable while its global
setting is already off, and it never turns one on. They are stored in the
account's `room_notifications` table, keyed by Matrix room ID, only while at
least one is on, for at most 256 rooms per account, through the same
validated save as the other notification switches; they hold no message
content and are separate from any room appearance setting.

A room's level does not change whether its calls ring. Incoming-call alerts
ignore exactly the rules the level is made of (the room's own muting override
and its room rule as the SDK writes them); every other push rule, the ignore
list and every trust check still apply, and call alerts for a whole account
are turned off with This account above.

The panel shows only while its account is live and its room is the one open,
and never over sign-in, the app password, Manage accounts or a recovery key.
Another room or account, one of those screens, a lock or a detach closes it
and clears what it showed; a result for an earlier opening, including after a
quick switch from A to B and back to A, is discarded. Escape, × or a click
outside closes it and returns focus to the bell, or to the Settings entry.
Tab stays inside the panel, and shortcuts pause while it is open. The panel
is never taller than the window below the header: at a small window or large
text its title and close action stay in place, the rest scrolls, and a control
reached by keyboard is scrolled into view.

## Read state and withdrawal

Notifications never mark a room or thread read and send no read receipt of
their own. Showing a room, activating the window or a sync batch sends nothing.
Read receipts come only from an explicit mark-as-read action or from the
opt-in automatic read tracking of room activity (off by default), which is the
single automatic receipt path. After each sync batch of the active account,
the unread counts the SDK computed from that account's receipts decide which
banners go: a room with no unread notification left, because it was read here
or on another device or client, loses its banners. Inactive accounts are never
read, and the request is bound to the account selection that made it.

A shown banner is remembered only as an opaque token with its account, room,
event and thread IDs, at most 32 of them and for at most 24 hours; no message
text is kept and nothing is written to disk. A newer message in the same room
replaces that room's banner once the new one is shown, and banners of a room
that was read here or elsewhere are withdrawn, where the platform allows: the
Linux desktop
notification service supports both through `CloseNotification`; the pinned
macOS and Windows backends have no withdrawal API, so their banners stay until
the user or the OS dismisses them. Quitting withdraws what it can. Removing or
signing out an account withdraws and forgets its banners.

## Opening a banner

Clicking a banner opens its exact account, room, event and, for a thread reply,
thread panel, through the same navigation as a message-search result. A banner
in the visible account opens directly. For another saved account the click goes
through the account menu's switch: an unfinished security operation still
blocks it, an active call still asks to be left first, and a protected account
still asks for the local app password. The target then waits for at most two
minutes and opens only when that exact account becomes visible; a newer click
or search result, or a different account becoming visible, drops it, so an
A → B → A sequence never navigates for the earlier request. While no account is
open, a click opens the banner's saved account through its protected activation
(app password included), then the banner's room once that account is open; a
newer click, another account opening first or two minutes passing drops it. An
invitation banner in the visible account points to the invitation in the room
list; one for another account only switches to that account, where the
invitation waits.

A banner resolves once; a dismissed, replaced, expired or unknown banner opens
nothing, and an old banner never brings back a removed account or a reused
local ID for a different Matrix user. A reply or edit being written in another
room is never discarded; the click is refused with a notice. A click for another
account never replaces an app-password or leave-call dialog that is already
open. Up to eight banners listen for clicks at a time. Banners past 24 hours are
withdrawn and forgotten; on Linux, with every listener taken, the oldest banner
Morpheus can withdraw is withdrawn so a new one keeps click handling. Otherwise
a banner beyond the bound is shown without click handling. On macOS the click also brings Morpheus forward;
on Linux and Windows a minimized window is restored, but raising it is up to the
desktop.

These behaviors were written against the pinned SDK and `notify-rust` sources
and have not been run. Click activation, withdrawal of banners for rooms read
on a real homeserver, push-rule changes on each platform, and the room panel, its
device switches and the call-ring separation remain to be accepted.

[Incoming-call sounds](INCOMING_CALLS.md) use a separate native speaker path
and the same local enabled/sounds preferences. They do not yet integrate with
system Focus/Do Not Disturb or produce incoming-call OS banners. Their native
tests and actual device behavior remain unverified.

Notifications are delivered by the running desktop process. Morpheus does not
install a background notification helper or register a push endpoint, so a
fully quit app has no notification producer. Background sync for inactive
accounts remains off; banners include the account ID for clarity.

The native presenter uses `notify-rust` 4.18.2. Its Linux backend uses the
desktop notification service, macOS uses Notification Center, and Windows uses
toast notifications with Morpheus's app ID. Packaged-app permissions, Windows
shortcut identity, sound behavior, and quit behavior require manual runtime
acceptance on each platform before the notification release gate can close.

On 2026-10-10, a signed macOS QA app installed in `~/Applications` and signed
in to two disposable homeservers passed a native delivery check. macOS
`usernoted` processed an alternate-room message for the exact
`io.github.brk275.morpheus` bundle ID and logged delivery to alert, lock
screen, and Notification Center with no Focus suppression. The machine had
originally denied Morpheus notifications and enabled Sleep Focus; the QA
temporarily enabled Morpheus and allowed it in Focus, then restored both
settings. A message in the visible room, one for inactive account A after
switching to B, and one after Morpheus fully quit produced no new notification
request. A visible banner and preview text were not captured, and macOS sound,
Linux, and Windows runtime behavior remain open.

# Desktop notifications

Morpheus receives notifications through the Matrix SDK's push-rule handler for
the selected account. This covers both native sliding sync and classic sync.
Room mutes and other Matrix push rules are evaluated by the SDK before an event
reaches Morpheus. The app then suppresses its own messages, the room currently
visible in the conversation pane, duplicate events, and message events more
than five minutes old. A switch permanently invalidates queued notifications
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
highlight_words = ["release", "urgent"]

[account_overrides."<local account ID>"]
notifications_enabled = false
```

Message previews are off by default. With previews off, a banner contains the
room, account, sender, and a generic “New message” label. With previews on, it
may include decrypted message text; unreadable encrypted events remain generic.
Local highlight words mark an SDK-approved notification as a highlight; they
do not override a muted room. Sounds play only when both the local switch and
the Matrix push action request a sound. The operating system's notification and
Focus settings still control presentation.

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

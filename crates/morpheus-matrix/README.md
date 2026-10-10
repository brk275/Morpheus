# Morpheus Matrix core

`morpheus-matrix` owns local Matrix accounts and basic messaging. It uses the
same pinned `matrix-rust-sdk` revision as `morpheus-rtc` so a selected account's
`matrix_sdk::Client` can be passed directly to native calling code.

## Account lifecycle

Create `AccountManager::open(data_dir)` once after installing the application's
Rustls crypto provider and starting a Tokio runtime. `open` reads the account
registry without accessing secrets. Call `restore_last_active().await` to open
the previously selected account, or `add_password_account(homeserver, username,
password).await` to sign in and select a new device. `accounts().await` supplies
the bottom-left account picker; use the stable `AccountInfo::id` with
`switch_to(id).await`.

`switch_to` stops the former account's sync before starting the next one.
Subscribe to `subscribe_active_account()` and discard all room, timeline, and
RTC objects from the former account on each change. `active_client().await`
returns an SDK client clone for account-scoped features such as native RTC.
`logout_account(id).await` attempts server logout, then removes the local
credential and store even when the network fails. If credential deletion fails,
the account stays in the picker so logout can be retried. Call `stop().await`
during shutdown.

Account mutations and account-scoped security operations are serialized with
switching. A slow homeserver or credential store may therefore delay the
account picker until the current operation finishes.

Each account has its own SQLite store under `data_dir/accounts/<uuid>/store`.
The local store passphrase, Matrix access/refresh tokens, and device session
are kept in the macOS Keychain, Windows Credential Manager, or Linux Secret
Service through `keyring`. `accounts.json` contains only display identities,
homeserver URLs, stable local IDs, and the selected ID. A locked or unavailable
credential store produces an error; it never triggers plaintext token storage.
Password login and Matrix browser SSO login-token flow are supported. For SSO,
the app first binds an HTTP listener on a loopback address and explicit port,
then calls `begin_sso(homeserver, loopback_redirect_url, identity_provider_id)`.
Open the returned `browser_url` in the system browser. When the listener
receives the full callback URL, pass it with the returned `attempt_id` to
`finish_sso(attempt_id, callback_url)`. The core validates callback origin,
path, and a random state before using the one-time login token. Do not log the
callback URL. The app is responsible for launching the browser and serving a
small completion page. MSC3861 OIDC, QR/device login, cross signing UI, and
key backup/recovery UX remain to be implemented.

## Messaging API

- `subscribe_sync_status()` reports `Starting`, `Running { backend }`,
  `Retrying`, or `Stopped`. `SlidingSticky` indicates the SDK UI SyncService
  feed needed by native MatrixRTC. `Classic` means chat sync is available but
  RTC sticky membership updates are not.
- `subscribe_room_updates().await` returns the selected account ID and an SDK
  room update receiver. Recreate it after switching accounts.
  `room_updates_change_space_graph(&updates)` detects child-link state changes
  so the app can invalidate a cached Space hierarchy.
- `rooms_snapshot().await` returns joined and invited rooms for display.
- `timeline(room_id).await` returns a live SDK UI timeline. Use its
  `subscribe()` for initial items and vector diffs, and its pagination API for
  history. Drop it after switching accounts.
- `message_feed(room_id).await` returns a native UI friendly watch receiver of
  `MessageInfo` values and `messages_snapshot(room_id).await` gives a one-time
  list. These intentionally omit non-message timeline events.
- `send_text`, `send_reply`, `send_thread_text`, `edit_text`, `toggle_reaction`,
  and `redact_message` use SDK timelines and local echo/send queues.
- `thread_timeline` and `paginate_backwards` support thread panels and
  historical messages. `search_room` queries the local SDK index.
- Room operations include `create_private_room`, `create_dm`, `join_room`,
  `leave_room`, `invite_user`, `kick_user`, `ban_user`, `unban_user`,
  `rename_room`, and `set_room_topic`. Server power levels still govern them.
- `invites_snapshot`, `accept_invite`, and `reject_invite` support invitation
  panels. `create_space`, `space_hierarchy`, `link_space_child`, and
  `unlink_space_child` support paginated Space navigation. The hierarchy
  reflects server-advertised child links; it does not imply that the user has
  joined each child.

## Device security

`security_info()` reports own-device verification, recovery, and backup
states. `request_own_verification()` starts a SAS flow with another verified
device; `verification_request_info`, `accept_verification_request`,
`start_sas_verification`, `accept_sas_verification`, `sas_presentation`,
`confirm_sas_verification`, and `cancel_verification` expose its steps. The app
must show the emoji or decimal SAS on both devices and call confirm only after
the user explicitly says they match. Incoming requests and flow changes arrive
through the SDK's sync feed; the UI must poll or subscribe to them. QR
verification UI is not implemented.

`enable_recovery(passphrase, wait_for_backup_upload)` creates secret storage
and encrypted room-key backup. Its `RecoveryKey` is not Debug/serializable and
zeroizes on drop; the app should display `reveal()` once and ask the user to
save it. `recover_account(key_or_passphrase)` imports secrets and repairs
backup on a new device. These wrappers have been type-checked but need an
end-to-end two-device test before distribution. Never log a recovery key.

The sync runner checks `/versions` for MSC4186, then runs SDK UI `SyncService`
with the `unstable-msc4354` sticky extension. It retries transient failures.
On homeservers that explicitly lack Sliding Sync, it runs classic `/sync` for
chat. It stops the former account's service before switching to another.

## Build

From the workspace root: `cargo check -p morpheus-matrix` and
`cargo test -p morpheus-matrix`.

License: AGPL-3.0-or-later.

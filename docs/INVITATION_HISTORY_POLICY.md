# Invitations and encrypted history

Morpheus sends ordinary Matrix invitations. It currently disables the pinned
SDK's experimental MSC4268 encrypted-history bundles, both sending bundles to
invitees and receiving bundles when accepting invitations. Inviting someone
does not opt in to distributing historical room keys. A future history-sharing
feature needs an explicit product flow and its own encryption/trust acceptance.

This setting is independent of encrypted messaging, existing-session key
requests, a user's own key backup, and recovery. Those retain their existing
policies. It does not verify a device, reset an account identity, or loosen the
SDK's checks on encrypted-history recipients.

## Why the default is explicit

The pinned Matrix SDK commit
`c6ad52ecc09eaa36601f55effb0a48a533eb87dd` enables history-on-invite by default.
Its `Room::invite_user_by_id` awaits `share_room_history` before sending the
Matrix invitation. After constructing a nonempty key bundle, that path uses a
conservative identity-based recipient strategy and can reject an unverified
sender before the invitation reaches the homeserver. Morpheus has no explicit
history-sharing feature yet, so its shared authenticated-client builder uses
`with_enable_share_history_on_invite(false)`.

Password sign-in, saved-session restore/switching, OAuth and legacy SSO all use
that builder. The separate unauthenticated discovery client cannot invite.
Relevant pinned sources are
[the builder option](https://github.com/matrix-org/matrix-rust-sdk/blob/c6ad52ecc09eaa36601f55effb0a48a533eb87dd/crates/matrix-sdk/src/client/builder/mod.rs),
[the invite operation](https://github.com/matrix-org/matrix-rust-sdk/blob/c6ad52ecc09eaa36601f55effb0a48a533eb87dd/crates/matrix-sdk/src/room/mod.rs),
[history sharing](https://github.com/matrix-org/matrix-rust-sdk/blob/c6ad52ecc09eaa36601f55effb0a48a533eb87dd/crates/matrix-sdk/src/room/shared_room_history.rs),
and [bundle recipient checks](https://github.com/matrix-org/matrix-rust-sdk/blob/c6ad52ecc09eaa36601f55effb0a48a533eb87dd/crates/matrix-sdk-crypto/src/session_manager/group_sessions/mod.rs).

## Evidence and limits

Hosted run `38112448483`, revision `db25e42`, reached the moderation test's
re-invitation after an encrypted message, redaction and kick, then failed at
`moderation_local.rs:415`. Its raw child error was withheld; the retained log
contains the panic location only. The SDK path above is a source-backed
explanation to test, not a confirmed reconstruction of that error.

The hosted moderation gate now requires re-invitation on a fresh device with
an existing, unverified account identity, unchanged identity trust, and actual
rejoin after kick and unban. It retains the existing authoritative state,
permissions, encryption, redaction, ban and rejected-join checks. Fixed
invitation-stage and failure categories can leave the private log; arbitrary
SDK/server messages cannot. Successful process exit alone cannot satisfy the
new evidence requirement.

The option and pinned source establish the bundle policy; the integration
test does not inspect every outgoing HTTP request or prove history sharing
interoperability. Updated hosted verification is still required. No native
test or live service was run locally for this change.

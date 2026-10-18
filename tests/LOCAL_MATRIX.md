# Local Matrix acceptance environment

`scripts/local_matrix_env.py` runs two disposable Synapse v1.162.0 homeservers
from the [official Element image](https://github.com/element-hq/synapse/blob/develop/docker/README.md).
Docker publishes their client APIs only on `127.0.0.1`. No email addresses or
public Matrix accounts are used.

## Start and inspect

```sh
python3 scripts/local_matrix_env.py up
python3 scripts/local_matrix_env.py status
```

The servers are `http://127.0.0.1:18008` (`morpheus-a.test`) and
`http://127.0.0.1:18009` (`morpheus-b.test`). One image is shared by both
containers. SQLite, config, and signing keys live in two dedicated Docker
volumes. The script seeds Alice and Bob on A and Carol on B, one room unique to
each homeserver, and an encrypted room joined by Alice and Bob. It verifies
password login, room membership, Megolm room state, and rejection of a token
issued by the other server.

The command prints the path to a mode-0600 `accounts.json` in a mode-0700 temp
directory. It holds account passwords and room IDs. Keep the file local and
never commit or paste it. The script is idempotent: `up` reuses these accounts
and rooms on later runs. It raises local login and invitation rate limits to allow automated
tests; the servers remain bound to loopback.

## Test the Rust account manager

Set `MORPHEUS_MATRIX_TEST_ACCOUNTS` to the **path** printed by `up`, then run:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-matrix --test live_local -- --nocapture
```

This test is skipped unless the variable is set. It signs in Alice and Carol
on different homeservers, checks that each account sees only its own rooms,
switches accounts, restarts `AccountManager` and restores the selected account,
then sends an encrypted message from Alice to Bob. It checks that Bob can read
the message before and after restarting his Matrix session. Successful runs
log out the test devices and delete their temporary client stores. A failed
run leaves its temporary client store for diagnosis.

The same credentials can be entered in the Morpheus desktop sign-in screen to
inspect the account switcher and room lists manually. The first two labels in
the file identify the Alice/Bob users on A; `b_primary` identifies Carol on B.

## Test room actions

With both local servers running and the same protected fixture path set:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-matrix --test room_actions_local -- --nocapture
```

This separate opt-in test creates private rooms, a DM, and a private Space.
An independent signed-in client reads each new room's `m.room.encryption` state
directly from Synapse and checks the Megolm algorithm before testing invite
accept/decline and joins by room ID and temporary alias. Creation fails if a
homeserver rejects the encryption state; Morpheus does not return a cleartext
private room as a fallback. The test makes disposable rooms on Synapse A,
removes the alias, and logs out its test devices. It does not test encryption
of a Space: Space child links are navigation state visible to participating
servers and clients, and Morpheus currently creates private Spaces without
room-message encryption. Ordinary room metadata such as names and membership
also remains visible to the homeserver. Delayed Space classification in the
SDK room list is handled by the app's temporary Space entry and still needs a
dedicated UI test.

## Test room settings and moderation

With both local servers running and the same protected fixture path set:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-matrix --test moderation_local -- --nocapture
```

This opt-in test creates a disposable room on Synapse A and checks that Alice
can rename it and set its topic while default-power Bob cannot rename or kick
the creator. Alice grants Bob moderator power, Bob renames the room, the server
rejects Bob's attempt to promote himself above his own level, and Alice
restores Bob's default power. Bob's next rename is rejected. The test then
checks a server-confirmed redaction of Bob's message in both clients, kick and
reinvite, ban and blocked rejoin, and unban and rejoin. It fetches authoritative
room state and the redacted event from Synapse instead of relying only on
local timeline snapshots. A passing run logs out the temporary test devices
and deletes their stores. A failure may leave a disposable room and store for
diagnosis. This checks Matrix core behavior against one loopback homeserver;
moderation UI and federation remain separate acceptance work.

## Test public discovery and Matrix links

With both local servers running and the same protected fixture path set:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-matrix --test discovery_local -- --nocapture
```

This opt-in test publishes disposable public rooms, searches with pagination,
resolves an alias, and joins a room from a `matrix.to` link with a routing
server. It checks that a directory response cannot cross the selected-account
boundary and that an account on homeserver B sees its own directory. The
loopback fixture explicitly permits public-room publication and removes the
rooms from the directory after a passing run. Its servers do not federate, so
remote-server directory queries and federated joins remain untested.

## Test local encrypted message search

With the same protected fixture path set:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-matrix --test search_local -- --nocapture
```

This opt-in test sends a unique message in Alice and Bob's encrypted room,
waits for its remote echo, and searches Alice's local index within the room
and across her joined rooms. It loads an event-focused timeline around the
result and checks that Carol on the separate homeserver cannot
read Alice's results. Each account's index is stored encrypted under its own
data directory; the test checks persistence across restart and scans the
store for the unique plaintext body. Search is limited to locally indexed
history and does not send message text to a server search endpoint.
Set `MORPHEUS_MATRIX_SEARCH_CLASSIC=1` for a separate run using the forced
classic `/sync` adapter; omit it for automatic native sliding sync.

## Test encrypted attachments

With both local servers running and the protected fixture path set:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-matrix --test media_local -- --nocapture
```

This opt-in test signs in disposable Alice and Bob on Synapse A, uploads a
binary file to their encrypted room through the persistent Matrix send queue,
and downloads it through Bob's session. It checks exact decrypted bytes,
filename, MIME type, declared size, encrypted-file metadata, and rejection of
a mismatched active account. After removing the plaintext input file, it scans
the sender and receiver account stores for a unique payload prefix. A passing
run logs out both test devices, removes their OS-vault entries, and deletes
their temporary stores. A failed run can leave a temporary store for diagnosis.

Uploads are capped at 25 MiB before entering the SDK. Downloads reject absent
or excessive declared sizes, then enforce the cap as HTTP chunks arrive through
the SDK's configured client. A unit test rejects an oversized stream, and the
encrypted Alice/Bob roundtrip passed after this change. An adversarial
homeserver response is still untested. The test covers the Matrix core;
the app's attach/save dialogs, upload controls, timeline card, and preview/open
behavior remain separate live UI acceptance work. Send queues run for the
selected account and pause on an account switch; an already in-flight upload
may finish. Recoverable room-queue errors retry with bounded backoff while
the account stays selected. A retained request is not treated as a terminal
failure or offered as a new duplicate send.

## Test encrypted composer drafts

With both local servers running and the protected fixture path set:

```sh
PATH=/opt/homebrew/opt/rustup/bin:$PATH cargo test --locked -p morpheus-matrix --test drafts_local -- --nocapture
```

This opt-in test saves different drafts for Alice and Carol, switches accounts,
restarts the client, checks both drafts, clears one, and verifies logout removes
that account's store. It also scans the local account data for the draft bodies
as raw bytes. The Matrix SDK's passphrase-protected SQLite state store holds
the drafts; the passphrase remains in the OS credential vault.

Morpheus retains a queued message's encrypted draft until the room send queue
reports server acceptance. A send failure restores unchanged text. Before a
room or thread message is handed to the SDK queue, its pending record (an
opaque ownership nonce, the draft generation and, once assigned, the exact
transaction id) is written into the same encrypted draft row as the text, and
a thread send also records its root in the room row's thread index. After a
restart the records are matched only against the SDK's own queue by exact
transaction id: matching requests are observed to their result, a request the
queue no longer holds is shown as unconfirmed for the user to check in the
conversation, and nothing is adopted by content or time or resent
automatically. Every record carries the complete typed text it was created
from, and a row a build cannot read is quarantined with its complete original
until the user discards it, pausing that room's queue meanwhile. The draft
fixture round-trips two room records with their text, a thread index and a
metadata-only row, and scans the raw account data for the encoded envelope and
its marker attribute. A second gated scenario in the same file drives real SDK
requests through controlled queue barriers: records durable before enqueue,
single-use enqueue grants, stale and wrong-authority refusals, out-of-order
cancellation, crash windows restored as unconfirmed, exact adoption of a
persisted request across a restart and a re-selection, and a quarantined row
pausing its room until the explicit discard. Neither scenario has yet been
built or run.

## Test encrypted-history recovery

After `up`, create a **fresh** Alice/Bob pair and encrypted room on Synapse A:

```sh
python3 scripts/local_matrix_env.py recovery-fixture
```

The command prints only a separate mode-0600 JSON fixture path. Set
`MORPHEUS_MATRIX_TEST_ACCOUNTS` to that path and
`MORPHEUS_MATRIX_TEST_RECOVERY=1`, then run the `recovery_local` test. Use a new
fixture for each run: the test creates a server key backup, so reusing its
accounts would invalidate the fresh-backup assertion. This command does not
alter the original Alice/Bob/Carol fixture or its rooms. `down` retains all
fixtures; `destroy` removes them with the two test volumes.

## Stop and remove

```sh
python3 scripts/local_matrix_env.py down
python3 scripts/local_matrix_env.py destroy
```

`down` removes only the named Morpheus test containers and network, retaining
volumes and credentials. `destroy` also removes those two volumes and the
credentials file. Both commands leave the shared Docker image in place.

These loopback servers test local chat, account isolation, and encrypted room
behavior. A separate [local MatrixRTC setup](LOCAL_MATRIX_RTC.md) adds a
disposable call backend to Synapse A. Neither setup establishes federation,
SSO, or cross-platform UI behavior.

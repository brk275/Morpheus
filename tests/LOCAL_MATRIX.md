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
and rooms on later runs. It raises local login rate limits to allow automated
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

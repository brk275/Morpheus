# Start a conversation

**Status: source only. Not built, run or tested yet.**

The direct-messages button beside "Create or join a Matrix room" at the top of the room list opens **Start a conversation**. Entering a complete Matrix user ID in the Join a room dialog, or in the directory's join field, opens it too, with that person filled in.

## Finding people

- The search asks only the selected account's own homeserver, through its user directory. It uses the account's existing authenticated client with one typed request:
  - one attempt;
  - at most 128 characters and 20 results;
  - a 64 KiB response limit and a 15 s time limit.
- No other server or URL is contacted, and no avatar is fetched. The query is never logged, and errors use fixed wording.
- The search runs after a short pause in typing, or at once on Enter.
- A complete Matrix ID such as `@name:example.org` appears at the top, marked "as typed", even if the directory does not list it.
- Results are one-line, sanitized names with their Matrix IDs, in a keyboard-navigable list. Up and Down move the selection and keep it in view, and Enter chooses. The account itself and duplicates are never listed.

## Opening or starting a conversation

Choosing a person only looks at this device's joined rooms and the account's `m.direct` data. The window says which case applies:

- **One known conversation:** Open conversation.
- **Several:** it shows how many and opens the first.
- **None known here:** it says so; one may still exist elsewhere.

Opening a conversation goes through the shared conversation path, so your drafts are kept.

**Start conversation...** asks for confirmation that names the exact Matrix ID. Confirming then:

- sends one creation request as a single attempt on the wire: SDK retries are off and its token-refresh replay is disabled, so an expired token ends the attempt instead of resending it. Morpheus never retries it. The room is encrypted from creation, uses the trusted-private preset and invites only that account.
  - Listing the room as a direct conversation in your account data (`m.direct`) uses Morpheus's own bounded writer, not the SDK helper:
    - it makes a fresh read, merges in only the new person-to-room entry (every other field value is kept as raw JSON; object key order and whitespace may change), re-reads immediately before writing, and refuses to write if the list changed in between;
    - the write is sent once, with no retry and no token-refresh replay;
    - Matrix account data has no compare-and-swap, so a change landing between the re-read and the write could still be overwritten. That window is one round trip.
    - Only one `m.direct` listing runs at a time in the app (an accepted direct-message invitation's included): this one waits its turn before its first read, at most a minute and not once the window closes or the account changes. If it never gets its turn, nothing is written and the window says the listing was not written.
  - The room exists whatever happens to that listing. If the listing was not written, or its outcome is unknown, the window says so instead of opening the room straight away. It never reports the creation as failed and never tries the listing again.
  - The request is raced against closing the window and against the account selection ending, so an old account's creation is never sent after a switch. A close or switch while the request may already be on its way is reported as unknown.
- marks the room as a direct conversation in your account data, and waits briefly for it to sync before opening it.

If something goes wrong:

- A refusal from the homeserver is reported as such.
- Any other failure after the request left is reported as unknown, because the conversation may exist. Start is then not offered again until you choose the person afresh.

## Privacy and fences

- Every search, lookup and creation is bound to the exact account selection and a cancellation token. An account switch (including A -> B -> A), lock, detach or closing the window ends it, and late results are dropped.
- Rows, the chosen person and notices are cleared synchronously with the account.
- App requests also carry the current account-security epoch, checked immediately before each SDK handoff. A security step invalidates that epoch even if its dialog closes before the request resumes. Closing or locking clears the pending presentation; a request already handed to the SDK cannot be recalled.
- Shortcuts pause while the window is open.

## Code

| Part | File |
| --- | --- |
| Core: user search, known conversations, encrypted creation | `crates/morpheus-matrix/src/people_directory.rs` |
| Controller | `crates/morpheus-app/src/people_directory.rs` |
| Window, entry button | `crates/morpheus-app/ui/app-window.slint` |

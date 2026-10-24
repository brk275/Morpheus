# Encrypted room-key files

Settings → Security & recovery offers explicit import and export for the currently selected, already loaded Matrix account. Export uses the Matrix encrypted key-file format and a fresh local passphrase (12–1024 UTF-8 bytes, entered twice). Import accepts an existing passphrase (1–1024 bytes). The app never saves the passphrase, writes plaintext recovery keys to a scratch file, replaces an existing export, or automatically retries an uncertain import/publication.

Search Settings for “import room keys”, “export room keys” or “encrypted key file” to reveal the passphrase input. Choosing that result only focuses the input; it does not open a file chooser or start an import or export. A busy or protected control requires a fresh search choice after it becomes available.

The password controls clear before submitting an action and when the page, account or protection context changes. This limits retained application copies; it does not promise erasure of operating-system, input-method, allocator or compiler copies. The chosen filename and detailed SDK/native errors are not logged or reflected in the result message.

## Exact account and task ownership

An intent captures the original account-selection epoch, loaded-client identity, verification-work lease, cancellation token and security generation. The SDK has no app-authority callback. Core preparation completes first; final checks occur after every preparation wait and immediately before the existing store-import or synchronous no-clobber publication handoff. Import holds the manager-state guard and exact SDK machine read guard through the actual admitted store operation. Cancellation revokes later handoffs and presentation, while admitted work is awaited rather than dropped.

The existing verification-work owner retains the actual enclosing Tokio task. The existing dialog supervisor separately retains the native chooser/CPU thread and publishes a per-worker ticket only after the real worker (including its thread-local destructors) and supervisor joins. A body result, canceled token or unrelated global idle state is not that receipt. Account removal/logout and ordinary quit use that same task owner's checked drain; an open chooser must actually close. No second task registry or detached blocking SDK operation is introduced.

Known worker panic or unproved native ownership becomes sticky failure on the exact existing account-work epoch after actual native joins. A missing/replaced epoch fails global ownership rather than blessing a replacement account. Pending still means a real retained task has not joined. Failed means a clean completion receipt is refused; it is not invented live work or proof that no effects occurred.

## Bounds and cryptography

The same pinned Matrix SDK/crypto revision supplies the canonical armor, PBKDF2, AES and HMAC implementation. The additive byte API caps armor at 32 MiB, keys at 20,000, KDF rounds at 1–1,000,000, and metadata per key. It checks the complete minimum layout before slicing/KDF, authenticates before decryption/JSON parsing, rejects the next record before decoding it, and protects authenticated plaintext with the existing zeroization facilities. Export uses 500,000 rounds and refuses an over-limit snapshot rather than presenting a truncated backup.

The SDK's existing store export first reads its full session snapshot. These file-output bounds do not claim constant-memory store access. Import counts distinguish complete decoded records, SDK-valid records and sessions actually imported. Existing remote backup triggering is not proof of a completed homeserver backup. An admitted import error may have partial store effects and is never presented as an unchanged store.

## Filesystem contract

Only bounded encrypted bytes reach platform IO. Absolute ordinary paths are traversed component by component with retained no-follow directory capabilities. Inputs must be one regular, single-link file; reads use the same opened file, a 32 MiB bound and a post-read identity/metadata check. Unix sets `O_NONBLOCK` before type admission so a FIFO cannot block first. This is not an immutable snapshot guarantee against a hostile process with the same user's authority.

Unix stages in a new private directory, with a private encrypted file. Darwin rejects existing parent allow ACLs before creation and clears/verifies ACLs only on newly owned objects; existing parent ACLs/permissions are untouched. Linux private modes also mask POSIX ACL group/named-user permissions. Bytes are written, synced and the data descriptor is checked closed before publication. Publication uses only `renameatx_np(RENAME_EXCL)` or `renameat2(RENAME_NOREPLACE)`. Exactly documented `EEXIST` is a definite refusal and leaves an explicitly discardable stage. Every other unproved rename result remains publication-unknown. Private-stage cleanup checks exact inode/type/ownership before unlinking. Descriptor closes consume each `File` once; a failed close is never retried or reconstructed from its possibly reused numeric descriptor.

Windows uses protected current-user/System ACLs and real handle-relative native opens. It verifies a new private staging directory before creating the fixed encrypted child, keeping short-name aliases inside that directory. The actual child handle is renamed to the original retained destination parent. Only an exact completed native name-collision status leaves a discardable stage; all other unproved publication outcomes remain unknown. Unexpected pending operations retain their original heap-stable native request storage and handles without reborrowing or freeing native-writable slots. The final Windows module requires its own independent ABI/ACL/source review; earlier held drafts are not accepted substitutes.

Successful publication is recorded before any later cleanup. A known published file with unproved cleanup remains reported as published, with a sticky failed owner. Unknown publication is never deleted or retried. Unsupported filesystem/ACL/path capabilities return a fixed error without a relaxed fallback. Checked file flushing/closing does not establish directory-entry, removable-media or power-loss durability; no volume/admin flush is attempted.

## Verification status

This contribution is source preparation. Original crypto fixtures retain their exact upstream provenance and still require the reviewed trusted-base scanner-manifest admission; there are no new scanner exceptions here. Shared SDK/vendor/core/UI and native-ABI changes require source review followed by affected checks on fresh free standard hosted runners. Slint compilation, real file dialogs, Matrix interoperability, accessibility, actual OS-specific ACL/no-clobber behavior and real account/removal/quit behavior remain unverified. No beta, distribution, installed-update or real-device acceptance follows from these source regressions.

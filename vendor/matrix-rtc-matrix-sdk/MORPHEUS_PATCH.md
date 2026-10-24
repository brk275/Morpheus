# Morpheus original slot-authority correction

This is `matrix-rtc-matrix-sdk` 0.5.0-rc.2 from Element Matrix Rust RTC
`53880ef1ef66b62df555ce71ccd1d6c85b813596` (`v0.5.0-rc.2`). All four original
package files remain present. `Cargo.toml.upstream` and
`Cargo.workspace.toml.upstream` retain their complete original declarations.
The managed manifest expands inherited values and sibling paths to the SAME
already locked Git tag/revision selectors. No dependency version, feature,
normal/dev dependency, target or original test was added or removed by this
normalization. The package has no build script or proc-macro target. Original
Element copyright, dual-license declarations, `LICENSE` and
`LICENSE-COMMERCIAL` remain exact; no commercial license grant is inferred.

The standalone call facade now uses the same `DialectBackend` wire translation
as joined calls. This backend preserves stable-over-unstable slot authority:
before ONE slot state write, it reads `m.rtc.slot` at the SAME original room and
state key from the original client's own homeserver. A successful JSON response
means the stable event remains authoritative, including empty, closed or
malformed slot content. Only exact HTTP 404 plus structured `M_NOT_FOUND`
selects the canonical unstable event type. Other HTTP/transport/parsing/body-limit
errors fail with fixed text and no state write. Non-slot state events, including
RoomState memberships, retain their original path without this read.

The read has the existing RTC request timeout, zero retries, and a 64 KiB
response limit enforced by the existing SDK streaming collector. PUT request
policy is unchanged. This introduces no alternate endpoint, token exchange,
OpenID request, peer URL, write replay or dual-spelling write. The original
room, state key and serialized application/encryption content are preserved.

An optional owned slot-write admission predicate retains the original app
selection through the entered read. The same original Room must still be Joined;
that predicate is checked again after constructing the sole PUT, with no await
before passing it to the original SDK client. Morpheus's explicit outgoing API
requires that predicate and its callers capture their existing original
AccountSelection clone. No account ID is used to renew authority. An operation
already entered keeps the existing controller's actual completion/reservation
ownership; this patch adds no worker or detached registry.

The existing Morpheus RTC native unit route contains bounded loopback HTTP
regressions against the REAL SDK adapter: present stable state, exact absence,
HTTP/auth/parser/size/transport failures, an actually entered GET held across
original admission revocation, and unchanged non-slot writes. Every fixture
worker/request handle is retained and joined; data and sessions are synthetic.
No upstream SDK dev-dependency test graph is selected. The existing local RTC
fixture separately exercises real same-key stable close/empty/reopen and fresh
canonical slot authority while preserving original media and cleanup assertions.
Source preparation does not claim those tests have passed.

Reads and writes are not an atomic cross-device Matrix transaction. Another
room-authorized device can change authority after the read. There is one write,
no retry under another type and no claim of atomic negotiation. Existing stable
read precedence is unchanged. Pinned stock JS reads only unstable slot state and
allows absence; it cannot observe a stable-only close. That preexisting stock
interoperability limitation is not claimed solved by preserving native old-room
close/reopen behavior. No default/mode/incoming Answer/key policy is changed.

The closed Linux run 40182055679 exposed a pre-join upper-slot observation
failure. Locked Ruma 0.35.0 does not define the slot aliases assumed by the
original standalone backend comment. That source mismatch is corrected, but
this patch is not claimed to be the sole historical cause of all reduced Sticky
failures or evidence of successful real encrypted interoperation.

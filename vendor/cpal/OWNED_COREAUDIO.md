# Morpheus same-version CPAL ownership patch

This directory preserves CPAL 0.18.2 source from the crates.io archive with SHA256
`6f02e8d0327b42d3e2e4ab2119af397344eb9fc54a34bf0ddeaa1277af8681f1`
and recorded upstream commit `e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7`.
Its Apache-2.0 license and original source, examples, Cargo.toml.orig and upstream
Cargo.lock are retained. The application uses its own root lock; the upstream
lock is provenance data and does not govern verification.

## Build and dependency boundary

The normalized packaging manifest is deliberately managed as an explicit workspace
library member. Example declarations and the four example-only dev dependencies
(anyhow, clap, hound, ringbuf) are omitted from that manifest. Their original
sources/manifest are preserved. Library tests use std and existing production
CPAL dependencies; no new dependency version is admitted. macOS adds a direct
libc 0.2.190 edge already present in the locked CPAL target closure, plus explicit
AudioComponent/AudioOutputUnit features on the already pinned objc2-audio-toolbox
0.3.2 extern-API declarations. Existing
hardware test bodies are preserved; focused hosted library checks must select
only reviewed synthetic ownership regressions. They are not hardware acceptance.

## macOS resource owner

Each native stream is constructed and controlled on a retained native thread.
A published supervisor exists before that thread can be created. Resources owns
partial AudioUnits, property-listener tuples and tap/aggregate identifiers before
examining native statuses. Stop, uninitialize, dispose, listener removal and tap/
aggregate destruction are checked independently. Successful phases are recorded;
a nonzero/uncertain phase is not blindly retried. An error leaves the same actual
owner/resources retained and blocks replacement. Native owner panic/TLS panic or
ownership uncertainty also retains a permanent fence. Typed Pending only observes
completion of that same retained owner; it does not create another native owner.

The result requires actual owner join including callback/user-capture and TLS
cleanup. Native body completion or is_finished alone does not establish that join.
A separate supervisor performs it off UI/Tokio. The final fixed supervisor OS
epilogue has no hard scheduling-time guarantee. Native APIs themselves can block;
observers remain bounded while the original resources and handles remain owned.

## Callback lifetime and realtime boundary

Native refcons carry checked nonreused integer tokens into a fixed 64-slot static
registry. They never dereference freed per-stream context. Generation checks and
stable-address, off-RT-initialized pthread NORMAL mutexes protect callback bodies.
The RT path uses checked trylock only, has no std poison/TLV mutex guard and no
channel wake. Failed lock never grants access; failed unlock permanently faults
the slot and forbids freeing/reuse/success. Every mutable failure write is inside
the exact-token gate; delayed tails can only emit bounded format-correct silence.
Stream Drop/close requests revoke exact tokens using atomics and existing owner
polling. Owner-thread/callback self-close is refused without blocking on itself.

Buffers, callback metadata, timebase, listeners and input storage are prepared
before callbacks. Output fallback uses CPAL storage widths including four-byte
I24/U24 and correct unsigned equilibrium; null zero-length native output never
constructs Rust Data/slices. No new allocation, waiting lock, logging or native
property query is introduced in the normal callback shim. The preexisting app
callback/mixer allocation/locking behavior is a separate realtime limitation.

A caught user callback/setup error is reported to the original operation and may
coexist with authentic successful resource teardown/join. Its callback body and
panic payload stay retained until off-RT cleanup. Unexpected owner/supervisor/TLS
panic and native teardown/ownership failures cannot be converted to success or a
fresh owner. catch_unwind cannot prevent allocation/logging by a user panic hook,
panic=abort/OOM/aborting destructors or foreign unwind/ABI violations. Those cases
provide no successful close receipt. Checked Apple resource API completion is not
a claim that every internal Apple audio thread has terminated.

## Application integration and limits

The separately bound 8761 consumer DATA explicitly consumes fallible macOS close
before cancelled opener, active call, ring and attachment worker/TLS completion.
Cancellation and account/call authority stay revoked while the same worker is
pending. New route values are immediately dropped on cancellation during old-route
close; final ordinary shutdown checks that no native stream remains admitted and
returns a closed static error after all existing mandatory cleanup. Other hosts
retain their honest narrower application-worker/TLS receipt; native Windows COM/
notification and ALSA PCM disposal errors are not silently called verified close.

This source-only proposal is not execution, installed-package, real-device,
normal-UI, signing, distribution or beta acceptance. Production updater installation
gates remain unchanged and false. All checks require independent source review
and affected fresh free hosted verification before any integration claim.

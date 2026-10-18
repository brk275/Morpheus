# Stickers

Morpheus has a native sticker catalog, picker and gated send path for the room and thread composers, built from Matrix image packs. It is written in Rust and Slint: there is no web view and no executable plugin. The catalog, picker and send path described here are **source only and have not executed**. Incoming encrypted room and thread sticker display, an earlier change, passed the real Linux media fixture in run 39157907074.

## Sources and formats

- **Matrix 1.19 room packs:** every `m.room.image_pack` state event of the destination room, ordered by state key.
- **Matrix 1.19 enabled packs:** `m.image_pack.rooms` global account data naming packs in other rooms. A pack is read only from a room this device has already joined.
- **Legacy MSC2545 shapes:** `im.ponies.room_emotes`, `im.ponies.emote_rooms` and the personal `im.ponies.user_emotes` pack. The personal pack is listed first. Matrix 1.19 defines no personal pack, so only the legacy one exists. A legacy image may narrow its own `usage`; a stable image uses the pack's usage only.
- **Usage:** a pack whose `usage` is absent or empty serves every usage. A pack that lists `sticker` is shown. Emoticon-only packs, unknown-only usages and malformed usage values are not shown.
- **Sticker content:** the sent `m.sticker` uses the image's `body`, or its shortcode when it has none, and its `mxc://` address. `info` carries only `w`, `h`, `size`, `mimetype` and `is_animated`. Thumbnail references and extension fields are dropped, not forwarded.
- **Threads:** a thread target is sent through the thread-focused timeline, which adds the thread relation. A room target carries no relation.
- **Cleared packs:** a cleared or redacted pack (content without `images`) is empty rather than malformed, and it takes no per-room slot.

## Limits

| Bound | Value |
| --- | --- |
| Pack state event, personal pack | 256 KiB raw JSON |
| Enabled-pack account data | 64 KiB raw JSON |
| Packs in one catalog | 32 |
| Packs per room | 16, the first by state key, chosen from headers before any content is parsed |
| Raw pack events of one type in one room | 64; a room with more has none scanned and all counted as skipped |
| Enabled rooms / enabled packs | 64 / 64 |
| Images per pack / catalog entries / rows shown | 500 / 2,000 / 300 |
| Shortcode / state key / body / info / mimetype | 100 bytes / 255 bytes / 1,024 bytes / 4 KiB / 255 bytes |
| Label / pack name / attribution | 80 / 64 / 120 characters, after removing controls, bidi and invisible formatting |
| Sent sticker content | 16 KiB |
| Preview media | 2 MiB; PNG and baseline JPEG only, at most 4,096 pixels per side and 4 megapixels |
| Search query | 100 characters |
| Queue lookup | 256 local echoes |
| Time limits | catalog 15 s in the core and 20 s in the picker; send preparation 20 s; preview fetch 45 s; decode-slot wait 5 s |

Malformed or oversized entries are left out and counted, and the picker reports that some were left out. Packs in rooms this device has not joined are shown only as a count.

## Boundaries

- **Local reads only.** Every source is read from the account's local store. Nothing joins a pack room, fetches room state from the server, or follows an HTTP or file URL. An image must be a valid `mxc://` address that passes the existing media address validator. Previews are fetched only through the existing bounded, authenticated media path.
- **Opaque UI.** The interface receives sanitized labels and pack names and per-session opaque keys. It never receives media addresses or media keys, and the catalog's debug output omits them.
- **Exact binding.** A catalog is bound to one account-selection epoch, one room, one optional thread root and one snapshot of its sources: each pack's event ID and exact content, and, for a pack enabled from another room, the whole enabling account data. Any difference revokes the choice, including enabling an unrelated pack.
- **Revalidation.** Preview and send re-read the sources and re-check the selection and pack-room membership after every await. The pack sources are read last before an action, and again after a preview's media request.
- **Consistency boundary.** These are separate local-store reads, and none is atomic with sync. A change that lands after the final read, for example while the SDK enqueues a send, is not seen by that one action. The next action, or the picker's catalog refresh, observes it.
- **Consent.** Pack images are public media even in an encrypted conversation: the homeserver can see which images are loaded and sent. For an encrypted destination, previews and sends need an explicit consent token bound to one catalog. An unknown encryption state fails closed. A rebuilt catalog needs consent again. Cancelling the prompt closes the picker. Labels stay usable before consent.
- **Admission.** Every picker command (search, navigation, pack choice, highlight, consent, send) is admitted only while the picker still matches the visible account, room, thread, composer mode and modal state. A stale picker is closed before anything is fetched, consented to or queued.
- **Revocation.** The existing account-selection revocation (account switch, protected switch, removal, detach or stop) and any change of the visible destination close the picker. Switching away and back never revives a choice, preview or consent.
- **Previews.** Each picker has one latest-only preview lane. A newer request replaces a waiting one and cancels a running fetch, so there is at most one fetch and one codec run at a time. A superseded job, or one whose picker or selection closed, is never decoded, even when the decode slot is free. The codec runs on a dedicated thread and keeps its slot until it returns.
- **Sending.** A sticker goes into the room's persistent SDK send queue, behind the existing per-room recovery and discovery gate and the pending-owner check. A blocked scope refuses it, and a thread that is waiting on a text reply refuses it. Nothing uses a direct room send or enables the account-wide queue. The text composer and its saved drafts are never touched.
- **One send at a time.** One sticker is tracked at a time. Cancel and retry are single-flight: the controls are disabled while a command runs, and the command channel holds at most one command. Retry only unwedges the same request in place.

## Uncertain sends and restarts

A send is uncertain when the SDK enqueue began but did not return within its time limit, so the request may or may not have been stored.

- **Quarantine.** The picker closes, so the choice is spent. No other sticker is sent and nothing is retried. The controller looks in the room's queue, through a read-only lookup that keeps the request's original transaction, 10 times, 3 seconds apart, for a sticker request for exactly that room or thread.
- **Found.** The request is then observed in place, with cancel and retry.
- **Not found.** The status says the sticker was either sent or never queued. Only an explicit **I checked** action lifts the quarantine; a plain dismissal does not.
- **Restart, or switching back.** The SDK store persists queued requests. After discovery covers the room, the existing gate resumes the request as its original transaction, as for attachments, and the timeline shows it as pending. Opening the picker in that room or thread finds the newest queued sticker and observes it, and counts any others. This code never sends such a request again.
- **No persistence or schema change.** The quarantine itself is in memory, and after a restart or a switch the SDK queue is the record. A request that was never stored is simply gone; a stored one is found in the queue or sent once. If the app restarts while a send is uncertain and that request was stored and then sent before the picker opens, the quarantine message is lost; the sent sticker remains visible in the conversation.

## Known gaps

- Canonical-space packs are not read.
- Pack avatars are not shown, and there is no inline custom-emoji (emoticon) insertion.
- The pre-MSC2545 legacy `short`/`emoticons` shapes are not read; only the `images` map is.
- Animated images, WebP, GIF and SVG are not previewed. Their labels are shown, and they can still be sent.
- Thumbnails are not used for previews, and extension fields such as blurhash are not forwarded.
- There is no sticker in reply or edit mode; the room picker is for new messages only.
- Pack management is not implemented: creating, editing, enabling and disabling packs happen elsewhere.
- The picker tracks one sticker at a time, although the SDK queue may hold more.
- Reads are not atomic with sync; see the consistency boundary above.

## Verification status

Nothing below has compiled or executed. Each item exists as source only.

- **Core unit tests:** the Matrix 1.19 example pack, usage rules, malformed and unsafe images, dropped thumbnails, count bounds, deduplication, sanitized labels, enabled-list parsing, consent binding, epoch, room and thread binding, header-only room-pack selection with large raw counts, exact snapshot comparison including the enabling account data, and changes during paused store reads (leaving the pack room, a changed enabling list, the selection ending).
- **Controller unit tests:** search, opaque keys, destination binding, queue status mapping, fixed failure text, and the latest-only preview lane (rapid replacements, cancelled fetches, closed contexts and a decode slot that is still taken).
- **Real-window UI scenarios:** keyboard and accessibility projection of the picker and the send status. At 200% text, in both the room composer and the narrow thread pane, every control stays inside the window and is keyboard reachable. The real controller is also driven over a recording fake core: stale commands after room, account, composer, modal, session and thread changes; persistence of an accepted send; single-flight cancel; quarantine and recovery of uncertain sends; and a sticker still queued after a restart.
- **Opt-in live fixture:** `sticker_packs_send_between_disposable_accounts` in `media_local`, with disposable local accounts and fictional content. It covers encrypted room and thread send and receive, consent, foreign and stale selections, a changed pack, an enabled and then disabled pack, an unjoined enabled room, the recovery gate for a new room, room departure and queue drain.

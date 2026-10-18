//! Native sticker catalog from Matrix image packs, and the gated sticker send.
//!
//! Sources, in the Matrix 1.19 "Image packs" order:
//! 1. packs the user enabled through `m.image_pack.rooms` account data, read
//!    only from rooms this account has already joined;
//! 2. `m.room.image_pack` state events of the destination room.
//!
//! The legacy MSC2545 shapes are read as well: `im.ponies.emote_rooms`,
//! `im.ponies.room_emotes` and the personal `im.ponies.user_emotes` pack. Matrix
//! 1.19 defines no personal pack, so only the legacy one exists; it is listed
//! first, as MSC2545 clients order it. Canonical-space packs are not read.
//!
//! Everything is read from the account's local store. Nothing here joins a
//! pack room, fetches a room's state from the server, or follows an HTTP or
//! file URL: images must be valid `mxc://` addresses and are fetched only
//! through the bounded authenticated media path.
//!
//! A catalog is bound to one account-selection epoch, destination room and
//! optional thread, and to the exact pack contents, event IDs and enabling
//! account data it was built from. Preview and send re-read those sources
//! after every await and refuse anything that changed, was redacted, disabled,
//! left or removed. The UI receives labels and indexes, never media addresses.
//!
//! Each source is a separate local-store read, and none is atomic with sync:
//! the store can change between two reads. Every read is therefore followed by
//! fresh selection and membership checks, and the pack sources are read last
//! before an action, and again after a preview's media request. A change
//! landing after that final read, for example while the SDK enqueues a send,
//! is not seen by that one action; the next action, or the catalog refresh,
//! observes it.
//!
//! Pack images are not encrypted, even in an encrypted conversation: the
//! homeserver can see which pack media is loaded or sent. Loading a preview or
//! sending into an encrypted room therefore needs an explicit consent token for
//! the exact catalog.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    Client, RoomState,
    deserialized_responses::RawAnySyncOrStrippedState,
    room::Room,
    ruma::{
        OwnedMxcUri, OwnedRoomId, RoomId, UInt,
        events::{
            AnyMessageLikeEventContent, GlobalAccountDataEventType, StateEventType,
            room::{ImageInfo, MediaSource, message::Relation},
            sticker::StickerEventContent,
        },
    },
    send_queue::LocalEchoContent,
};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::{Map, Value};

use crate::{
    AccountManager, AccountSelection, MatrixCoreError, QueuedMessage, Result, account_room, media,
    parse_event_id, require_pending_owner, resume_room_queue, selected_loaded_for,
};

/// Sticker previews are smaller than general image previews.
pub const MAX_STICKER_PREVIEW_BYTES: usize = 2 * 1024 * 1024;
/// One pack event or personal pack, as stored JSON.
const MAX_PACK_EVENT_BYTES: usize = 256 * 1024;
/// One enabled-pack list, as stored JSON.
const MAX_PACK_LIST_BYTES: usize = 64 * 1024;
const MAX_PACKS: usize = 32;
const MAX_PACKS_PER_ROOM: usize = 16;
/// A room holding more pack events of one type than this has none of them
/// scanned or parsed; they are all counted as skipped. The SDK has already
/// loaded every raw event of the type into memory by then; this bound limits
/// our own scanning, parsing and retention, not that allocation.
const MAX_RAW_PACK_EVENTS: usize = 64;
const MAX_ENABLED_ROOMS: usize = 64;
const MAX_ENABLED_PACKS: usize = 64;
const MAX_IMAGES_PER_PACK: usize = 500;
const MAX_CATALOG_ITEMS: usize = 2000;
/// Matrix 1.19: a shortcode MUST NOT exceed 100 bytes.
const MAX_SHORTCODE_BYTES: usize = 100;
const MAX_STATE_KEY_BYTES: usize = 255;
/// Bound of a pack image's `body`, which becomes the sticker's body.
const MAX_BODY_BYTES: usize = 1024;
const MAX_INFO_BYTES: usize = 4 * 1024;
const MAX_MIMETYPE_BYTES: usize = 255;
const MAX_LABEL_CHARS: usize = 80;
const MAX_PACK_NAME_CHARS: usize = 64;
const MAX_ATTRIBUTION_CHARS: usize = 120;
const MAX_STICKER_EVENT_BYTES: usize = 16 * 1024;
const BUILD_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(20);
/// Local echoes inspected when looking for a queued sticker.
const MAX_PENDING_ECHOES: usize = 256;

const ROOM_PACK: &str = "m.room.image_pack";
const ENABLED_PACKS: &str = "m.image_pack.rooms";
const LEGACY_ROOM_PACK: &str = "im.ponies.room_emotes";
const LEGACY_ENABLED_PACKS: &str = "im.ponies.emote_rooms";
const LEGACY_USER_PACK: &str = "im.ponies.user_emotes";

static CATALOG_IDS: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    /// Matrix 1.19: usage is a pack property only.
    Stable,
    /// MSC2545: an image may also narrow its own usage.
    Legacy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StickerPackSource {
    /// Defined in the destination room.
    Room,
    /// Enabled by the user for every conversation.
    Enabled,
    /// The user's personal pack (legacy format only).
    Personal,
}

/// Safe to show: sanitized, bounded text only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickerPack {
    pub name: String,
    pub attribution: Option<String>,
    pub source: StickerPackSource,
    pub stickers: usize,
}

/// Safe to show: sanitized, bounded text and an index into `packs()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickerEntry {
    /// The image's description, or its shortcode when it has none.
    pub label: String,
    pub shortcode: String,
    pub pack: usize,
}

/// The user's explicit agreement to load and send public pack media for one
/// catalog: its selection, destination and exact pack snapshot. A rebuilt
/// catalog needs a new agreement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PublicMediaConsent {
    catalog: u64,
}

/// Fetched public sticker media for a native preview. Deliberately neither
/// Debug nor Serialize; decode on the caller's bounded worker and discard.
pub struct StickerPreview {
    bytes: Vec<u8>,
}

impl StickerPreview {
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// The newest sticker request this device's SDK queue still holds for one
/// destination, for example from before a restart or after an uncertain
/// enqueue. Observing it never sends anything again: the request keeps its
/// original transaction and is sent, cancelled or retried in place.
pub struct PendingSticker {
    /// Sanitized, bounded text of the queued sticker; never its media address.
    pub label: String,
    /// The SDK recorded an unrecoverable failure; it waits for retry or cancel.
    pub wedged: bool,
    /// Other sticker requests queued for the same destination.
    pub others: usize,
    pub queued: QueuedMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PackOrigin {
    Room {
        room_id: OwnedRoomId,
        event_type: &'static str,
        state_key: String,
        event_id: String,
    },
    Personal {
        event_type: &'static str,
    },
}

/// The account data that made a pack from another room available, exactly as
/// read. Any later difference, not only removal of this pack, revokes it.
#[derive(Clone, Debug, PartialEq)]
struct EnabledBy {
    list_type: &'static str,
    list: Arc<Value>,
}

#[derive(Clone, Debug, PartialEq)]
struct PackSnapshot {
    origin: PackOrigin,
    format: Format,
    /// The pack content exactly as read; every later use compares against it.
    content: Value,
    enabled_by: Option<EnabledBy>,
}

#[derive(Clone)]
struct CatalogImage {
    pack: usize,
    shortcode: String,
    url: OwnedMxcUri,
    body: Option<String>,
    info: ImageInfo,
}

/// Packs and stickers for one destination under one account selection.
/// Holds media addresses privately; only labels and indexes are public.
pub struct StickerCatalog {
    id: u64,
    selection: AccountSelection,
    room_id: String,
    thread_root: Option<String>,
    encrypted: bool,
    packs: Vec<StickerPack>,
    snapshots: Vec<PackSnapshot>,
    images: Vec<CatalogImage>,
    entries: Vec<StickerEntry>,
    skipped: usize,
    unavailable: usize,
}

impl fmt::Debug for StickerCatalog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StickerCatalog")
            .field("id", &self.id)
            .field("packs", &self.packs.len())
            .field("stickers", &self.entries.len())
            .finish_non_exhaustive()
    }
}

impl StickerCatalog {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn account_id(&self) -> &str {
        self.selection.account_id()
    }

    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    pub fn thread_root(&self) -> Option<&str> {
        self.thread_root.as_deref()
    }

    pub fn packs(&self) -> &[StickerPack] {
        &self.packs
    }

    pub fn entries(&self) -> &[StickerEntry] {
        &self.entries
    }

    /// Images or packs left out because they were malformed or over a limit.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// Enabled packs in rooms this device has not joined, or that have no
    /// readable pack event. They are never joined or fetched.
    pub fn unavailable_packs(&self) -> usize {
        self.unavailable
    }

    /// True when the destination was encrypted while the catalog was built.
    /// An action re-checks the room and can still require consent later.
    pub fn requires_public_media_consent(&self) -> bool {
        self.encrypted
    }

    /// Record the user's explicit agreement for this catalog only. Call this
    /// solely from a deliberate user action on the consent prompt.
    pub fn public_media_consent(&self) -> PublicMediaConsent {
        PublicMediaConsent { catalog: self.id }
    }

    /// Whether this catalog belongs to exactly this selection epoch, room and
    /// thread. Matching account or room IDs alone are not enough.
    pub fn is_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
    ) -> bool {
        self.selection.is_current()
            && self.selection.same_epoch(selection)
            && self.room_id == room_id
            && self.thread_root.as_deref() == thread_root
    }

    fn consent_matches(&self, consent: Option<&PublicMediaConsent>) -> bool {
        consent.is_some_and(|consent| consent.catalog == self.id)
    }
}

impl AccountManager {
    /// Build the sticker catalog for a joined room, or one thread in it.
    pub async fn sticker_catalog_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
    ) -> Result<StickerCatalog> {
        if let Some(root) = thread_root {
            parse_event_id(root)?;
        }
        tokio::time::timeout(
            BUILD_TIMEOUT,
            self.build_sticker_catalog(selection, room_id, thread_root),
        )
        .await
        .map_err(|_| MatrixCoreError::Sdk("Loading sticker packs timed out.".into()))?
    }

    /// `None` while every source still matches; otherwise a rebuilt catalog
    /// for the same destination, which needs its own consent. An added or
    /// newly enabled pack also counts as a change.
    pub async fn refresh_sticker_catalog(
        &self,
        catalog: &StickerCatalog,
    ) -> Result<Option<StickerCatalog>> {
        let fresh = self
            .sticker_catalog_for(&catalog.selection, &catalog.room_id, catalog.thread_root())
            .await?;
        let unchanged = fresh.snapshots == catalog.snapshots
            && fresh.encrypted == catalog.encrypted
            && fresh.skipped == catalog.skipped
            && fresh.unavailable == catalog.unavailable;
        Ok((!unchanged).then_some(fresh))
    }

    /// Fetch one sticker image through the bounded authenticated media path,
    /// after re-checking the selection, room, encryption consent and the
    /// sticker's exact pack source, before and after the network request.
    pub async fn download_sticker_preview_for(
        &self,
        catalog: &StickerCatalog,
        index: usize,
        consent: Option<&PublicMediaConsent>,
    ) -> Result<StickerPreview> {
        let (room, client) = self
            .sticker_room(&catalog.selection, &catalog.room_id)
            .await?;
        require_consent(catalog, &room, consent).await?;
        let image = validated_choice(catalog, index, &room, &client).await?;
        if let Some(size) = image.info.size {
            let declared = u64::from(size);
            if declared > MAX_STICKER_PREVIEW_BYTES as u64 {
                return Err(MatrixCoreError::MediaTooLarge {
                    maximum: MAX_STICKER_PREVIEW_BYTES as u64,
                    actual: declared,
                });
            }
        }
        let source = MediaSource::Plain(image.url.clone());
        let bytes =
            media::fetch_bounded_media(&client, &source, MAX_STICKER_PREVIEW_BYTES).await?;
        // Nothing fetched may be presented for a departed selection, room,
        // consent decision or pack.
        let (room, client) = self
            .sticker_room(&catalog.selection, &catalog.room_id)
            .await?;
        require_consent(catalog, &room, consent).await?;
        validated_choice(catalog, index, &room, &client).await?;
        current(&catalog.selection)?;
        Ok(StickerPreview { bytes })
    }

    /// Queue one sticker through the room's persistent SDK send queue, under
    /// the same discovery/recovery gate as text. A thread target goes through
    /// the thread-focused timeline, which adds the thread relation. The text
    /// composer and its saved drafts are never touched.
    ///
    /// The switch lock is held from validation through enqueue; every await
    /// is followed by a fresh check of the selection, the joined room, the
    /// consent decision and the exact pack source. Once SDK enqueue has begun,
    /// a timeout reports an uncertain send and nothing is ever re-enqueued.
    pub async fn send_sticker_for(
        &self,
        catalog: &StickerCatalog,
        index: usize,
        consent: Option<&PublicMediaConsent>,
    ) -> Result<QueuedMessage> {
        let entered = AtomicBool::new(false);
        tokio::time::timeout(SEND_TIMEOUT, async {
            let selection = &catalog.selection;
            self.require_selection(selection)?;
            let state = self.state.lock().await;
            let loaded = selected_loaded_for(&state, selection.account_id())?;
            let room = account_room(&state, selection.account_id(), &catalog.room_id)?;
            current(selection)?;
            joined(&room)?;
            require_consent(catalog, &room, consent).await?;
            let content = sticker_content(
                validated_choice(catalog, index, &room, &loaded.client).await?,
            )?;
            let timeline = match &catalog.thread_root {
                Some(root) => Some(loaded.thread_timeline(&catalog.room_id, root).await?),
                None => None,
            };
            current(selection)?;
            joined(&room)?;
            resume_room_queue(loaded, &room).await?;
            let queue = room.send_queue();
            let (_, updates) = queue.subscribe().await.map_err(|_| unavailable())?;
            // A blocked scope refuses, and a thread with a waiting text reply
            // stays limited to that one waiting reply.
            require_pending_owner(loaded, &catalog.room_id, catalog.thread_root(), None).await?;
            // Final re-reads after every await above: nothing changed is
            // queued. The pack sources are read last; separate store reads
            // cannot be atomic with sync, so a change landing between this
            // final read and the SDK's own enqueue is not seen by this send.
            require_consent(catalog, &room, consent).await?;
            validated_choice(catalog, index, &room, &loaded.client).await?;
            current(selection)?;
            joined(&room)?;
            entered.store(true, Ordering::Release);
            let event = AnyMessageLikeEventContent::Sticker(content);
            let handle = match timeline {
                Some(timeline) => timeline.send(event).await.map_err(|_| uncertain())?,
                None => queue.send(event).await.map_err(|_| uncertain())?,
            };
            Ok(QueuedMessage {
                transaction_id: handle.transaction_id().to_string(),
                handle,
                updates,
                finished: false,
            })
        })
        .await
        .map_err(|_| {
            if entered.load(Ordering::Acquire) {
                MatrixCoreError::ComposedSendUncertain
            } else {
                MatrixCoreError::Sdk("Sticker preparation timed out before it was queued.".into())
            }
        })?
    }

    /// Find the newest sticker request still held by the room's persistent
    /// queue for exactly this room or thread. Read-only: nothing is queued,
    /// resumed or replayed, and the room's recovery gate is not touched. The
    /// returned observer subscribed together with the queue snapshot, so no
    /// later update for it can be missed.
    pub async fn pending_sticker_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
    ) -> Result<Option<PendingSticker>> {
        self.require_selection(selection)?;
        let room = {
            let state = self.state.lock().await;
            selected_loaded_for(&state, selection.account_id())?;
            account_room(&state, selection.account_id(), room_id)?
        };
        let (echoes, updates) = room
            .send_queue()
            .subscribe()
            .await
            .map_err(|_| unavailable())?;
        current(selection)?;
        let mut found = Vec::new();
        for echo in echoes.into_iter().take(MAX_PENDING_ECHOES) {
            let LocalEchoContent::Event {
                serialized_event,
                send_handle,
                send_error,
            } = echo.content
            else {
                continue;
            };
            let Ok(AnyMessageLikeEventContent::Sticker(sticker)) = serialized_event.deserialize()
            else {
                continue;
            };
            if in_scope(&sticker, thread_root) {
                let label = display_text(&sticker.body, MAX_LABEL_CHARS);
                let wedged = send_error.is_some();
                found.push((echo.transaction_id, send_handle, wedged, label));
            }
        }
        let others = found.len().saturating_sub(1);
        let Some((transaction_id, handle, wedged, label)) = found.pop() else {
            return Ok(None);
        };
        Ok(Some(PendingSticker {
            label,
            wedged,
            others,
            queued: QueuedMessage {
                transaction_id: transaction_id.to_string(),
                handle,
                updates,
                finished: false,
            },
        }))
    }

    fn require_selection(&self, selection: &AccountSelection) -> Result<()> {
        if selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| current.same_epoch(selection))
        {
            Ok(())
        } else {
            Err(MatrixCoreError::SelectedAccountChanged)
        }
    }

    /// The joined destination room of this exact selection epoch. Callers run
    /// it again after awaiting before trusting earlier reads.
    async fn sticker_room(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> Result<(Room, Client)> {
        self.require_selection(selection)?;
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, selection.account_id())?;
        let room = account_room(&state, selection.account_id(), room_id)?;
        current(selection)?;
        joined(&room)?;
        Ok((room, loaded.client.clone()))
    }

    async fn build_sticker_catalog(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
    ) -> Result<StickerCatalog> {
        let (room, client) = self.sticker_room(selection, room_id).await?;
        let encryption = room
            .latest_encryption_state()
            .await
            .map_err(|_| encryption_unknown())?;
        current(selection)?;
        if encryption.is_unknown() {
            return Err(encryption_unknown());
        }
        let mut builder = Builder::default();
        match account_content(&client, LEGACY_USER_PACK, MAX_PACK_EVENT_BYTES).await? {
            AccountContent::Present(content) => builder.add(
                PackOrigin::Personal {
                    event_type: LEGACY_USER_PACK,
                },
                Format::Legacy,
                content,
                StickerPackSource::Personal,
                None,
                "Your stickers".into(),
            ),
            AccountContent::TooLarge => builder.skipped += 1,
            AccountContent::Absent => {}
        }
        current(selection)?;
        for (list_type, pack_type, format) in [
            (ENABLED_PACKS, ROOM_PACK, Format::Stable),
            (LEGACY_ENABLED_PACKS, LEGACY_ROOM_PACK, Format::Legacy),
        ] {
            let list = match account_content(&client, list_type, MAX_PACK_LIST_BYTES).await? {
                AccountContent::Present(list) => Arc::new(list),
                AccountContent::TooLarge => {
                    builder.skipped += 1;
                    continue;
                }
                AccountContent::Absent => continue,
            };
            current(selection)?;
            let (references, skipped) = enabled_references(&list);
            builder.skipped += skipped;
            for (pack_room_id, state_key) in references {
                // Never join or fetch: a pack in a room this device has not
                // joined is reported as unavailable instead.
                let Some(pack_room) = client
                    .get_room(&pack_room_id)
                    .filter(|room| room.state() == RoomState::Joined)
                else {
                    builder.unavailable += 1;
                    continue;
                };
                let raw = pack_room
                    .get_state_event(StateEventType::from(pack_type), &state_key)
                    .await
                    .map_err(|_| unavailable())?;
                current(selection)?;
                // The room may have been left while its state was read.
                if pack_room.state() != RoomState::Joined {
                    builder.unavailable += 1;
                    continue;
                }
                let Some((key, event_id, content)) = raw.as_ref().and_then(state_content) else {
                    builder.unavailable += 1;
                    continue;
                };
                if key != state_key {
                    builder.skipped += 1;
                    continue;
                }
                let in_destination = pack_room.room_id() == room.room_id();
                let enabled_by = (!in_destination).then(|| EnabledBy {
                    list_type,
                    list: list.clone(),
                });
                builder.add(
                    PackOrigin::Room {
                        room_id: pack_room_id.clone(),
                        event_type: pack_type,
                        state_key,
                        event_id,
                    },
                    format,
                    content,
                    if in_destination {
                        StickerPackSource::Room
                    } else {
                        StickerPackSource::Enabled
                    },
                    enabled_by,
                    room_name(&pack_room),
                );
            }
        }
        for (pack_type, format) in [(ROOM_PACK, Format::Stable), (LEGACY_ROOM_PACK, Format::Legacy)]
        {
            let events = room
                .get_state_events(StateEventType::from(pack_type))
                .await
                .map_err(|_| unavailable())?;
            current(selection)?;
            let (packs, skipped) = bounded_room_packs(&events);
            builder.skipped += skipped;
            for (state_key, event_id, content) in packs {
                builder.add(
                    PackOrigin::Room {
                        room_id: room.room_id().to_owned(),
                        event_type: pack_type,
                        state_key,
                        event_id,
                    },
                    format,
                    content,
                    StickerPackSource::Room,
                    None,
                    room_name(&room),
                );
            }
        }
        // Re-establish the selection, account and joined room after the reads.
        self.sticker_room(selection, room_id).await?;
        Ok(builder.finish(
            selection.clone(),
            room_id.to_owned(),
            thread_root.map(str::to_owned),
            encryption.is_encrypted(),
        ))
    }
}

/// A room destination holds stickers without a thread relation; a thread
/// destination holds stickers related to exactly its root.
fn in_scope(sticker: &StickerEventContent, thread_root: Option<&str>) -> bool {
    match (&sticker.relates_to, thread_root) {
        (Some(Relation::Thread(thread)), Some(root)) => thread.event_id.as_str() == root,
        (Some(Relation::Thread(_)), None) | (_, Some(_)) => false,
        (_, None) => true,
    }
}

fn current(selection: &AccountSelection) -> Result<()> {
    if selection.is_current() {
        Ok(())
    } else {
        Err(MatrixCoreError::SelectedAccountChanged)
    }
}

fn joined(room: &Room) -> Result<()> {
    if room.state() == RoomState::Joined {
        Ok(())
    } else {
        Err(unavailable())
    }
}

fn unavailable() -> MatrixCoreError {
    MatrixCoreError::StickerUnavailable
}

fn uncertain() -> MatrixCoreError {
    MatrixCoreError::ComposedSendUncertain
}

fn encryption_unknown() -> MatrixCoreError {
    MatrixCoreError::InvalidMedia("Cannot check room encryption".into())
}

/// An encrypted destination, now or when the catalog was built, needs the
/// consent recorded for this exact catalog. Unknown encryption fails closed.
async fn require_consent(
    catalog: &StickerCatalog,
    room: &Room,
    consent: Option<&PublicMediaConsent>,
) -> Result<()> {
    let encryption = room
        .latest_encryption_state()
        .await
        .map_err(|_| encryption_unknown())?;
    current(&catalog.selection)?;
    if encryption.is_unknown() {
        return Err(encryption_unknown());
    }
    if (encryption.is_encrypted() || catalog.encrypted) && !catalog.consent_matches(consent) {
        return Err(MatrixCoreError::StickerConsentRequired);
    }
    Ok(())
}

/// Re-read the chosen sticker's pack source and require it unchanged.
async fn validated_choice<'a>(
    catalog: &'a StickerCatalog,
    index: usize,
    room: &Room,
    client: &Client,
) -> Result<&'a CatalogImage> {
    let image = catalog.images.get(index).ok_or_else(unavailable)?;
    let snapshot = catalog.snapshots.get(image.pack).ok_or_else(unavailable)?;
    snapshot_current(&StoreReader(client), &catalog.selection, snapshot).await?;
    current(&catalog.selection)?;
    joined(room)?;
    Ok(image)
}

/// `(state_key, event_id, content)` of one pack state event.
type StateContent = (String, String, Value);

/// The local-store reads that revalidation needs. Each read is a separate
/// store access and none is atomic with sync, so every read is followed by
/// fresh checks of the selection and of the pack room's membership.
trait PackReader {
    fn joined(&self, room_id: &RoomId) -> bool;

    fn state(
        &self,
        room_id: &RoomId,
        event_type: &'static str,
        state_key: &str,
    ) -> impl Future<Output = Result<Option<StateContent>>> + Send;

    fn account(
        &self,
        event_type: &'static str,
        maximum: usize,
    ) -> impl Future<Output = Result<AccountContent>> + Send;
}

struct StoreReader<'a>(&'a Client);

impl PackReader for StoreReader<'_> {
    fn joined(&self, room_id: &RoomId) -> bool {
        self.0
            .get_room(room_id)
            .is_some_and(|room| room.state() == RoomState::Joined)
    }

    async fn state(
        &self,
        room_id: &RoomId,
        event_type: &'static str,
        state_key: &str,
    ) -> Result<Option<StateContent>> {
        let Some(room) = self.0.get_room(room_id) else {
            return Ok(None);
        };
        let raw = room
            .get_state_event(StateEventType::from(event_type), state_key)
            .await
            .map_err(|_| unavailable())?;
        Ok(raw.as_ref().and_then(state_content))
    }

    async fn account(&self, event_type: &'static str, maximum: usize) -> Result<AccountContent> {
        account_content(self.0, event_type, maximum).await
    }
}

/// Re-read one pack's sources and require them exactly unchanged: the pack
/// event and its content, and the whole enabling account data when another
/// room's pack was enabled. Each read is followed by fresh selection and pack
/// room membership checks. A change landing after the final check, while the
/// caller completes its action, is outside what separate store reads exclude.
async fn snapshot_current(
    reader: &impl PackReader,
    selection: &AccountSelection,
    snapshot: &PackSnapshot,
) -> Result<()> {
    match &snapshot.origin {
        PackOrigin::Room {
            room_id,
            event_type,
            state_key,
            event_id,
        } => {
            pack_room_joined(reader, room_id)?;
            let read = reader.state(room_id, *event_type, state_key).await?;
            current(selection)?;
            pack_room_joined(reader, room_id)?;
            let (key, id, content) = read.ok_or_else(unavailable)?;
            if &key != state_key || &id != event_id || content != snapshot.content {
                return Err(unavailable());
            }
        }
        PackOrigin::Personal { event_type } => {
            let read = reader.account(*event_type, MAX_PACK_EVENT_BYTES).await?;
            current(selection)?;
            if !matches!(read, AccountContent::Present(content) if content == snapshot.content) {
                return Err(unavailable());
            }
        }
    }
    if let Some(enabled) = &snapshot.enabled_by {
        let read = reader
            .account(enabled.list_type, MAX_PACK_LIST_BYTES)
            .await?;
        current(selection)?;
        if !matches!(read, AccountContent::Present(list) if list == *enabled.list) {
            return Err(unavailable());
        }
        // The pack room may have been left while the list was read.
        if let PackOrigin::Room { room_id, .. } = &snapshot.origin {
            pack_room_joined(reader, room_id)?;
        }
    }
    Ok(())
}

fn pack_room_joined(reader: &impl PackReader, room_id: &RoomId) -> Result<()> {
    if reader.joined(room_id) {
        Ok(())
    } else {
        Err(unavailable())
    }
}

/// The header of a room pack event: enough to choose and order it without
/// materializing its content.
#[derive(Deserialize)]
struct PackHeader {
    state_key: String,
    content: PackPresence,
}

#[derive(Deserialize)]
struct PackPresence {
    images: Option<IgnoredAny>,
}

/// At most `MAX_PACKS_PER_ROOM` room packs, the first by state key, with the
/// number refused. They are chosen from bounded headers before any content is
/// parsed, and only the chosen events are then read in full.
fn bounded_room_packs(events: &[RawAnySyncOrStrippedState]) -> (Vec<StateContent>, usize) {
    if events.len() > MAX_RAW_PACK_EVENTS {
        return (Vec::new(), events.len());
    }
    let raw: Vec<Option<&str>> = events
        .iter()
        .map(|event| match event {
            RawAnySyncOrStrippedState::Sync(raw) => Some(raw.json().get()),
            RawAnySyncOrStrippedState::Stripped(_) => None,
        })
        .collect();
    let (chosen, mut skipped) = choose_room_packs(&raw);
    let mut packs = Vec::with_capacity(chosen.len());
    for index in chosen {
        match state_content(&events[index]) {
            Some(pack) => packs.push(pack),
            None => skipped += 1,
        }
    }
    (packs, skipped)
}

/// Indexes of the room pack events to read in full, in state-key order, and
/// the number refused. Only headers are parsed, never more than
/// `MAX_RAW_PACK_EVENTS` of them; cleared or redacted packs hold no slot.
fn choose_room_packs(events: &[Option<&str>]) -> (Vec<usize>, usize) {
    if events.len() > MAX_RAW_PACK_EVENTS {
        return (Vec::new(), events.len());
    }
    let mut chosen = Vec::new();
    let mut skipped = 0;
    for (index, json) in events.iter().copied().enumerate() {
        let header = json
            .filter(|json| json.len() <= MAX_PACK_EVENT_BYTES)
            .and_then(|json| serde_json::from_str::<PackHeader>(json).ok())
            .filter(|header| header.state_key.len() <= MAX_STATE_KEY_BYTES);
        match header {
            Some(header) if header.content.images.is_none() => {}
            Some(header) => chosen.push((header.state_key, index)),
            None => skipped += 1,
        }
    }
    chosen.sort();
    if chosen.len() > MAX_PACKS_PER_ROOM {
        skipped += chosen.len() - MAX_PACKS_PER_ROOM;
        chosen.truncate(MAX_PACKS_PER_ROOM);
    }
    (chosen.into_iter().map(|(_, index)| index).collect(), skipped)
}

enum AccountContent {
    Absent,
    TooLarge,
    Present(Value),
}

async fn account_content(
    client: &Client,
    event_type: &'static str,
    maximum: usize,
) -> Result<AccountContent> {
    let raw = client
        .account()
        .account_data_raw(GlobalAccountDataEventType::from(event_type))
        .await
        .map_err(|_| unavailable())?;
    let Some(raw) = raw else {
        return Ok(AccountContent::Absent);
    };
    if raw.json().get().len() > maximum {
        return Ok(AccountContent::TooLarge);
    }
    Ok(serde_json::from_str::<Value>(raw.json().get())
        .map(AccountContent::Present)
        .unwrap_or(AccountContent::Absent))
}

/// `(state_key, event_id, content)` of a bounded joined-room state event.
/// Stripped invite state is never used.
fn state_content(raw: &RawAnySyncOrStrippedState) -> Option<(String, String, Value)> {
    let RawAnySyncOrStrippedState::Sync(raw) = raw else {
        return None;
    };
    if raw.json().get().len() > MAX_PACK_EVENT_BYTES {
        return None;
    }
    let state_key = raw.get_field::<String>("state_key").ok()??;
    if state_key.len() > MAX_STATE_KEY_BYTES {
        return None;
    }
    let event_id = raw.get_field::<String>("event_id").ok()??;
    let content = raw.get_field::<Value>("content").ok()??;
    Some((state_key, event_id, content))
}

/// Bounded `(room, state key)` references from an enabled-pack list. The
/// bottom-level objects are reserved; anything else is skipped and counted.
fn enabled_references(list: &Value) -> (Vec<(OwnedRoomId, String)>, usize) {
    let mut references = Vec::new();
    let mut skipped = 0;
    let Some(rooms) = list.get("rooms").and_then(Value::as_object) else {
        return (references, usize::from(list.get("rooms").is_some()));
    };
    for (index, (room_id, keys)) in rooms.iter().enumerate() {
        let (Ok(room_id), Some(keys)) = (RoomId::parse(room_id), keys.as_object()) else {
            skipped += 1;
            continue;
        };
        if index >= MAX_ENABLED_ROOMS {
            skipped += keys.len().max(1);
            continue;
        }
        let mut in_room = 0;
        for (state_key, meta) in keys {
            if !meta.is_object()
                || state_key.len() > MAX_STATE_KEY_BYTES
                || in_room >= MAX_PACKS_PER_ROOM
                || references.len() >= MAX_ENABLED_PACKS
            {
                skipped += 1;
                continue;
            }
            in_room += 1;
            references.push((room_id.clone(), state_key.clone()));
        }
    }
    (references, skipped)
}

fn room_name(room: &Room) -> String {
    room.name()
        .map(|name| display_text(&name, MAX_PACK_NAME_CHARS))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Room stickers".into())
}

struct ParsedImage {
    shortcode: String,
    url: OwnedMxcUri,
    body: Option<String>,
    info: ImageInfo,
}

struct ParsedPack {
    display_name: Option<String>,
    attribution: Option<String>,
    images: Vec<ParsedImage>,
    skipped: usize,
}

/// Matrix 1.19: a pack whose usage is absent or empty serves every usage.
/// Unknown usages alone, or a malformed usage, do not make a sticker pack.
fn usage_allows_sticker(usage: Option<&Value>) -> bool {
    match usage {
        None | Some(Value::Null) => true,
        Some(Value::Array(values)) => {
            values.is_empty() || values.iter().any(|value| value.as_str() == Some("sticker"))
        }
        Some(_) => false,
    }
}

fn parse_pack(content: &Value, format: Format) -> Option<ParsedPack> {
    let content = content.as_object()?;
    let empty = Map::new();
    let images = match content.get("images") {
        // A cleared or redacted pack (`{}`) is empty rather than malformed.
        None => &empty,
        Some(images) => images.as_object()?,
    };
    let meta = content.get("pack").and_then(Value::as_object);
    let pack_allows = usage_allows_sticker(meta.and_then(|meta| meta.get("usage")));
    let text = |field: &str, maximum: usize| {
        meta.and_then(|meta| meta.get(field))
            .and_then(Value::as_str)
            .map(|value| display_text(value, maximum))
            .filter(|value| !value.is_empty())
    };
    let mut parsed = Vec::new();
    let mut skipped = 0;
    for (shortcode, image) in images {
        let allowed = match (format, image.get("usage")) {
            (Format::Legacy, Some(usage)) => usage_allows_sticker(Some(usage)),
            _ => pack_allows,
        };
        if !allowed {
            continue;
        }
        if parsed.len() >= MAX_IMAGES_PER_PACK {
            skipped += 1;
            continue;
        }
        match parse_image(shortcode, image) {
            Some(image) => parsed.push(image),
            None => skipped += 1,
        }
    }
    parsed.sort_by(|a, b| a.shortcode.cmp(&b.shortcode));
    Some(ParsedPack {
        display_name: text("display_name", MAX_PACK_NAME_CHARS),
        attribution: text("attribution", MAX_ATTRIBUTION_CHARS),
        images: parsed,
        skipped,
    })
}

/// A malformed shortcode is still shown (Matrix 1.19 asks clients to render
/// them), but it must be bounded and free of control characters.
fn parse_image(shortcode: &str, image: &Value) -> Option<ParsedImage> {
    if shortcode.trim().is_empty()
        || shortcode.len() > MAX_SHORTCODE_BYTES
        || shortcode.chars().any(char::is_control)
    {
        return None;
    }
    let image = image.as_object()?;
    let url = image.get("url")?.as_str()?;
    if !url.starts_with("mxc://") {
        return None;
    }
    let url = OwnedMxcUri::from(url);
    media::validate_media_address(&url).ok()?;
    let body = match image.get("body") {
        None | Some(Value::Null) => None,
        Some(Value::String(body)) if body.len() <= MAX_BODY_BYTES => {
            Some(body.clone()).filter(|body| !body.trim().is_empty())
        }
        Some(_) => return None,
    };
    let info = match image.get("info") {
        None | Some(Value::Null) => ImageInfo::new(),
        Some(Value::Object(info)) => image_info(info)?,
        Some(_) => return None,
    };
    Some(ParsedImage {
        shortcode: shortcode.to_owned(),
        url,
        body,
        info,
    })
}

/// Only the standard image fields are carried into a sent sticker. Thumbnail
/// references and extension fields are dropped rather than forwarded.
fn image_info(info: &Map<String, Value>) -> Option<ImageInfo> {
    if serde_json::to_vec(info).ok()?.len() > MAX_INFO_BYTES {
        return None;
    }
    let mut parsed = ImageInfo::new();
    parsed.width = optional_uint(info.get("w"))?;
    parsed.height = optional_uint(info.get("h"))?;
    parsed.size = optional_uint(info.get("size"))?;
    parsed.mimetype = match info.get("mimetype") {
        None | Some(Value::Null) => None,
        Some(Value::String(mimetype))
            if mimetype.len() <= MAX_MIMETYPE_BYTES
                && mimetype.starts_with("image/")
                && mimetype.parse::<mime::Mime>().is_ok() =>
        {
            Some(mimetype.clone())
        }
        Some(_) => return None,
    };
    parsed.is_animated = match info.get("is_animated") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(animated)) => Some(*animated),
        Some(_) => return None,
    };
    Some(parsed)
}

fn optional_uint(value: Option<&Value>) -> Option<Option<UInt>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(value) => value.as_u64().and_then(UInt::new).map(Some),
    }
}

/// One line of display text: no controls, bidi overrides or invisible
/// formatting, collapsed whitespace and a character bound.
fn display_text(source: &str, maximum: usize) -> String {
    let mut text = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for ch in source.chars() {
        if ch.is_whitespace() {
            pending_space = !text.is_empty();
            continue;
        }
        if ch.is_control()
            || matches!(ch,
                '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}' | '\u{feff}' | '\u{fff9}'..='\u{fffc}')
        {
            continue;
        }
        let needed = usize::from(pending_space) + 1;
        if count + needed > maximum {
            text.push('…');
            return text;
        }
        if pending_space {
            text.push(' ');
            count += 1;
            pending_space = false;
        }
        text.push(ch);
        count += 1;
    }
    text
}

/// Matrix 1.19: the sticker body is the image's body, or its shortcode.
fn sticker_content(image: &CatalogImage) -> Result<StickerEventContent> {
    let body = image.body.as_ref().unwrap_or(&image.shortcode).clone();
    let content = StickerEventContent::new(body, image.info.clone(), image.url.clone());
    let encoded = serde_json::to_vec(&content).map_err(|_| unavailable())?;
    if encoded.len() > MAX_STICKER_EVENT_BYTES {
        return Err(unavailable());
    }
    Ok(content)
}

#[derive(Default)]
struct Builder {
    packs: Vec<StickerPack>,
    snapshots: Vec<PackSnapshot>,
    images: Vec<CatalogImage>,
    seen: HashMap<(OwnedRoomId, &'static str, String), usize>,
    skipped: usize,
    unavailable: usize,
}

impl Builder {
    fn add(
        &mut self,
        origin: PackOrigin,
        format: Format,
        content: Value,
        source: StickerPackSource,
        enabled_by: Option<EnabledBy>,
        fallback_name: String,
    ) {
        let key = match &origin {
            PackOrigin::Room {
                room_id,
                event_type,
                state_key,
                ..
            } => Some((room_id.clone(), *event_type, state_key.clone())),
            PackOrigin::Personal { .. } => None,
        };
        if let Some(index) = key.as_ref().and_then(|key| self.seen.get(key)).copied() {
            // A pack of the destination room needs no enabling reference.
            if enabled_by.is_none() {
                self.snapshots[index].enabled_by = None;
                self.packs[index].source = source;
            }
            return;
        }
        if self.packs.len() >= MAX_PACKS {
            self.skipped += 1;
            return;
        }
        let Some(parsed) = parse_pack(&content, format) else {
            self.skipped += 1;
            return;
        };
        self.skipped += parsed.skipped;
        let index = self.packs.len();
        let mut added = 0;
        for image in parsed.images {
            if self.images.len() >= MAX_CATALOG_ITEMS {
                self.skipped += 1;
                continue;
            }
            self.images.push(CatalogImage {
                pack: index,
                shortcode: image.shortcode,
                url: image.url,
                body: image.body,
                info: image.info,
            });
            added += 1;
        }
        if added == 0 {
            return;
        }
        if let Some(key) = key {
            self.seen.insert(key, index);
        }
        self.packs.push(StickerPack {
            name: parsed.display_name.unwrap_or(fallback_name),
            attribution: parsed.attribution,
            source,
            stickers: added,
        });
        self.snapshots.push(PackSnapshot {
            origin,
            format,
            content,
            enabled_by,
        });
    }

    fn finish(
        self,
        selection: AccountSelection,
        room_id: String,
        thread_root: Option<String>,
        encrypted: bool,
    ) -> StickerCatalog {
        let entries = self
            .images
            .iter()
            .map(|image| {
                let shortcode = display_text(&image.shortcode, MAX_LABEL_CHARS);
                let label = image
                    .body
                    .as_deref()
                    .map(|body| display_text(body, MAX_LABEL_CHARS))
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| shortcode.clone());
                StickerEntry {
                    label,
                    shortcode,
                    pack: image.pack,
                }
            })
            .collect();
        StickerCatalog {
            id: CATALOG_IDS.fetch_add(1, Ordering::Relaxed),
            selection,
            room_id,
            thread_root,
            encrypted,
            packs: self.packs,
            snapshots: self.snapshots,
            images: self.images,
            entries,
            skipped: self.skipped,
            unavailable: self.unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_selection::Selections;
    use matrix_sdk::ruma::serde::Raw;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::sync::oneshot;

    fn spec_example() -> Value {
        json!({
            "images": {
                "cat_wave": {
                    "url": "mxc://example.org/abc123",
                    "body": "a waving cat",
                    "info": {"mimetype": "image/png", "w": 512, "h": 512, "size": 84400}
                },
                "cat_nap": {"url": "mxc://example.org/def456", "body": "a sleeping cat"}
            },
            "pack": {
                "display_name": "Cats",
                "avatar_url": "mxc://example.org/abc123",
                "attribution": "drawn by a fictional artist"
            }
        })
    }

    fn room_origin(room: &str, key: &str, event: &str) -> PackOrigin {
        PackOrigin::Room {
            room_id: RoomId::parse(room).unwrap(),
            event_type: ROOM_PACK,
            state_key: key.into(),
            event_id: event.into(),
        }
    }

    fn enabled(list: Value) -> EnabledBy {
        EnabledBy {
            list_type: ENABLED_PACKS,
            list: Arc::new(list),
        }
    }

    fn catalog(builder: Builder, encrypted: bool) -> (Selections, StickerCatalog) {
        let selections = Selections::default();
        selections.replace(Some("robin-local"));
        let selection = selections.for_account("robin-local").unwrap();
        let catalog = builder.finish(selection, "!dest:example.org".into(), None, encrypted);
        (selections, catalog)
    }

    #[test]
    fn stable_example_pack_parses_in_shortcode_order_with_projected_info() {
        let pack = parse_pack(&spec_example(), Format::Stable).unwrap();
        assert_eq!(pack.display_name.as_deref(), Some("Cats"));
        let attribution = pack.attribution.as_deref();
        assert_eq!(attribution, Some("drawn by a fictional artist"));
        assert_eq!(pack.skipped, 0);
        let shortcodes: Vec<_> = pack
            .images
            .iter()
            .map(|image| image.shortcode.as_str())
            .collect();
        assert_eq!(shortcodes, ["cat_nap", "cat_wave"]);
        let wave = &pack.images[1];
        assert_eq!(wave.body.as_deref(), Some("a waving cat"));
        assert_eq!(wave.info.mimetype.as_deref(), Some("image/png"));
        assert_eq!(wave.info.width, UInt::new(512));
        assert_eq!(wave.info.size, UInt::new(84_400));
        assert!(pack.images[0].info.mimetype.is_none());
    }

    #[test]
    fn pack_usage_selects_sticker_packs_and_legacy_images_may_narrow_it() {
        for (usage, expected) in [
            (json!(null), 2),
            (json!([]), 2),
            (json!(["sticker"]), 2),
            (json!(["emoticon", "sticker"]), 2),
            (json!(["emoticon"]), 0),
            (json!(["future_usage"]), 0),
            (json!("sticker"), 0),
        ] {
            let mut content = spec_example();
            content["pack"]["usage"] = usage.clone();
            for format in [Format::Stable, Format::Legacy] {
                let pack = parse_pack(&content, format).unwrap();
                assert_eq!(pack.images.len(), expected, "usage {usage} {format:?}");
            }
        }
        let mut content = spec_example();
        content["pack"]["usage"] = json!(["sticker"]);
        content["images"]["cat_nap"]["usage"] = json!(["emoticon"]);
        // The stable schema has no per-image usage; MSC2545 does.
        let stable = parse_pack(&content, Format::Stable).unwrap();
        assert_eq!(stable.images.len(), 2);
        let legacy = parse_pack(&content, Format::Legacy).unwrap();
        assert_eq!(legacy.images.len(), 1);
        assert_eq!(legacy.images[0].shortcode, "cat_wave");
    }

    #[test]
    fn unsafe_or_malformed_images_are_skipped_and_counted() {
        let long = "x".repeat(MAX_SHORTCODE_BYTES + 1);
        let mut content = json!({
            "images": {
                "ok": {"url": "mxc://example.org/ok"},
                "https": {"url": "https://example.org/image.png"},
                "file": {"url": "file:///private/image.png"},
                "no_media": {"url": "mxc://example.org"},
                "path": {"url": "mxc://example.org/a/b"},
                "missing": {"body": "no url"},
                "body_type": {"url": "mxc://example.org/a", "body": 7},
                "long_body": {"url": "mxc://example.org/b", "body": "y".repeat(MAX_BODY_BYTES + 1)},
                "info_type": {"url": "mxc://example.org/c", "info": "large"},
                "not_image": {"url": "mxc://example.org/d", "info": {"mimetype": "text/html"}},
                "negative": {"url": "mxc://example.org/e", "info": {"w": -1}},
                "animated": {"url": "mxc://example.org/f", "info": {"is_animated": "yes"}},
                "ctl\u{7}": {"url": "mxc://example.org/g"},
                "not:grammar but shown": {"url": "mxc://example.org/i"},
                "list": ["mxc://example.org/j"]
            }
        });
        content["images"][long.as_str()] = json!({"url": "mxc://example.org/h"});
        let pack = parse_pack(&content, Format::Stable).unwrap();
        let kept: Vec<_> = pack
            .images
            .iter()
            .map(|image| image.shortcode.as_str())
            .collect();
        assert_eq!(kept, ["not:grammar but shown", "ok"]);
        assert_eq!(pack.skipped, 14);
        for broken in [json!(null), json!([]), json!({"images": []})] {
            assert!(parse_pack(&broken, Format::Stable).is_none());
        }
        // A cleared or redacted pack is empty, not malformed.
        for cleared in [json!({}), json!({"pack": {}})] {
            let pack = parse_pack(&cleared, Format::Stable).unwrap();
            assert!(pack.images.is_empty());
            assert_eq!(pack.skipped, 0);
        }
    }

    #[test]
    fn thumbnails_and_extension_fields_are_not_forwarded() {
        let content = json!({"images": {"one": {
            "url": "mxc://example.org/one",
            "info": {
                "mimetype": "image/webp", "w": 64, "h": 32, "is_animated": true,
                "thumbnail_url": "mxc://elsewhere.example/thumb",
                "thumbnail_info": {"w": 1, "h": 1},
                "xyz.amorgan.blurhash": "LKO2?U%2Tw=w"
            }
        }}});
        let pack = parse_pack(&content, Format::Stable).unwrap();
        let image = &pack.images[0];
        assert_eq!(image.info.is_animated, Some(true));
        assert!(image.info.thumbnail_source.is_none());
        assert!(image.info.thumbnail_info.is_none());
        let sticker = sticker_content(&CatalogImage {
            pack: 0,
            shortcode: image.shortcode.clone(),
            url: image.url.clone(),
            body: image.body.clone(),
            info: image.info.clone(),
        })
        .unwrap();
        let encoded = serde_json::to_value(&sticker).unwrap();
        assert_eq!(encoded["body"], "one");
        assert_eq!(encoded["url"], "mxc://example.org/one");
        assert_eq!(encoded["info"]["mimetype"], "image/webp");
        assert!(encoded["info"].get("thumbnail_url").is_none());
        assert!(encoded["info"].get("xyz.amorgan.blurhash").is_none());
        assert!(encoded.get("m.relates_to").is_none());
    }

    #[test]
    fn image_and_pack_counts_are_bounded() {
        let mut images = Map::new();
        for index in 0..MAX_IMAGES_PER_PACK + 5 {
            images.insert(
                format!("s{index:04}"),
                json!({"url": format!("mxc://example.org/m{index}")}),
            );
        }
        let content = json!({ "images": images });
        let pack = parse_pack(&content, Format::Stable).unwrap();
        assert_eq!(pack.images.len(), MAX_IMAGES_PER_PACK);
        assert_eq!(pack.skipped, 5);

        let mut builder = Builder::default();
        for index in 0..MAX_PACKS + 2 {
            builder.add(
                room_origin(
                    "!packs:example.org",
                    &format!("k{index}"),
                    &format!("$e{index}"),
                ),
                Format::Stable,
                content.clone(),
                StickerPackSource::Room,
                None,
                "Room stickers".into(),
            );
        }
        assert!(builder.images.len() <= MAX_CATALOG_ITEMS);
        assert!(builder.packs.len() <= MAX_PACKS);
        assert!(builder.skipped > 0);
    }

    #[test]
    fn a_pack_both_enabled_and_in_the_destination_is_listed_once_without_its_list() {
        let mut builder = Builder::default();
        builder.add(
            room_origin("!dest:example.org", "", "$one"),
            Format::Stable,
            spec_example(),
            StickerPackSource::Enabled,
            Some(enabled(json!({"rooms": {"!dest:example.org": {"": {}}}}))),
            "Destination".into(),
        );
        builder.add(
            room_origin("!dest:example.org", "", "$one"),
            Format::Stable,
            spec_example(),
            StickerPackSource::Room,
            None,
            "Destination".into(),
        );
        assert_eq!(builder.packs.len(), 1);
        assert_eq!(builder.packs[0].source, StickerPackSource::Room);
        assert_eq!(builder.snapshots[0].enabled_by, None);
        // The same key in the legacy event type is a different pack.
        builder.add(
            PackOrigin::Room {
                room_id: RoomId::parse("!dest:example.org").unwrap(),
                event_type: LEGACY_ROOM_PACK,
                state_key: String::new(),
                event_id: "$two".into(),
            },
            Format::Legacy,
            spec_example(),
            StickerPackSource::Room,
            None,
            "Destination".into(),
        );
        assert_eq!(builder.packs.len(), 2);
    }

    #[test]
    fn public_entries_are_sanitized_labels_without_media_addresses() {
        let content = json!({"images": {
            "plain": {"url": "mxc://example.org/plain"},
            "described": {"url": "mxc://example.org/described",
                "body": "  A\u{202e}gnirts \u{200b}waving\n\tcat  "}
        }, "pack": {"display_name": "Fictional\u{2066} pack"}});
        let mut builder = Builder::default();
        builder.add(
            room_origin("!dest:example.org", "", "$one"),
            Format::Stable,
            content,
            StickerPackSource::Room,
            None,
            "Room stickers".into(),
        );
        let (_selections, catalog) = catalog(builder, true);
        assert_eq!(catalog.packs()[0].name, "Fictional pack");
        assert_eq!(catalog.entries()[0].label, "Agnirts waving cat");
        assert_eq!(catalog.entries()[0].shortcode, "described");
        assert_eq!(catalog.entries()[1].label, "plain");
        let public = format!("{:?} {:?} {:?}", catalog, catalog.entries(), catalog.packs());
        assert!(!public.contains("mxc://"));
        assert!(catalog.requires_public_media_consent());
    }

    #[test]
    fn display_text_is_bounded_and_single_line() {
        assert_eq!(display_text("a\r\nb\u{0}c", 10), "a bc");
        assert_eq!(display_text("abcdef", 3), "abc…");
        assert_eq!(display_text(" \u{2067}\u{fffc} ", 3), "");
        assert_eq!(display_text("ab cd", 4), "ab c…");
    }

    #[test]
    fn enabled_lists_are_bounded_and_ignore_malformed_entries() {
        let list = json!({"rooms": {
            "!one:example.org": {"": {}, "second": {}, "bad": 1},
            "not a room": {"": {}},
            "!two:example.org": "string"
        }});
        let (references, skipped) = enabled_references(&list);
        let one = RoomId::parse("!one:example.org").unwrap();
        let expected = [(one.clone(), String::new()), (one, "second".into())];
        assert_eq!(references, expected);
        assert_eq!(skipped, 3);
        assert_eq!(enabled_references(&json!({"rooms": []})), (Vec::new(), 1));
        assert_eq!(enabled_references(&json!({})), (Vec::new(), 0));
    }

    #[test]
    fn consent_is_bound_to_one_catalog() {
        let mut builder = Builder::default();
        builder.add(
            room_origin("!dest:example.org", "", "$one"),
            Format::Stable,
            spec_example(),
            StickerPackSource::Room,
            None,
            "Room stickers".into(),
        );
        let (selections, first) = catalog(builder, true);
        let second = Builder::default().finish(
            selections.for_account("robin-local").unwrap(),
            "!dest:example.org".into(),
            None,
            true,
        );
        assert_ne!(first.id(), second.id());
        assert!(first.consent_matches(Some(&first.public_media_consent())));
        assert!(!first.consent_matches(Some(&second.public_media_consent())));
        assert!(!first.consent_matches(None));
    }

    #[test]
    fn catalogs_belong_to_one_selection_epoch_room_and_thread() {
        let selections = Selections::default();
        selections.replace(Some("robin-local"));
        let selection = selections.for_account("robin-local").unwrap();
        let catalog = Builder::default().finish(
            selection.clone(),
            "!dest:example.org".into(),
            Some("$root".into()),
            false,
        );
        assert!(catalog.is_for(&selection, "!dest:example.org", Some("$root")));
        assert!(!catalog.is_for(&selection, "!dest:example.org", None));
        assert!(!catalog.is_for(&selection, "!other:example.org", Some("$root")));
        // Switching away and back yields a new epoch; the old catalog is dead.
        selections.replace(Some("casey-local"));
        selections.replace(Some("robin-local"));
        let again = selections.for_account("robin-local").unwrap();
        assert!(!catalog.is_for(&again, "!dest:example.org", Some("$root")));
        assert!(!catalog.is_for(&selection, "!dest:example.org", Some("$root")));
    }

    #[test]
    fn snapshots_differ_on_content_event_and_enabling_list_changes() {
        let base = PackSnapshot {
            origin: room_origin("!pack:example.org", "", "$one"),
            format: Format::Stable,
            content: spec_example(),
            enabled_by: Some(enabled(json!({"rooms": {"!pack:example.org": {"": {}}}}))),
        };
        let mut changed = base.clone();
        changed.content["images"]["cat_nap"]["url"] = json!("mxc://example.org/swapped");
        assert_ne!(base, changed);
        let mut redacted = base.clone();
        redacted.content = json!({});
        assert_ne!(base, redacted);
        let mut replaced = base.clone();
        replaced.origin = room_origin("!pack:example.org", "", "$two");
        assert_ne!(base, replaced);
        let mut local = base.clone();
        local.enabled_by = None;
        assert_ne!(base, local);
        // Any change to the enabling account data, not only removal of this
        // pack, is a different snapshot.
        let mut relisted = base.clone();
        relisted.enabled_by = Some(enabled(json!({"rooms": {
            "!pack:example.org": {"": {}},
            "!other:example.org": {"": {}}
        }})));
        assert_ne!(base, relisted);
        assert_eq!(base, base.clone());
    }

    #[test]
    fn sticker_body_falls_back_to_the_shortcode_and_events_are_bounded() {
        let image = CatalogImage {
            pack: 0,
            shortcode: "fallback".into(),
            url: OwnedMxcUri::from("mxc://example.org/one"),
            body: None,
            info: ImageInfo::new(),
        };
        let content = sticker_content(&image).unwrap();
        assert_eq!(content.body, "fallback");
        let mut huge = image.clone();
        huge.body = Some("z".repeat(MAX_STICKER_EVENT_BYTES));
        assert!(matches!(
            sticker_content(&huge),
            Err(MatrixCoreError::StickerUnavailable)
        ));
    }

    fn pack_event(state_key: &str, content: Value) -> String {
        json!({
            "type": ROOM_PACK,
            "state_key": state_key,
            "event_id": format!("${state_key}"),
            "sender": "@robin:example.org",
            "origin_server_ts": 1,
            "content": content
        })
        .to_string()
    }

    fn sync_event(json: &str) -> RawAnySyncOrStrippedState {
        RawAnySyncOrStrippedState::Sync(Raw::from_json_string(json.to_owned()).unwrap())
    }

    #[test]
    fn room_packs_are_chosen_from_headers_before_any_content_is_read() {
        // Forty packs with every fourth one cleared: the first sixteen
        // non-empty packs by state key are chosen, the rest are refused.
        let events: Vec<String> = (0..40)
            .map(|index| {
                let content = if index % 4 == 0 {
                    json!({})
                } else {
                    spec_example()
                };
                pack_event(&format!("k{index:02}"), content)
            })
            .collect();
        let raw: Vec<_> = events.iter().map(|json| Some(json.as_str())).collect();
        let (chosen, skipped) = choose_room_packs(&raw);
        assert_eq!(chosen.len(), MAX_PACKS_PER_ROOM);
        assert_eq!(skipped, 30 - MAX_PACKS_PER_ROOM);
        assert!(chosen.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(chosen.iter().all(|index| index % 4 != 0));

        // Only the chosen events are read in full and retained.
        let full: Vec<_> = events.iter().map(|json| sync_event(json)).collect();
        let (packs, refused) = bounded_room_packs(&full);
        assert_eq!(packs.len(), MAX_PACKS_PER_ROOM);
        assert_eq!(refused, skipped);
        assert_eq!(packs[0].0, "k01");

        // Beyond the raw bound nothing is scanned, parsed or retained.
        let many: Vec<_> = (0..=MAX_RAW_PACK_EVENTS).map(|_| raw[1]).collect();
        let refused = choose_room_packs(&many);
        assert_eq!(refused, (Vec::new(), MAX_RAW_PACK_EVENTS + 1));
        let many_full: Vec<_> = (0..=MAX_RAW_PACK_EVENTS)
            .map(|_| sync_event(&events[1]))
            .collect();
        let (packs, refused) = bounded_room_packs(&many_full);
        assert!(packs.is_empty());
        assert_eq!(refused, MAX_RAW_PACK_EVENTS + 1);

        // Oversized, stripped and malformed headers are refused unread.
        let body = "z".repeat(MAX_PACK_EVENT_BYTES);
        let content = json!({"images": {"x": {"url": "mxc://example.org/x", "body": body}}});
        let huge = pack_event("huge", content);
        let odd = r#"{"state_key": 1, "content": {"images": {}}}"#;
        let (chosen, skipped) = choose_room_packs(&[Some(huge.as_str()), None, Some(odd)]);
        assert!(chosen.is_empty());
        assert_eq!(skipped, 3);
    }

    /// A store whose reads can be held, so membership, the selection or the
    /// data can change while a revalidation is suspended inside a read.
    #[derive(Default)]
    struct PausedReader {
        joined: Mutex<bool>,
        state: Mutex<Option<StateContent>>,
        list: Mutex<Option<Value>>,
        hold_state: Mutex<Option<oneshot::Receiver<()>>>,
        hold_list: Mutex<Option<oneshot::Receiver<()>>>,
    }

    impl PackReader for PausedReader {
        fn joined(&self, _: &RoomId) -> bool {
            *self.joined.lock().unwrap()
        }

        async fn state(
            &self,
            _room_id: &RoomId,
            _event_type: &'static str,
            _state_key: &str,
        ) -> Result<Option<StateContent>> {
            let hold = self.hold_state.lock().unwrap().take();
            if let Some(hold) = hold {
                let _ = hold.await;
            }
            Ok(self.state.lock().unwrap().clone())
        }

        async fn account(&self, _: &'static str, _: usize) -> Result<AccountContent> {
            let hold = self.hold_list.lock().unwrap().take();
            if let Some(hold) = hold {
                let _ = hold.await;
            }
            let list = self.list.lock().unwrap().clone();
            Ok(list.map_or(AccountContent::Absent, AccountContent::Present))
        }
    }

    fn enabled_list() -> Value {
        json!({"rooms": {"!pack:example.org": {"": {}}}})
    }

    fn enabled_snapshot() -> PackSnapshot {
        PackSnapshot {
            origin: room_origin("!pack:example.org", "", "$one"),
            format: Format::Stable,
            content: spec_example(),
            enabled_by: Some(enabled(enabled_list())),
        }
    }

    fn paused_reader() -> PausedReader {
        let reader = PausedReader::default();
        *reader.joined.lock().unwrap() = true;
        *reader.state.lock().unwrap() = Some((String::new(), "$one".into(), spec_example()));
        *reader.list.lock().unwrap() = Some(enabled_list());
        reader
    }

    fn revoked(result: Result<()>) -> bool {
        matches!(result, Err(MatrixCoreError::StickerUnavailable))
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn revalidation_requires_the_exact_pack_and_enabling_data() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("robin-local"));
            let selection = selections.for_account("robin-local").unwrap();
            let snapshot = enabled_snapshot();
            let reader = paused_reader();
            let unchanged = snapshot_current(&reader, &selection, &snapshot).await;
            assert!(unchanged.is_ok());
            // Still listed, but the enabling data changed: revoked.
            *reader.list.lock().unwrap() = Some(json!({"rooms": {
                "!pack:example.org": {"": {}},
                "!other:example.org": {"": {}}
            }}));
            let relisted = snapshot_current(&reader, &selection, &snapshot).await;
            assert!(revoked(relisted));
            *reader.list.lock().unwrap() = Some(enabled_list());
            *reader.state.lock().unwrap() = Some((String::new(), "$two".into(), spec_example()));
            let replaced = snapshot_current(&reader, &selection, &snapshot).await;
            assert!(revoked(replaced));
            *reader.state.lock().unwrap() = Some((String::new(), "$one".into(), json!({})));
            let cleared = snapshot_current(&reader, &selection, &snapshot).await;
            assert!(revoked(cleared));
        });
    }

    #[test]
    fn changes_during_a_paused_read_revoke_the_choice() {
        runtime().block_on(async {
            let selections = Selections::default();
            selections.replace(Some("robin-local"));
            let selection = selections.for_account("robin-local").unwrap();
            let snapshot = enabled_snapshot();
            for pause_list in [false, true] {
                // The pack room is left while the store read is suspended.
                let reader = paused_reader();
                let (release, hold) = oneshot::channel();
                let slot = if pause_list {
                    &reader.hold_list
                } else {
                    &reader.hold_state
                };
                *slot.lock().unwrap() = Some(hold);
                let mut check = Box::pin(snapshot_current(&reader, &selection, &snapshot));
                assert!(futures_util::poll!(check.as_mut()).is_pending());
                *reader.joined.lock().unwrap() = false;
                release.send(()).unwrap();
                assert!(revoked(check.await));
            }

            // The enabling data changes while the pack read is suspended.
            let reader = paused_reader();
            let (release, hold) = oneshot::channel();
            *reader.hold_state.lock().unwrap() = Some(hold);
            let mut check = Box::pin(snapshot_current(&reader, &selection, &snapshot));
            assert!(futures_util::poll!(check.as_mut()).is_pending());
            *reader.list.lock().unwrap() = Some(json!({"rooms": {}}));
            release.send(()).unwrap();
            assert!(revoked(check.await));

            // The selection ends while the list read is suspended.
            let reader = paused_reader();
            let (release, hold) = oneshot::channel();
            *reader.hold_list.lock().unwrap() = Some(hold);
            let mut check = Box::pin(snapshot_current(&reader, &selection, &snapshot));
            assert!(futures_util::poll!(check.as_mut()).is_pending());
            selections.replace(Some("casey-local"));
            release.send(()).unwrap();
            assert!(matches!(
                check.await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
        });
    }
}

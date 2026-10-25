//! Native multi-source call stage for the active MatrixRTC call.
//!
//! The RTC task plans one featured tile (a pinned tile, otherwise the first
//! live remote screen share) plus one page of participant tiles. Only remote
//! video on that plan is subscribed with a size cap and opened as a frame
//! stream; every other remote video stream is paused at the SFU. Each open
//! stream keeps only its newest decoded frame. One blocking worker at a time
//! converts a bounded batch of them to scaled RGB, at most fifteen times per
//! second. Slint receives owned pixel buffers in fixed frame slots beside a
//! revisioned layout, inside one latest-value watch channel, so a picture is
//! never shown against another tile's layout.

use std::{
    collections::{HashMap, HashSet},
    future::poll_fn,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use futures_util::{StreamExt, stream::BoxStream};
use morpheus_rtc::{
    Dimensions, MediaConstraints, MediaStreamKind, ParticipantInfo, RemoteTrackHandle,
    RtcCallEvent, RtcService, RtcStreamKind, VideoDetail, VideoFrame, VideoRotation,
};
use slint::{Rgb8Pixel, SharedPixelBuffer};
use tokio::{
    sync::watch,
    task::JoinHandle,
    time::{Instant, MissedTickBehavior},
};

/// Frame slots shared with the UI. Every planned layout fits in them.
pub const FRAME_SLOTS: usize = 9;
/// Participant tiles on one page when nothing is featured.
const GRID_PAGE: usize = 9;
/// Participant tiles beside a featured tile.
const FILMSTRIP_PAGE: usize = 4;
const FEATURED_SIZE: (u32, u32) = (1920, 1080);
const FILMSTRIP_SIZE: (u32, u32) = (320, 180);
/// Largest decoded frame accepted for scaling. Conversion work follows the
/// scaled output size, not the source size.
const SOURCE_MAX_EDGE: u32 = 4096;
const SOURCE_MAX_PIXELS: usize = 4096 * 2304;
const FRAME_INTERVAL: Duration = Duration::from_millis(67);
const THUMBNAIL_INTERVAL: Duration = Duration::from_millis(133);
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// How long a pinned tile reports that its participant left or its share ended.
const DEPARTED_PLACEHOLDER: Duration = Duration::from_secs(8);
/// Frames taken from one stream in a single wakeup before yielding.
const MAX_DRAIN: usize = 8;

const _: () = assert!(GRID_PAGE <= FRAME_SLOTS && FILMSTRIP_PAGE < FRAME_SLOTS);

static NEXT_PRESENTATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TileMedium {
    Camera,
    Screen,
}

impl TileMedium {
    fn stream(self) -> MediaStreamKind {
        match self {
            Self::Camera => MediaStreamKind::Camera,
            Self::Screen => MediaStreamKind::ScreenShare,
        }
    }

    fn roster_stream(self) -> RtcStreamKind {
        match self {
            Self::Camera => RtcStreamKind::Camera,
            Self::Screen => RtcStreamKind::ScreenShare,
        }
    }

    fn from_stream(kind: MediaStreamKind) -> Option<Self> {
        match kind {
            MediaStreamKind::Camera => Some(Self::Camera),
            MediaStreamKind::ScreenShare => Some(Self::Screen),
            _ => None,
        }
    }
}

/// One video medium of one membership. `member_id` is unique for each join,
/// so a participant who leaves and rejoins gets new tiles.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TileKey {
    pub member_id: String,
    pub medium: TileMedium,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileState {
    /// Subscribed and presented; `frame_slot` holds its newest picture.
    Live,
    /// Published and unmuted, but no subscribed track is available yet.
    Connecting,
    /// The publisher paused this stream.
    Muted,
    /// No camera is published.
    Off,
    /// No transport this client supports reaches this participant.
    Unavailable,
    /// Their frames are not decrypting with the media keys this client holds.
    Undecryptable,
    /// A pinned participant left the call.
    Left,
    /// A pinned screen share stopped.
    Ended,
    /// This device's own membership, not publishing this medium.
    Local,
    /// This device publishes this medium. Local capture is not shown here.
    LocalPublishing,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TileView {
    /// Stable while this tile stays in the call.
    pub id: u64,
    pub key: TileKey,
    pub user_id: String,
    pub state: TileState,
    pub is_local: bool,
    pub pinned: bool,
    pub microphone_muted: bool,
    pub speaking: bool,
    pub hand_raised: bool,
    pub frame_slot: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StageLayout {
    pub featured: Option<TileView>,
    /// The current page of screen-share and participant tiles.
    pub tiles: Vec<TileView>,
    pub page: usize,
    pub pages: usize,
    /// Tiles across every page, excluding the featured tile.
    pub tile_count: usize,
    /// A media transport is reconnecting; pictures may freeze meanwhile.
    pub media_degraded: bool,
}

#[derive(Clone, Default)]
pub struct FrameSlot {
    /// Changes whenever this slot is released or given to another stream.
    pub epoch: u64,
    /// Changes with every presented picture.
    pub sequence: u64,
    pub pixels: Option<SharedPixelBuffer<Rgb8Pixel>>,
}

#[derive(Clone)]
pub struct StageSnapshot {
    /// Unique to one video task. A rejoin keeps the call token but starts a
    /// new task whose revisions and slot epochs begin again.
    pub presentation: u64,
    /// Changes only when `layout` changes; new pictures keep it.
    pub revision: u64,
    pub layout: Arc<StageLayout>,
    pub frames: [FrameSlot; FRAME_SLOTS],
}

#[derive(Clone)]
pub enum VideoUpdate {
    Clear {
        token: u64,
    },
    Stage {
        token: u64,
        stage: Arc<StageSnapshot>,
    },
}

impl VideoUpdate {
    pub fn token(&self) -> u64 {
        match self {
            Self::Clear { token } | Self::Stage { token, .. } => *token,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RosterUpdate {
    pub token: u64,
    pub members: Vec<ParticipantInfo>,
}

/// What the UI asks of the stage. `pinned` and `page` belong to the call
/// generation in `token`; `presented` says whether the stage is on screen.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StageRequest {
    pub token: u64,
    pub pinned: Option<TileKey>,
    pub page: usize,
    pub presented: bool,
}

impl StageRequest {
    /// Apply a pin or page choice for `token`, discarding another call's.
    pub fn choose(&mut self, token: u64, change: impl FnOnce(&mut Self)) {
        if self.token != token {
            self.token = token;
            self.pinned = None;
            self.page = 0;
        }
        change(self);
    }

    /// Returns whether visibility changed.
    pub fn present(&mut self, presented: bool) -> bool {
        let changed = self.presented != presented;
        self.presented = presented;
        changed
    }
}

/// Run beside the audio future. Dropping this future drops every frame
/// stream, pending frame and conversion result. The controller checks the
/// call token again before any queued Slint update is applied.
pub async fn run_remote_video(
    rtc: &RtcService,
    room_id: &str,
    token: u64,
    updates: watch::Sender<VideoUpdate>,
    roster_updates: watch::Sender<RosterUpdate>,
    mut requests: watch::Receiver<StageRequest>,
    active_token: &AtomicU64,
) {
    let Some(call) = rtc.call(room_id) else {
        return;
    };
    let mut roster = call.subscribe_participants();
    let mut events = call.subscribe_call_events();
    let mut refresh = tokio::time::interval(REFRESH_INTERVAL);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut presentation = tokio::time::interval(FRAME_INTERVAL);
    presentation.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut stage = Stage::new(CallMedia { rtc, room_id }, token);
    stage.request(&requests.borrow_and_update());
    stage.reload(&roster_updates, active_token);
    stage.replan();
    stage.publish(&updates, active_token);
    let mut requests_open = true;
    let mut conversion: Option<ConversionBatch> = None;

    loop {
        tokio::select! {
            // Apply queued stop/mute/leave events before presenting a
            // conversion or frame that became ready at the same time.
            biased;
            event = events.recv() => {
                use tokio::sync::broadcast::error::RecvError;
                let reload = match event {
                    Ok(RtcCallEvent::Ended { .. }) | Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => {
                        // Stop events may be lost; never keep a stream that
                        // might have been unpublished meanwhile.
                        stage.presenter.release_all();
                        true
                    }
                    Ok(ref event) => stage.event(event),
                };
                if reload {
                    stage.reload(&roster_updates, active_token);
                }
                stage.replan();
            }
            changed = requests.changed(), if requests_open => {
                requests_open = changed.is_ok();
                stage.request(&requests.borrow_and_update());
                stage.replan();
            }
            changed = roster.changed() => {
                if changed.is_err() {
                    break;
                }
                stage.reload(&roster_updates, active_token);
                stage.replan();
            }
            _ = refresh.tick() => {
                stage.presenter.cooling.clear();
                stage.reload(&roster_updates, active_token);
                stage.replan();
            }
            converted = async { conversion.as_mut().expect("guarded conversion").await }, if conversion.is_some() => {
                conversion = None;
                if let Ok(converted) = converted {
                    stage.presenter.accept(converted);
                }
            }
            // Before draining frames, so constantly arriving frames can
            // never starve presentation.
            now = presentation.tick(), if conversion.is_none() => {
                let batch = stage.presenter.take_due_frames(now);
                if !batch.is_empty() {
                    // RGB conversion can be expensive at 1080p. Never block
                    // receiving stop/mute/leave events while it runs. A
                    // released slot changes its epoch, so a late result for
                    // it is discarded even within the same call token.
                    conversion = Some(tokio::task::spawn_blocking(move || convert_batch(batch)));
                }
            }
            () = poll_fn(|cx| stage.presenter.poll_frames(cx)), if stage.presenter.decoding() => {
                if std::mem::take(&mut stage.presenter.ended) {
                    stage.replan();
                }
            }
        }
        stage.publish(&updates, active_token);
    }
    if let Some(conversion) = conversion.take() {
        // Cancel queued work; an already running converter may finish but no
        // longer has a receiver that can publish its result.
        conversion.abort();
    }
    if active_token.load(Ordering::SeqCst) == token {
        updates.send_replace(VideoUpdate::Clear { token });
    }
}

fn publish_roster(
    updates: &watch::Sender<RosterUpdate>,
    token: u64,
    members: Vec<ParticipantInfo>,
    active_token: &AtomicU64,
) {
    if active_token.load(Ordering::SeqCst) != token {
        return;
    }
    updates.send_if_modified(|current| {
        if current.token == token && current.members == members {
            false
        } else {
            *current = RosterUpdate { token, members };
            true
        }
    });
}

/// The call operations the stage needs, kept narrow so the stage's
/// bookkeeping does not depend on a live call.
trait StageMedia {
    fn participants(&self) -> Vec<ParticipantInfo>;
    fn track(&self, key: &TileKey) -> Option<Arc<dyn RemoteTrackHandle>>;
    fn constrain(&self, key: &TileKey, constraints: MediaConstraints);
}

#[derive(Clone, Copy)]
struct CallMedia<'a> {
    rtc: &'a RtcService,
    room_id: &'a str,
}

impl StageMedia for CallMedia<'_> {
    fn participants(&self) -> Vec<ParticipantInfo> {
        self.rtc.participants(self.room_id).unwrap_or_default()
    }

    fn track(&self, key: &TileKey) -> Option<Arc<dyn RemoteTrackHandle>> {
        self.rtc
            .call(self.room_id)?
            .remote_track(&key.member_id, key.medium.stream())
    }

    fn constrain(&self, key: &TileKey, constraints: MediaConstraints) {
        if let Some(call) = self.rtc.call(self.room_id) {
            call.set_constraints(&key.member_id, key.medium.stream(), constraints);
        }
    }
}

/// Paused at the SFU: still subscribed, so it resumes instantly, but no
/// media is sent or decoded.
fn paused() -> MediaConstraints {
    MediaConstraints {
        visible: false,
        ..MediaConstraints::default()
    }
}

/// Presented, with the SFU asked for the smallest layer covering `size`.
fn sized((width, height): (u32, u32)) -> MediaConstraints {
    MediaConstraints {
        detail: VideoDetail::Dimensions(Dimensions { width, height }),
        ..MediaConstraints::default()
    }
}

/// Output size and cadence for each tile of a grid page with `tiles` tiles.
fn grid_quality(tiles: usize) -> ((u32, u32), Duration) {
    match tiles {
        0 | 1 => ((1280, 720), FRAME_INTERVAL),
        2 => ((960, 540), FRAME_INTERVAL),
        3 | 4 => ((640, 360), FRAME_INTERVAL),
        _ => ((480, 270), THUMBNAIL_INTERVAL),
    }
}

fn video_key(member_id: &str, kind: MediaStreamKind) -> Option<TileKey> {
    TileMedium::from_stream(kind).map(|medium| TileKey {
        member_id: member_id.to_owned(),
        medium,
    })
}

struct Stage<M> {
    media: M,
    token: u64,
    presentation: u64,
    participants: Vec<ParticipantInfo>,
    pinned: Option<TileKey>,
    page: usize,
    presented: bool,
    speaking: HashSet<String>,
    undecryptable: HashSet<String>,
    media_degraded: bool,
    planner: Planner,
    presenter: Presenter,
    /// Last constraints sent for each remote video stream on the roster.
    constraints: HashMap<TileKey, MediaConstraints>,
    layout: Arc<StageLayout>,
    revision: u64,
}

impl<M: StageMedia> Stage<M> {
    fn new(media: M, token: u64) -> Self {
        Self {
            media,
            token,
            presentation: NEXT_PRESENTATION.fetch_add(1, Ordering::Relaxed),
            participants: Vec::new(),
            pinned: None,
            page: 0,
            presented: false,
            speaking: HashSet::new(),
            undecryptable: HashSet::new(),
            media_degraded: false,
            planner: Planner::default(),
            presenter: Presenter::default(),
            constraints: HashMap::new(),
            layout: Arc::new(StageLayout::default()),
            revision: 0,
        }
    }

    fn request(&mut self, request: &StageRequest) {
        self.presented = request.presented;
        if request.token == self.token {
            self.pinned = request.pinned.clone();
            self.page = request.page;
        } else {
            self.pinned = None;
            self.page = 0;
        }
    }

    fn reload(&mut self, roster_updates: &watch::Sender<RosterUpdate>, active_token: &AtomicU64) {
        self.participants = self.media.participants();
        publish_roster(
            roster_updates,
            self.token,
            self.participants.clone(),
            active_token,
        );
    }

    /// Release presented streams an event invalidated. Returns whether the
    /// roster must be read again.
    fn event(&mut self, event: &RtcCallEvent) -> bool {
        match event {
            RtcCallEvent::StreamStarted { member_id, kind } => {
                if let Some(key) = video_key(member_id, *kind) {
                    self.presenter.cooling.remove(&key);
                    // A republished track has the same membership and kind
                    // but a different decoder stream. The roster's latest
                    // snapshot can coalesce Stop/Start into an unchanged row,
                    // so compare the track itself and release a replaced one
                    // together with its frozen picture.
                    if let Some(slot) = self.presenter.slot_of(&key) {
                        let current = self.media.track(&key);
                        if !self.presenter.is_track(slot, current.as_ref()) {
                            self.presenter.release(slot);
                        }
                    }
                }
                true
            }
            RtcCallEvent::StreamStopped { member_id, kind }
            | RtcCallEvent::StreamMuted { member_id, kind } => {
                if let Some(key) = video_key(member_id, *kind)
                    && let Some(slot) = self.presenter.slot_of(&key)
                {
                    self.presenter.release(slot);
                }
                true
            }
            RtcCallEvent::StreamUnmuted { .. }
            | RtcCallEvent::ParticipantJoined { .. }
            | RtcCallEvent::HandRaised { .. }
            | RtcCallEvent::HandLowered { .. } => true,
            RtcCallEvent::ParticipantLeft { member_id } => {
                self.presenter.release_member(member_id);
                self.speaking.remove(member_id);
                self.undecryptable.remove(member_id);
                true
            }
            RtcCallEvent::ActiveSpeakers { speakers } => {
                self.speaking.clear();
                self.speaking
                    .extend(speakers.iter().map(|speaker| speaker.member_id.clone()));
                false
            }
            RtcCallEvent::FrameEncryptionState {
                member_id, state, ..
            } => {
                if !state.is_failure() {
                    self.undecryptable.remove(member_id);
                } else if self.undecryptable.insert(member_id.clone()) {
                    self.presenter.release_member(member_id);
                }
                false
            }
            // A new key may decrypt their media again; a remaining failure
            // reports itself with another encryption-state event.
            RtcCallEvent::KeyImported { member_id, .. } => {
                self.undecryptable.remove(member_id);
                false
            }
            RtcCallEvent::MediaConnectionState { degraded } => {
                self.media_degraded = *degraded;
                false
            }
            _ => false,
        }
    }

    /// Recompute the layout from the cached roster and requests, open and
    /// close streams to match it, and update SFU constraints.
    fn replan(&mut self) {
        let input = PlanInput {
            participants: &self.participants,
            pinned: self.pinned.as_ref(),
            page: self.page,
            presented: self.presented,
            speaking: &self.speaking,
            undecryptable: &self.undecryptable,
            now: Instant::now(),
        };
        let Plan { mut layout, decode } = self
            .planner
            .plan(&input, |key| self.media.track(key).is_some());
        self.open_planned(&decode);
        self.constrain(&decode);
        for tile in layout.featured.iter_mut().chain(layout.tiles.iter_mut()) {
            tile.frame_slot = self.presenter.slot_of(&tile.key);
        }
        layout.media_degraded = self.media_degraded;
        if *self.layout != layout {
            self.layout = Arc::new(layout);
            self.revision += 1;
            self.presenter.dirty = true;
        }
    }

    /// Keep exactly the planned streams open, each in a stable slot.
    fn open_planned(&mut self, decode: &[DecodeRequest]) {
        for slot in 0..FRAME_SLOTS {
            let keep = match self.presenter.decoders[slot].as_ref() {
                Some(decoder) if decode.iter().any(|request| request.key == decoder.key) => {
                    // The SDK can refresh a same-key track handle without a
                    // StreamStarted event. Never retain its old frame stream
                    // or picture just because the membership and kind match.
                    let current = self.media.track(&decoder.key);
                    let same = self.presenter.is_track(slot, current.as_ref());
                    if !same {
                        // A replacement starts with its own subscription state.
                        // Reapply the current demand even if the size is unchanged.
                        self.constraints.remove(&decoder.key);
                    }
                    same
                }
                _ => false,
            };
            if !keep {
                self.presenter.release(slot);
            }
        }
        for request in decode {
            if let Some(decoder) = self
                .presenter
                .decoders
                .iter_mut()
                .flatten()
                .find(|decoder| decoder.key == request.key)
            {
                decoder.size = request.size;
                decoder.interval = request.interval;
                continue;
            }
            if self.presenter.cooling.contains(&request.key) {
                continue;
            }
            let Some(slot) = self.presenter.free_slot() else {
                break;
            };
            let Some(track) = self.media.track(&request.key) else {
                continue;
            };
            let Some(frames) = track.video_frames() else {
                continue;
            };
            self.presenter.open(slot, request, track, frames);
        }
    }

    /// Pause every remote video stream the plan does not present, so the
    /// SFU stops sending it, and cap each presented stream at its tile size.
    fn constrain(&mut self, decode: &[DecodeRequest]) {
        for participant in self
            .participants
            .iter()
            .filter(|participant| !participant.is_local)
        {
            for medium in [TileMedium::Camera, TileMedium::Screen] {
                if !participant
                    .streams
                    .iter()
                    .any(|stream| stream.kind == medium.roster_stream())
                {
                    continue;
                }
                let key = TileKey {
                    member_id: participant.member_id.clone(),
                    medium,
                };
                let wanted = decode
                    .iter()
                    .find(|request| request.key == key)
                    .map_or_else(paused, |request| sized(request.size));
                if self.constraints.get(&key) != Some(&wanted) {
                    self.media.constrain(&key, wanted);
                    self.constraints.insert(key, wanted);
                }
            }
        }
        // The SDK forgets a departed membership's constraints with it.
        let participants = &self.participants;
        self.constraints.retain(|key, _| {
            participants
                .iter()
                .any(|participant| participant.member_id == key.member_id)
        });
    }

    fn publish(&mut self, updates: &watch::Sender<VideoUpdate>, active_token: &AtomicU64) {
        if !std::mem::take(&mut self.presenter.dirty)
            || active_token.load(Ordering::SeqCst) != self.token
        {
            return;
        }
        updates.send_replace(VideoUpdate::Stage {
            token: self.token,
            stage: Arc::new(StageSnapshot {
                presentation: self.presentation,
                revision: self.revision,
                layout: Arc::clone(&self.layout),
                frames: self.presenter.frames.clone(),
            }),
        });
    }
}

struct PlanInput<'a> {
    participants: &'a [ParticipantInfo],
    pinned: Option<&'a TileKey>,
    page: usize,
    presented: bool,
    speaking: &'a HashSet<String>,
    undecryptable: &'a HashSet<String>,
    now: Instant,
}

struct DecodeRequest {
    key: TileKey,
    size: (u32, u32),
    interval: Duration,
}

struct Plan {
    layout: StageLayout,
    decode: Vec<DecodeRequest>,
}

struct TileIdentity {
    id: u64,
    user_id: String,
    seen: u64,
}

struct Departed {
    tile: TileView,
    until: Instant,
}

#[derive(Default)]
struct Planner {
    identities: HashMap<TileKey, TileIdentity>,
    next_id: u64,
    generation: u64,
    departed: Option<Departed>,
}

impl Planner {
    fn plan(&mut self, input: &PlanInput<'_>, has_track: impl Fn(&TileKey) -> bool) -> Plan {
        self.generation += 1;
        let mut tiles = Vec::with_capacity(input.participants.len() + 1);
        for participant in input.participants {
            if participant
                .streams
                .iter()
                .any(|stream| stream.kind == RtcStreamKind::ScreenShare)
            {
                tiles.push(self.tile(participant, TileMedium::Screen, input, &has_track));
            }
            tiles.push(self.tile(participant, TileMedium::Camera, input, &has_track));
        }
        // Screen shares first, then remote people, then this device, each in
        // the order they first appeared.
        tiles.sort_by_key(|tile| {
            let person = tile.key.medium == TileMedium::Camera;
            (person, tile.is_local, tile.id)
        });
        let featured = self.featured(&mut tiles, input);

        let capacity = if featured.is_some() {
            FILMSTRIP_PAGE
        } else {
            GRID_PAGE
        };
        let tile_count = tiles.len();
        let pages = tile_count.div_ceil(capacity).max(1);
        let page = input.page.min(pages - 1);
        let tiles: Vec<_> = tiles
            .into_iter()
            .skip(page * capacity)
            .take(capacity)
            .collect();

        let mut decode = Vec::new();
        if input.presented {
            if let Some(tile) = featured
                .as_ref()
                .filter(|tile| tile.state == TileState::Live)
            {
                decode.push(DecodeRequest {
                    key: tile.key.clone(),
                    size: FEATURED_SIZE,
                    interval: FRAME_INTERVAL,
                });
            }
            let (size, interval) = if featured.is_some() {
                (FILMSTRIP_SIZE, THUMBNAIL_INTERVAL)
            } else {
                grid_quality(tiles.len())
            };
            decode.extend(
                tiles
                    .iter()
                    .filter(|tile| tile.state == TileState::Live)
                    .map(|tile| DecodeRequest {
                        key: tile.key.clone(),
                        size,
                        interval,
                    }),
            );
        }

        let generation = self.generation;
        let departed = self.departed.as_ref().map(|departed| &departed.tile.key);
        self.identities
            .retain(|key, identity| identity.seen == generation || Some(key) == departed);
        Plan {
            layout: StageLayout {
                featured,
                tiles,
                page,
                pages,
                tile_count,
                media_degraded: false,
            },
            decode,
        }
    }

    fn tile(
        &mut self,
        participant: &ParticipantInfo,
        medium: TileMedium,
        input: &PlanInput<'_>,
        has_track: &impl Fn(&TileKey) -> bool,
    ) -> TileView {
        let key = TileKey {
            member_id: participant.member_id.clone(),
            medium,
        };
        let id = match self.identities.get_mut(&key) {
            Some(identity) => {
                identity.seen = self.generation;
                identity.id
            }
            None => {
                self.next_id += 1;
                self.identities.insert(
                    key.clone(),
                    TileIdentity {
                        id: self.next_id,
                        user_id: participant.user_id.clone(),
                        seen: self.generation,
                    },
                );
                self.next_id
            }
        };
        let stream = participant
            .streams
            .iter()
            .find(|stream| stream.kind == medium.roster_stream());
        let state = if participant.is_local {
            if stream.is_some_and(|stream| !stream.muted) {
                TileState::LocalPublishing
            } else {
                TileState::Local
            }
        } else if !participant.reachable {
            TileState::Unavailable
        } else {
            match stream {
                None => TileState::Off,
                Some(stream) if stream.muted => TileState::Muted,
                Some(_) if input.undecryptable.contains(&participant.member_id) => {
                    TileState::Undecryptable
                }
                Some(_) if has_track(&key) => TileState::Live,
                Some(_) => TileState::Connecting,
            }
        };
        let person = medium == TileMedium::Camera;
        TileView {
            id,
            pinned: input.pinned == Some(&key),
            user_id: participant.user_id.clone(),
            state,
            is_local: participant.is_local,
            microphone_muted: person
                && participant
                    .streams
                    .iter()
                    .any(|stream| stream.kind == RtcStreamKind::Microphone && stream.muted),
            speaking: person && input.speaking.contains(&participant.member_id),
            hand_raised: person && participant.hand_raised_at_ms.is_some(),
            frame_slot: None,
            key,
        }
    }

    /// The pinned tile, a placeholder saying it left, or the first live
    /// remote screen share. A featured tile is removed from `tiles`.
    fn featured(&mut self, tiles: &mut Vec<TileView>, input: &PlanInput<'_>) -> Option<TileView> {
        let stale = self.departed.as_ref().is_some_and(|departed| {
            input.pinned != Some(&departed.tile.key)
                || departed.until <= input.now
                || tiles.iter().any(|tile| tile.key == departed.tile.key)
        });
        if stale {
            self.departed = None;
        }
        if let Some(pinned) = input.pinned {
            if let Some(index) = tiles.iter().position(|tile| &tile.key == pinned) {
                return Some(tiles.remove(index));
            }
            // Report only a tile that was in the previous plan. An expired
            // placeholder's identity is older and is never reported again.
            if self.departed.is_none()
                && let Some(identity) = self.identities.get(pinned)
                && identity.seen + 1 == self.generation
            {
                let left = !input
                    .participants
                    .iter()
                    .any(|participant| participant.member_id == pinned.member_id);
                self.departed = Some(Departed {
                    tile: TileView {
                        id: identity.id,
                        key: pinned.clone(),
                        user_id: identity.user_id.clone(),
                        state: if left {
                            TileState::Left
                        } else {
                            TileState::Ended
                        },
                        is_local: false,
                        pinned: true,
                        microphone_muted: false,
                        speaking: false,
                        hand_raised: false,
                        frame_slot: None,
                    },
                    until: input.now + DEPARTED_PLACEHOLDER,
                });
            }
            if let Some(departed) = &self.departed {
                return Some(departed.tile.clone());
            }
        }
        tiles
            .iter()
            .position(|tile| {
                tile.key.medium == TileMedium::Screen
                    && matches!(tile.state, TileState::Live | TileState::Connecting)
            })
            .map(|index| tiles.remove(index))
    }
}

struct Decoder {
    key: TileKey,
    track: Arc<dyn RemoteTrackHandle>,
    frames: BoxStream<'static, VideoFrame>,
    /// Overwritten by each decoded frame; decoded frames are never queued.
    newest: Option<VideoFrame>,
    size: (u32, u32),
    interval: Duration,
    due: Instant,
}

struct Conversion {
    slot: usize,
    epoch: u64,
    frame: VideoFrame,
    size: (u32, u32),
}

struct Converted {
    slot: usize,
    epoch: u64,
    pixels: Result<SharedPixelBuffer<Rgb8Pixel>, FrameError>,
}

type ConversionBatch = JoinHandle<Vec<Converted>>;

/// Open frame streams and the pictures last converted from them.
#[derive(Default)]
struct Presenter {
    decoders: [Option<Decoder>; FRAME_SLOTS],
    frames: [FrameSlot; FRAME_SLOTS],
    next_epoch: u64,
    /// Streams whose frame stream ended; retried after a refresh or restart.
    cooling: HashSet<TileKey>,
    /// A snapshot must be published.
    dirty: bool,
    /// A frame stream ended since the last plan.
    ended: bool,
}

impl Presenter {
    fn decoding(&self) -> bool {
        self.decoders.iter().any(Option::is_some)
    }

    fn slot_of(&self, key: &TileKey) -> Option<usize> {
        self.decoders
            .iter()
            .position(|decoder| decoder.as_ref().is_some_and(|decoder| &decoder.key == key))
    }

    fn free_slot(&self) -> Option<usize> {
        self.decoders.iter().position(Option::is_none)
    }

    fn is_track(&self, slot: usize, track: Option<&Arc<dyn RemoteTrackHandle>>) -> bool {
        match (&self.decoders[slot], track) {
            (Some(decoder), Some(track)) => Arc::ptr_eq(&decoder.track, track),
            _ => false,
        }
    }

    fn open(
        &mut self,
        slot: usize,
        request: &DecodeRequest,
        track: Arc<dyn RemoteTrackHandle>,
        frames: BoxStream<'static, VideoFrame>,
    ) {
        self.next_epoch += 1;
        self.frames[slot] = FrameSlot {
            epoch: self.next_epoch,
            sequence: 0,
            pixels: None,
        };
        self.decoders[slot] = Some(Decoder {
            key: request.key.clone(),
            track,
            frames,
            newest: None,
            size: request.size,
            interval: request.interval,
            due: Instant::now(),
        });
        self.dirty = true;
    }

    /// Close a slot's stream and drop its picture immediately.
    fn release(&mut self, slot: usize) {
        if self.decoders[slot].take().is_some() || self.frames[slot].pixels.is_some() {
            self.next_epoch += 1;
            self.frames[slot] = FrameSlot {
                epoch: self.next_epoch,
                sequence: 0,
                pixels: None,
            };
            self.dirty = true;
        }
    }

    fn release_member(&mut self, member_id: &str) {
        for slot in 0..FRAME_SLOTS {
            if self.decoders[slot]
                .as_ref()
                .is_some_and(|decoder| decoder.key.member_id == member_id)
            {
                self.release(slot);
            }
        }
    }

    fn release_all(&mut self) {
        for slot in 0..FRAME_SLOTS {
            self.release(slot);
        }
    }

    /// Keep only the newest decoded frame of every open stream. Ready when
    /// any frame arrived or any stream ended.
    fn poll_frames(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut progressed = false;
        for slot in 0..FRAME_SLOTS {
            let Some(decoder) = self.decoders[slot].as_mut() else {
                continue;
            };
            let mut ended = false;
            for _ in 0..MAX_DRAIN {
                match decoder.frames.poll_next_unpin(cx) {
                    Poll::Ready(Some(frame)) => {
                        decoder.newest = Some(frame);
                        progressed = true;
                    }
                    Poll::Ready(None) => {
                        ended = true;
                        break;
                    }
                    Poll::Pending => break,
                }
            }
            if ended {
                let key = decoder.key.clone();
                self.cooling.insert(key);
                self.release(slot);
                self.ended = true;
                progressed = true;
            }
        }
        if progressed {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    fn take_due_frames(&mut self, now: Instant) -> Vec<Conversion> {
        let mut batch = Vec::new();
        for (slot, decoder) in self.decoders.iter_mut().enumerate() {
            let Some(decoder) = decoder else {
                continue;
            };
            if decoder.due > now {
                continue;
            }
            let Some(frame) = decoder.newest.take() else {
                continue;
            };
            // Allow half a tick of timer jitter so a stream keeps its cadence.
            decoder.due = now + decoder.interval.saturating_sub(FRAME_INTERVAL / 2);
            batch.push(Conversion {
                slot,
                epoch: self.frames[slot].epoch,
                frame,
                size: decoder.size,
            });
        }
        batch
    }

    fn accept(&mut self, converted: Vec<Converted>) {
        for result in converted {
            let Ok(pixels) = result.pixels else {
                continue;
            };
            // A released or reassigned slot has a new epoch; drop late pictures.
            let frame = &mut self.frames[result.slot];
            if self.decoders[result.slot].is_some() && frame.epoch == result.epoch {
                frame.sequence += 1;
                frame.pixels = Some(pixels);
                self.dirty = true;
            }
        }
    }
}

fn convert_batch(batch: Vec<Conversion>) -> Vec<Converted> {
    batch
        .into_iter()
        .map(|conversion| Converted {
            slot: conversion.slot,
            epoch: conversion.epoch,
            pixels: rgb_buffer(&conversion.frame, conversion.size),
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameError {
    InvalidDimensions,
    InvalidPlane,
}

/// The largest size within `bounds` with the source aspect ratio.
/// Pictures are never enlarged.
fn fitted_size((width, height): (u32, u32), (max_width, max_height): (u32, u32)) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    let (width, height) = (u64::from(width), u64::from(height));
    let (bound_width, bound_height) = (u64::from(max_width), u64::from(max_height));
    if width * bound_height >= height * bound_width {
        let scaled = (height * bound_width / width).max(1);
        (max_width, u32::try_from(scaled).unwrap_or(max_height))
    } else {
        let scaled = (width * bound_height / height).max(1);
        (u32::try_from(scaled).unwrap_or(max_width), max_height)
    }
}

/// Convert BT.601 limited-range I420 to RGB, applying clockwise rotation and
/// nearest-sample scaling into `bounds`. Work is proportional to the output.
fn rgb_buffer(
    frame: &VideoFrame,
    bounds: (u32, u32),
) -> Result<SharedPixelBuffer<Rgb8Pixel>, FrameError> {
    let source = &frame.buffer;
    let width = usize::try_from(source.width).map_err(|_| FrameError::InvalidDimensions)?;
    let height = usize::try_from(source.height).map_err(|_| FrameError::InvalidDimensions)?;
    let pixels = width
        .checked_mul(height)
        .ok_or(FrameError::InvalidDimensions)?;
    if width == 0
        || height == 0
        || source.width > SOURCE_MAX_EDGE
        || source.height > SOURCE_MAX_EDGE
        || pixels > SOURCE_MAX_PIXELS
    {
        return Err(FrameError::InvalidDimensions);
    }
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let stride_y = checked_plane(&source.data_y, source.stride_y, width, height)?;
    let stride_u = checked_plane(&source.data_u, source.stride_u, chroma_width, chroma_height)?;
    let stride_v = checked_plane(&source.data_v, source.stride_v, chroma_width, chroma_height)?;
    let quarter_turn = matches!(frame.rotation, VideoRotation::Deg90 | VideoRotation::Deg270);
    let (display_width, display_height) = if quarter_turn {
        (height, width)
    } else {
        (width, height)
    };
    let display = if quarter_turn {
        (source.height, source.width)
    } else {
        (source.width, source.height)
    };
    let (output_width, output_height) = fitted_size(display, bounds);
    let mut output = SharedPixelBuffer::<Rgb8Pixel>::new(output_width, output_height);
    let output_bytes = output.make_mut_bytes();
    let output_width = output_width as usize;
    let output_height = output_height as usize;
    for output_y in 0..output_height {
        // Sample the centre of the source area each output pixel covers.
        let display_y = (output_y * 2 + 1) * display_height / (output_height * 2);
        for output_x in 0..output_width {
            let display_x = (output_x * 2 + 1) * display_width / (output_width * 2);
            let (x, y) = match frame.rotation {
                VideoRotation::Deg0 => (display_x, display_y),
                VideoRotation::Deg90 => (display_y, height - 1 - display_x),
                VideoRotation::Deg180 => (width - 1 - display_x, height - 1 - display_y),
                VideoRotation::Deg270 => (width - 1 - display_y, display_x),
            };
            let luma = source.data_y[y * stride_y + x];
            let chroma_u = source.data_u[(y / 2) * stride_u + x / 2];
            let chroma_v = source.data_v[(y / 2) * stride_v + x / 2];
            let rgb = yuv_to_rgb(luma, chroma_u, chroma_v);
            let offset = (output_y * output_width + output_x) * 3;
            output_bytes[offset..offset + 3].copy_from_slice(&rgb);
        }
    }
    Ok(output)
}

fn checked_plane(
    data: &[u8],
    stride: u32,
    visible_width: usize,
    rows: usize,
) -> Result<usize, FrameError> {
    let stride = usize::try_from(stride).map_err(|_| FrameError::InvalidPlane)?;
    let length = stride.checked_mul(rows).ok_or(FrameError::InvalidPlane)?;
    if stride < visible_width || data.len() < length {
        return Err(FrameError::InvalidPlane);
    }
    Ok(stride)
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = i32::from(y) - 16;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let channel = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
    [
        channel(298 * c + 409 * e),
        channel(298 * c - 100 * d - 208 * e),
        channel(298 * c + 516 * d),
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use morpheus_rtc::{I420Buffer, ParticipantStreamInfo};

    fn padded_frame(rotation: VideoRotation) -> VideoFrame {
        VideoFrame {
            buffer: I420Buffer {
                width: 2,
                height: 3,
                data_y: vec![16, 235, 0, 81, 145, 0, 41, 106, 0],
                stride_y: 3,
                data_u: vec![128, 0, 128, 0],
                stride_u: 2,
                data_v: vec![128, 0, 128, 0],
                stride_v: 2,
            },
            rotation,
            timestamp_us: 0,
        }
    }

    fn participant(member: &str, streams: &[(RtcStreamKind, bool)]) -> ParticipantInfo {
        ParticipantInfo {
            member_id: member.into(),
            user_id: format!("@{member}:example.org"),
            device_id: None,
            is_local: false,
            reachable: true,
            streams: streams
                .iter()
                .map(|&(kind, muted)| ParticipantStreamInfo { kind, muted })
                .collect(),
            hand_raised_at_ms: None,
            joined_at_ms: None,
        }
    }

    fn camera(member: &str) -> TileKey {
        TileKey {
            member_id: member.into(),
            medium: TileMedium::Camera,
        }
    }

    fn screen(member: &str) -> TileKey {
        TileKey {
            member_id: member.into(),
            medium: TileMedium::Screen,
        }
    }

    struct Track;

    impl RemoteTrackHandle for Track {
        fn kind(&self) -> MediaStreamKind {
            MediaStreamKind::Camera
        }

        fn video_frames(&self) -> Option<BoxStream<'static, VideoFrame>> {
            Some(futures_util::stream::pending().boxed())
        }
    }

    #[derive(Default)]
    struct FakeMedia {
        participants: Vec<ParticipantInfo>,
        tracks: Mutex<HashMap<TileKey, Arc<dyn RemoteTrackHandle>>>,
        constraints: Mutex<Vec<(TileKey, MediaConstraints)>>,
    }

    impl FakeMedia {
        fn publish(&self, key: TileKey) -> Arc<dyn RemoteTrackHandle> {
            let track: Arc<dyn RemoteTrackHandle> = Arc::new(Track);
            self.tracks.lock().unwrap().insert(key, Arc::clone(&track));
            track
        }

        fn last_constraint(&self, key: &TileKey) -> Option<MediaConstraints> {
            self.constraints
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(constrained, _)| constrained == key)
                .map(|(_, constraints)| *constraints)
        }
    }

    impl StageMedia for FakeMedia {
        fn participants(&self) -> Vec<ParticipantInfo> {
            self.participants.clone()
        }

        fn track(&self, key: &TileKey) -> Option<Arc<dyn RemoteTrackHandle>> {
            self.tracks.lock().unwrap().get(key).cloned()
        }

        fn constrain(&self, key: &TileKey, constraints: MediaConstraints) {
            self.constraints
                .lock()
                .unwrap()
                .push((key.clone(), constraints));
        }
    }

    fn plan(
        planner: &mut Planner,
        participants: &[ParticipantInfo],
        pinned: Option<&TileKey>,
        now: Instant,
    ) -> Plan {
        let speaking = HashSet::new();
        let undecryptable = HashSet::new();
        planner.plan(
            &PlanInput {
                participants,
                pinned,
                page: 0,
                presented: true,
                speaking: &speaking,
                undecryptable: &undecryptable,
                now,
            },
            |_| true,
        )
    }

    #[test]
    fn respects_padding_and_limited_range() {
        let image = rgb_buffer(&padded_frame(VideoRotation::Deg0), FEATURED_SIZE).unwrap();
        assert_eq!((image.width(), image.height()), (2, 3));
        assert_eq!(
            &image.as_bytes()[..9],
            &[0, 0, 0, 255, 255, 255, 76, 76, 76]
        );
    }

    #[test]
    fn applies_clockwise_quarter_turn() {
        let image = rgb_buffer(&padded_frame(VideoRotation::Deg90), FEATURED_SIZE).unwrap();
        assert_eq!((image.width(), image.height()), (3, 2));
        assert_eq!(&image.as_bytes()[..3], &[29, 29, 29]);
    }

    #[test]
    fn scales_to_the_tile_bound_by_sampling_pixel_centres() {
        let frame = VideoFrame {
            buffer: I420Buffer {
                width: 4,
                height: 2,
                data_y: vec![16, 16, 16, 16, 16, 235, 16, 81],
                stride_y: 4,
                data_u: vec![128, 128],
                stride_u: 2,
                data_v: vec![128, 128],
                stride_v: 2,
            },
            rotation: VideoRotation::Deg0,
            timestamp_us: 0,
        };
        let image = rgb_buffer(&frame, (2, 2)).unwrap();
        assert_eq!((image.width(), image.height()), (2, 1));
        assert_eq!(image.as_bytes(), &[255, 255, 255, 76, 76, 76]);
        assert_eq!(fitted_size((3840, 2160), FEATURED_SIZE), FEATURED_SIZE);
        assert_eq!(fitted_size((1080, 1920), FEATURED_SIZE), (607, 1080));
        assert_eq!(fitted_size((640, 360), FEATURED_SIZE), (640, 360));
    }

    #[test]
    fn rejects_short_planes_and_excessive_dimensions() {
        let mut frame = padded_frame(VideoRotation::Deg0);
        frame.buffer.data_u.pop();
        assert_eq!(
            rgb_buffer(&frame, FEATURED_SIZE).unwrap_err(),
            FrameError::InvalidPlane
        );
        frame.buffer.width = SOURCE_MAX_EDGE + 1;
        assert_eq!(
            rgb_buffer(&frame, FEATURED_SIZE).unwrap_err(),
            FrameError::InvalidDimensions
        );
    }

    #[test]
    fn screen_share_is_featured_while_cameras_stay_in_a_bounded_filmstrip() {
        let mut planner = Planner::default();
        let mut participants = vec![participant(
            "alice",
            &[
                (RtcStreamKind::Camera, false),
                (RtcStreamKind::ScreenShare, false),
            ],
        )];
        participants.extend(
            (0..10)
                .map(|index| participant(&format!("m{index}"), &[(RtcStreamKind::Camera, false)])),
        );
        let plan = plan(&mut planner, &participants, None, Instant::now());
        let featured = plan.layout.featured.as_ref().unwrap();
        assert_eq!(featured.key, screen("alice"));
        assert_eq!(plan.layout.tiles[0].key, camera("alice"));
        assert_eq!(plan.layout.tiles.len(), FILMSTRIP_PAGE);
        assert_eq!(plan.layout.pages, 11_usize.div_ceil(FILMSTRIP_PAGE));
        assert!(plan.decode.len() <= FRAME_SLOTS);
        assert_eq!(plan.decode[0].size, FEATURED_SIZE);
        assert!(
            plan.decode[1..]
                .iter()
                .all(|request| request.size == FILMSTRIP_SIZE)
        );
    }

    #[test]
    fn placeholders_describe_muted_absent_unreachable_and_local_media() {
        let mut planner = Planner::default();
        let mut unreachable = participant("carol", &[(RtcStreamKind::Camera, false)]);
        unreachable.reachable = false;
        let mut local = participant("me", &[(RtcStreamKind::Camera, false)]);
        local.is_local = true;
        let participants = [
            participant(
                "alice",
                &[
                    (RtcStreamKind::Camera, true),
                    (RtcStreamKind::Microphone, true),
                ],
            ),
            participant("bob", &[(RtcStreamKind::Microphone, false)]),
            unreachable,
            local,
        ];
        let plan = plan(&mut planner, &participants, None, Instant::now());
        let states: Vec<_> = plan
            .layout
            .tiles
            .iter()
            .map(|tile| {
                (
                    tile.key.member_id.as_str(),
                    tile.state,
                    tile.microphone_muted,
                )
            })
            .collect();
        assert_eq!(
            states,
            [
                ("alice", TileState::Muted, true),
                ("bob", TileState::Off, false),
                ("carol", TileState::Unavailable, false),
                ("me", TileState::LocalPublishing, false),
            ]
        );
        assert!(plan.decode.is_empty());
    }

    #[test]
    fn pinned_participant_who_leaves_is_reported_then_released() {
        let mut planner = Planner::default();
        let now = Instant::now();
        let bob = camera("bob");
        let both = [
            participant("alice", &[(RtcStreamKind::Camera, false)]),
            participant("bob", &[(RtcStreamKind::Camera, false)]),
        ];
        let first = plan(&mut planner, &both, Some(&bob), now);
        let pinned_id = first.layout.featured.as_ref().unwrap().id;
        let alice_id = first.layout.tiles[0].id;

        let left = plan(&mut planner, &both[..1], Some(&bob), now);
        let placeholder = left.layout.featured.as_ref().unwrap();
        assert_eq!(
            (placeholder.id, placeholder.state),
            (pinned_id, TileState::Left)
        );
        assert!(left.decode.iter().all(|request| request.key != bob));
        assert_eq!(left.layout.tiles[0].id, alice_id);

        let later = plan(
            &mut planner,
            &both[..1],
            Some(&bob),
            now + DEPARTED_PLACEHOLDER,
        );
        assert!(later.layout.featured.is_none());
        assert_eq!(later.layout.tiles[0].id, alice_id);
    }

    #[test]
    fn hidden_stage_pauses_every_remote_stream_and_drops_pictures() {
        let media = FakeMedia {
            participants: vec![
                participant("alice", &[(RtcStreamKind::ScreenShare, false)]),
                participant("bob", &[(RtcStreamKind::Camera, false)]),
            ],
            ..FakeMedia::default()
        };
        media.publish(screen("alice"));
        media.publish(camera("bob"));
        let mut stage = Stage::new(media, 4);
        stage.participants = stage.media.participants();
        stage.request(&StageRequest {
            presented: true,
            ..StageRequest::default()
        });
        stage.replan();
        assert_eq!(
            stage.media.last_constraint(&screen("alice")),
            Some(sized(FEATURED_SIZE))
        );
        assert_eq!(
            stage.media.last_constraint(&camera("bob")),
            Some(sized(FILMSTRIP_SIZE))
        );
        let slot = stage.presenter.slot_of(&camera("bob")).unwrap();
        stage.presenter.frames[slot].pixels =
            Some(rgb_buffer(&padded_frame(VideoRotation::Deg0), FILMSTRIP_SIZE).unwrap());

        stage.request(&StageRequest::default());
        stage.replan();
        // A hidden subscribed track remains live without a decoder slot. A
        // receiver may still hold this snapshot when visibility is requested.
        let hidden_camera = stage
            .layout
            .tiles
            .iter()
            .find(|tile| tile.key == camera("bob"))
            .unwrap();
        assert_eq!(hidden_camera.state, TileState::Live);
        assert_eq!(hidden_camera.frame_slot, None);
        assert!(!stage.presenter.decoding());
        assert!(
            stage
                .presenter
                .frames
                .iter()
                .all(|frame| frame.pixels.is_none())
        );
        assert_eq!(
            stage.media.last_constraint(&screen("alice")),
            Some(paused())
        );
        assert_eq!(stage.media.last_constraint(&camera("bob")), Some(paused()));

        stage.request(&StageRequest {
            presented: true,
            ..StageRequest::default()
        });
        stage.replan();
        let visible_camera = stage
            .layout
            .tiles
            .iter()
            .find(|tile| tile.key == camera("bob"))
            .unwrap();
        assert_eq!(visible_camera.state, TileState::Live);
        let visible_slot = visible_camera.frame_slot.unwrap();
        assert_ne!(stage.presenter.frames[visible_slot].epoch, 0);
        assert_eq!(stage.presenter.frames[visible_slot].sequence, 0);
        assert!(stage.presenter.frames[visible_slot].pixels.is_none());
        assert_eq!(
            stage.media.last_constraint(&camera("bob")),
            Some(sized(FILMSTRIP_SIZE))
        );
    }

    #[test]
    fn stopped_muted_or_republished_tracks_release_their_slot() {
        let media = FakeMedia {
            participants: vec![participant("bob", &[(RtcStreamKind::Camera, false)])],
            ..FakeMedia::default()
        };
        media.publish(camera("bob"));
        let mut stage = Stage::new(media, 9);
        stage.participants = stage.media.participants();
        stage.request(&StageRequest {
            presented: true,
            ..StageRequest::default()
        });
        stage.replan();
        let slot = stage.presenter.slot_of(&camera("bob")).unwrap();
        let started = RtcCallEvent::StreamStarted {
            member_id: "bob".into(),
            kind: MediaStreamKind::Camera,
        };
        // The same subscribed track announced again keeps its stream.
        stage.event(&started);
        assert_eq!(stage.presenter.slot_of(&camera("bob")), Some(slot));
        // A replacement track must never keep the old decoder or picture.
        stage.media.publish(camera("bob"));
        stage.presenter.frames[slot].pixels =
            Some(rgb_buffer(&padded_frame(VideoRotation::Deg0), FILMSTRIP_SIZE).unwrap());
        stage.event(&started);
        assert!(stage.presenter.slot_of(&camera("bob")).is_none());
        assert!(stage.presenter.frames[slot].pixels.is_none());
        stage.replan();
        assert!(stage.presenter.slot_of(&camera("bob")).is_some());
        stage.event(&RtcCallEvent::StreamMuted {
            member_id: "bob".into(),
            kind: MediaStreamKind::Camera,
        });
        assert!(!stage.presenter.decoding());
        stage.replan();
        stage.event(&RtcCallEvent::ParticipantLeft {
            member_id: "bob".into(),
        });
        assert!(!stage.presenter.decoding());
    }

    #[test]
    fn refreshed_same_key_track_replaces_its_stream_without_a_start_event() {
        struct Retirement(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Retirement {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct QueuedTrack(Mutex<Option<BoxStream<'static, VideoFrame>>>);
        impl RemoteTrackHandle for QueuedTrack {
            fn kind(&self) -> MediaStreamKind {
                MediaStreamKind::Camera
            }
            fn video_frames(&self) -> Option<BoxStream<'static, VideoFrame>> {
                self.0.lock().unwrap().take()
            }
        }
        fn queued_track(
            retired: Arc<std::sync::atomic::AtomicBool>,
        ) -> (
            Arc<dyn RemoteTrackHandle>,
            tokio::sync::mpsc::Sender<VideoFrame>,
        ) {
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            let retirement = Retirement(retired);
            let frames = futures_util::stream::poll_fn(move |cx| {
                let _ = &retirement;
                receiver.poll_recv(cx)
            })
            .boxed();
            (Arc::new(QueuedTrack(Mutex::new(Some(frames)))), sender)
        }
        fn snapshot(updates: &watch::Receiver<VideoUpdate>) -> Arc<StageSnapshot> {
            match updates.borrow().clone() {
                VideoUpdate::Stage { stage, .. } => stage,
                VideoUpdate::Clear { .. } => panic!("expected a stage snapshot"),
            }
        }

        let old_retired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let new_retired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (old_track, old_sender) = queued_track(Arc::clone(&old_retired));
        let (new_track, new_sender) = queued_track(Arc::clone(&new_retired));
        let media = FakeMedia {
            participants: vec![participant("bob", &[(RtcStreamKind::Camera, false)])],
            ..FakeMedia::default()
        };
        media
            .tracks
            .lock()
            .unwrap()
            .insert(camera("bob"), old_track);
        let mut stage = Stage::new(media, 21);
        stage.participants = stage.media.participants();
        stage.request(&StageRequest {
            token: 21,
            presented: true,
            ..StageRequest::default()
        });
        let active = AtomicU64::new(21);
        let (published, updates) = watch::channel(VideoUpdate::Clear { token: 21 });
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        stage.replan();
        let slot = stage.presenter.slot_of(&camera("bob")).unwrap();
        old_sender
            .try_send(padded_frame(VideoRotation::Deg0))
            .unwrap();
        let first_polled = stage.presenter.poll_frames(&mut context).is_ready();
        let first = convert_batch(stage.presenter.take_due_frames(Instant::now()));
        stage.presenter.accept(first);
        stage.publish(&published, &active);
        let initial = snapshot(&updates);
        let original_constraints = stage.media.constraints.lock().unwrap().len();

        // Retain a real conversion result from the old epoch across a same-key
        // replacement, just as a blocking conversion can finish after refresh.
        old_sender
            .try_send(padded_frame(VideoRotation::Deg0))
            .unwrap();
        let old_polled = stage.presenter.poll_frames(&mut context).is_ready();
        let stale = convert_batch(
            stage
                .presenter
                .take_due_frames(Instant::now() + Duration::from_secs(1)),
        );
        stage
            .media
            .tracks
            .lock()
            .unwrap()
            .insert(camera("bob"), new_track);
        // No StreamStarted event: exercise the same production replan used by
        // periodic refresh and roster changes after the SDK refreshes its Arc.
        stage.replan();
        stage.presenter.accept(stale);
        stage.publish(&published, &active);
        let replaced = snapshot(&updates);
        let replacement_constraints = stage.media.constraints.lock().unwrap().len();
        let old_closed = old_sender.is_closed();
        let old_stream_retired = old_retired.load(Ordering::SeqCst);

        new_sender
            .try_send(padded_frame(VideoRotation::Deg180))
            .unwrap();
        let new_polled = stage.presenter.poll_frames(&mut context).is_ready();
        let fresh = convert_batch(stage.presenter.take_due_frames(Instant::now()));
        stage.presenter.accept(fresh);
        stage.publish(&published, &active);
        let delivered = snapshot(&updates);
        stage.replan();
        stage.publish(&published, &active);
        let unchanged = snapshot(&updates);
        let unchanged_constraints = stage.media.constraints.lock().unwrap().len();
        let new_stream_retained = !new_retired.load(Ordering::SeqCst);

        // The ordinary hidden-stage path closes the actual new receiver too.
        stage.request(&StageRequest::default());
        stage.replan();
        let new_closed = new_sender.is_closed();
        let new_stream_retired = new_retired.load(Ordering::SeqCst);
        assert!(first_polled && old_polled && new_polled);
        assert_ne!(initial.frames[slot].epoch, 0);
        assert_eq!(initial.frames[slot].sequence, 1);
        assert!(initial.frames[slot].pixels.is_some());
        assert!(old_closed && old_stream_retired);
        assert_eq!(replaced.layout, initial.layout);
        assert_ne!(replaced.frames[slot].epoch, initial.frames[slot].epoch);
        assert_eq!(replaced.frames[slot].sequence, 0);
        assert!(replaced.frames[slot].pixels.is_none());
        assert_eq!(replacement_constraints, original_constraints + 1);
        assert_eq!(delivered.frames[slot].epoch, replaced.frames[slot].epoch);
        assert_eq!(delivered.frames[slot].sequence, 1);
        assert_eq!(
            delivered.frames[slot].pixels.as_ref().unwrap().as_slice(),
            rgb_buffer(&padded_frame(VideoRotation::Deg180), (1280, 720))
                .unwrap()
                .as_slice()
        );
        assert_eq!(unchanged.frames[slot].epoch, delivered.frames[slot].epoch);
        assert_eq!(
            unchanged.frames[slot].sequence,
            delivered.frames[slot].sequence
        );
        assert_eq!(unchanged_constraints, replacement_constraints);
        assert!(new_stream_retained && new_closed && new_stream_retired);
    }

    #[test]
    fn released_slot_discards_a_late_conversion() {
        let mut presenter = Presenter::default();
        let request = DecodeRequest {
            key: camera("bob"),
            size: FILMSTRIP_SIZE,
            interval: FRAME_INTERVAL,
        };
        let track: Arc<dyn RemoteTrackHandle> = Arc::new(Track);
        let frames = track.video_frames().unwrap();
        presenter.open(0, &request, track, frames);
        presenter.decoders[0].as_mut().unwrap().newest = Some(padded_frame(VideoRotation::Deg0));
        let batch = presenter.take_due_frames(Instant::now());
        assert_eq!(batch.len(), 1);
        let converted = convert_batch(batch);
        // Stop arrives while the converter runs.
        presenter.release(0);
        presenter.accept(converted);
        assert!(presenter.frames[0].pixels.is_none());
        assert!(!presenter.decoding());
    }

    #[test]
    fn stage_requests_from_another_call_are_ignored() {
        let mut request = StageRequest::default();
        request.choose(3, |request| request.pinned = Some(camera("bob")));
        request.present(true);
        let mut stage = Stage::new(FakeMedia::default(), 4);
        stage.request(&request);
        assert!(stage.pinned.is_none() && stage.presented);
        request.choose(4, |request| request.page = 2);
        assert!(request.pinned.is_none());
        stage.request(&request);
        assert_eq!(stage.page, 2);
    }
}

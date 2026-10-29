// Copyright 2026 Element Creations Ltd.
//
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial
// Please see LICENSE in the repository root for full details.
//
// Morpheus modification: retain same-participation media across membership lapses;
// see MORPHEUS_PATCH.md.

//! The connection pool: a session's media over N transport connections, keyed
//! by `member_id`.
//!
//! # Connections (MSC4195 multi-SFU)
//!
//! Every member publishes media on the focus they announced in their
//! membership's `transports`; subscribing to them means connecting to *their*
//! focus. The pool groups members by [`MediaTransport::connection_key`] and
//! keeps exactly one connection per key:
//!
//! - a key appearing in the snapshot opens a connection via
//!   [`MediaTransport::connect`], retried with exponential backoff while
//!   members still need it;
//! - a key whose last member left is closed after a short idle grace (so
//!   membership flaps don't churn connections);
//! - a peer-focus connection dying tears down its members' streams and
//!   reconnects — only the *own* focus (the one we publish on, adopted via
//!   [`MediaPool::adopt_own_connection`]) ending is reported as lost.
//!
//! # Identity mapping
//!
//! Transports know participants by their own identities (LiveKit: the MSC4195
//! pseudonymous identity), and the pool reverse-maps those to `member_id`s
//! using [`MediaTransport::remote_identity`]. Transport participants that map
//! to no membership never surface as members (and their media could not be
//! decrypted anyway — keys are distributed per membership). Media and keys
//! that arrive *before* their membership (SFU connects are often faster than
//! sticky event propagation) are buffered and flushed when the membership
//! lands.
//!
//! # Driving it
//!
//! The pool is a state machine, not a task: its owner (an application's actor,
//! the call SDK's engine for calls) feeds it membership snapshots and the
//! [`PoolMessage`]s from the inbox [`MediaPool::new`] returns, and applies the
//! [`PoolEvent`]s each call hands back. Owning the ordering is what lets that
//! actor keep its own state (a roster) consistent with the media, which a pool
//! running beside it could not. Connects and timers run on
//! [`matrix_rtc_core::executor`] and report back through the inbox.

use std::collections::{HashMap, HashSet};
use std::future::{Future, poll_fn};
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;
use std::time::Duration;

use matrix_rtc_core::JoinedMembership;
use matrix_rtc_core::executor as rt;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};

use crate::{
    ConnectionContext, ConnectionEvent, FrameEncryptionState, MediaConstraints, MediaStreamKind,
    MediaTransport, RemoteTrackHandle, TransportConnection, TransportError,
};

/// How long a connection with no remaining members is kept before closing,
/// so a membership flap (sticky expiry glitch, quick rejoin) doesn't tear a
/// connection down just to rebuild it.
pub const IDLE_GRACE: Duration = Duration::from_secs(10);

/// First-retry delay after a failed connect; doubles per attempt.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Retry delay ceiling.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Constraint changes are coalesced for this long before being applied —
/// scroll-driven visibility churn is the hot path, and only the final state
/// matters to the transport.
pub const CONSTRAINTS_DEBOUNCE: Duration = Duration::from_millis(150);

fn backoff_delay(attempt: u32) -> Duration {
    BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(BACKOFF_MAX)
}

/// Subscribed remote tracks, keyed by member and stream kind. Shared between
/// the pool (writes) and [`RemoteTracks`] (reads).
type TrackMap = Arc<Mutex<HashMap<(String, MediaStreamKind), PooledTrack>>>;

/// A subscribed handle and its actual source connection. This provenance
/// survives a membership lapse, so retiring the source also retires its buffer.
#[derive(Clone)]
struct PooledTrack {
    connection_key: String,
    generation: u64,
    track: Arc<dyn RemoteTrackHandle>,
    /// Participant-level state observed while this exact receiver was present.
    /// A new receiver starts unknown; state never follows only a member ID.
    encryption_state: Option<FrameEncryptionState>,
}

/// Tracks whose transport identity has no membership yet, buffered per
/// identity until the membership lands.
type PendingTracks = HashMap<String, Vec<(MediaStreamKind, PooledTrack)>>;

/// One latest intent and one worker per original membership/stream. A worker
/// settles an entered apply before observing the next intent; it never races
/// another apply from this entry. Queued intents do not retain remote tracks.
struct StoredConstraints {
    value: MediaConstraints,
    generation: u64,
    writer: ConstraintWriter,
}

#[derive(Default)]
struct ConstraintWriter {
    shared: Arc<ConstraintWrites>,
}

#[derive(Default)]
struct ConstraintWrites {
    state: Mutex<ConstraintWriteState>,
    changed: Notify,
}

impl ConstraintWrites {
    fn lock_state(&self) -> std::sync::MutexGuard<'_, ConstraintWriteState> {
        // The external first poll can unwind while admission is locked. It
        // does not mutate this private writer state. Preserve the panic as a
        // failed worker, but let a later real intent use the same writer.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Default)]
struct ConstraintWriteState {
    revision: u64,
    pending: Option<ConstraintApplication>,
    worker: Weak<()>,
    closed: bool,
}

struct ConstraintApplication {
    connection: Arc<dyn TransportConnection>,
    serial: Arc<AsyncMutex<()>>,
    identity: String,
    kind: MediaStreamKind,
    resolved: crate::ResolvedConstraints,
    revision: u64,
}

impl ConstraintWriter {
    fn invalidate(&self) {
        let mut state = self.shared.lock_state();
        state.revision += 1;
        let pending = state.pending.take();
        drop(state);
        self.shared.changed.notify_one();
        drop(pending);
    }

    fn submit(&self, mut application: ConstraintApplication) -> Option<rt::JoinHandle<()>> {
        let mut state = self.shared.lock_state();
        if state.closed {
            return None;
        }
        state.revision += 1;
        application.revision = state.revision;
        let pending = state.pending.replace(application);
        let worker = if state.worker.upgrade().is_none() {
            let worker = Arc::new(());
            state.worker = Arc::downgrade(&worker);
            Some(worker)
        } else {
            None
        };
        drop(state);
        self.shared.changed.notify_one();
        drop(pending);
        worker.map(|worker| rt::spawn(run_constraint_writer(self.shared.clone(), worker)))
    }
}

impl Drop for ConstraintWriter {
    fn drop(&mut self) {
        let mut state = self.shared.lock_state();
        state.closed = true;
        let pending = state.pending.take();
        drop(state);
        self.shared.changed.notify_one();
        drop(pending);
    }
}

async fn run_constraint_writer(shared: Arc<ConstraintWrites>, worker: Arc<()>) {
    let mut worker = Some(worker);
    loop {
        let application = {
            let mut state = shared.lock_state();
            if state.closed {
                worker.take();
                return;
            }
            match state.pending.take() {
                Some(application) => application,
                None => {
                    // Retire under the same lock used by submit, closing the
                    // last-item/worker-exit race without a detached successor.
                    worker.take();
                    return;
                }
            }
        };
        let serial = {
            let mut lock = std::pin::pin!(application.serial.lock());
            let mut changed = std::pin::pin!(shared.changed.notified());
            poll_fn(|cx| {
                loop {
                    {
                        let state = shared.lock_state();
                        if state.closed || state.revision != application.revision {
                            return Poll::Ready(None);
                        }
                    }
                    if changed.as_mut().poll(cx).is_pending() {
                        break;
                    }
                    changed.set(shared.changed.notified());
                }
                lock.as_mut().poll(cx).map(Some)
            })
            .await
        };
        let Some(_serial) = serial else { continue };
        // Validate and admit the actual trait call's first poll atomically
        // with removal/invalidation. The synchronous guard is released as
        // soon as that poll returns, including Pending; it never spans await.
        let first_poll = poll_fn(|cx| {
            let state = shared.lock_state();
            if state.closed || state.revision != application.revision {
                return Poll::Ready(None);
            }
            let mut apply = application.connection.apply_constraints(
                &application.identity,
                application.kind,
                application.resolved,
            );
            let result = apply.as_mut().poll(cx);
            drop(state);
            Poll::Ready(Some((apply, result)))
        })
        .await;
        let Some((apply, first_poll)) = first_poll else {
            continue;
        };
        // Only the original connection's async serial mutex remains held.
        // Once admitted, settle this operation before the next/latest intent.
        let result = match first_poll {
            Poll::Ready(result) => result,
            Poll::Pending => apply.await,
        };
        if let Err(error) = result {
            log::warn!(
                "applying constraints for {} ({:?}) failed: {error}",
                application.identity,
                application.kind
            );
        }
    }
}

/// What [`MediaTransport::connect`] resolves to, as carried in the inbox.
type ConnectOutcome = Result<
    (
        Box<dyn TransportConnection>,
        mpsc::UnboundedReceiver<ConnectionEvent>,
    ),
    TransportError,
>;

/// Static configuration of a [`MediaPool`].
pub struct PoolConfig {
    /// Transport backends, in descending order of preference.
    pub transports: Vec<Arc<dyn MediaTransport>>,
    /// Session-scoped context passed to [`MediaTransport::connect`].
    pub ctx: ConnectionContext,
    /// Connection key of the focus we publish on. The pool never opens this
    /// one itself — the owner establishes it (so a join can fail fast) and
    /// hands it over via [`MediaPool::adopt_own_connection`].
    pub own_connection_key: Option<String>,
}

/// What the pool reports, keyed by `member_id`.
#[derive(Clone, Debug, PartialEq)]
pub enum PoolEvent {
    /// A member's stream is subscribed; its handle is in [`RemoteTracks`].
    StreamAdded {
        member_id: String,
        kind: MediaStreamKind,
    },
    /// A member's stream went away (unpublished, or its connection died).
    StreamRemoved {
        member_id: String,
        kind: MediaStreamKind,
    },
    /// The sender muted or unmuted one of their streams.
    StreamMuted {
        member_id: String,
        kind: MediaStreamKind,
        muted: bool,
    },
    /// Who is speaking now, as the transport reports it. Only members are
    /// listed: a level nothing maps to a membership is not actionable.
    ActiveSpeakers { speakers: Vec<ActiveSpeaker> },
    /// The frame cryptor's verdict on a member's media changed.
    EncryptionStateChanged {
        member_id: String,
        state: FrameEncryptionState,
    },
    /// A media key for a member was imported (reported through
    /// [`MediaPool::key_imported`], released once the membership is known),
    /// with the transport identity it was installed under.
    KeyImported {
        member_id: String,
        identity: String,
        key_index: u8,
    },
    /// A transport participant joined that maps to no membership.
    UnknownParticipant { identity: String },
    /// Whether any connection is impaired (reconnecting or failing to
    /// connect). Reported on transitions only.
    Degraded(bool),
    /// The own-focus connection is gone; nothing we publish reaches anyone.
    OwnConnectionLost { message: String },
}

/// One speaking member and how loud they are.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveSpeaker {
    pub member_id: String,
    /// `0.0` (silent) to `1.0` (loudest).
    pub level: f32,
}

/// A connect result, connection event or timer for the pool, from the inbox
/// [`MediaPool::new`] returns. Opaque: hand it to [`MediaPool::handle`].
pub struct PoolMessage(Message);

enum Message {
    /// An event from a pooled connection. `generation` guards against events
    /// of a replaced connection being applied to its successor.
    Connection {
        connection_key: String,
        generation: u64,
        event: ConnectionEvent,
    },
    /// A pooled connection's event stream ended.
    ConnectionEnded {
        connection_key: String,
        generation: u64,
    },
    /// A [`MediaTransport::connect`] attempt resolved.
    ConnectFinished {
        connection_key: String,
        attempt: u32,
        result: ConnectOutcome,
    },
    /// A backoff timer elapsed; try connecting again.
    RetryConnect {
        connection_key: String,
        attempt: u32,
    },
    /// An idle grace timer elapsed; close the connection if still unneeded.
    CloseIfIdle {
        connection_key: String,
        idle_generation: u64,
    },
    /// A constraints debounce timer elapsed; apply if still current.
    ApplyConstraints {
        member_id: String,
        kind: MediaStreamKind,
        generation: u64,
    },
}

/// Read access to the subscribed remote tracks, from outside the owner's
/// actor. Cheap to clone.
#[derive(Clone)]
pub struct RemoteTracks(TrackMap);

impl RemoteTracks {
    /// The handle for a member's subscribed stream; `None` while no such
    /// stream is up (see [`PoolEvent::StreamAdded`]).
    pub fn get(
        &self,
        member_id: &str,
        kind: MediaStreamKind,
    ) -> Option<Arc<dyn RemoteTrackHandle>> {
        self.0
            .lock()
            .expect("track map mutex poisoned")
            .get(&(member_id.to_owned(), kind))
            .map(|pooled| pooled.track.clone())
    }

    /// [`RemoteTracks::get`] for many streams under one lock, in order.
    pub fn get_many(
        &self,
        streams: &[(String, MediaStreamKind)],
    ) -> Vec<Option<Arc<dyn RemoteTrackHandle>>> {
        let map = self.0.lock().expect("track map mutex poisoned");
        streams
            .iter()
            .map(|key| map.get(key).map(|pooled| pooled.track.clone()))
            .collect()
    }
}

/// A pooled connection to one focus.
struct ManagedConnection {
    /// Backend that opened (and re-opens) this connection. `None` for the
    /// adopted own-focus connection, which the pool never re-opens.
    backend: Option<Arc<dyn MediaTransport>>,
    /// Members whose media lives on this focus, per the latest snapshot.
    members: HashSet<String>,
    /// Whether this is the focus we publish on.
    is_own: bool,
    state: ConnState,
    /// Bumped every time a live connection is installed; events carrying an
    /// older generation belong to a replaced connection and are dropped.
    generation: u64,
    /// Bumped whenever the member set changes; invalidates pending idle-close
    /// timers.
    idle_generation: u64,
}

enum ConnState {
    Connecting {
        attempt: u32,
    },
    Up {
        // Arc, not Box: publish/apply-constraints calls run in spawned tasks
        // that need shared ownership while the entry stays in the pool.
        connection: Arc<dyn TransportConnection>,
        /// Also serializes a removed/recreated constraint entry on this exact
        /// native connection; a replacement connection gets its own mutex.
        constraints_serial: Arc<AsyncMutex<()>>,
    },
    Backoff {
        attempt: u32,
    },
}

/// One session's media connections and the identity mapping over them.
pub struct MediaPool {
    transports: Vec<Arc<dyn MediaTransport>>,
    ctx: ConnectionContext,
    own_connection_key: Option<String>,
    /// Handed to connection forwarders and timers so everything funnels into
    /// the owner's inbox.
    messages_tx: mpsc::UnboundedSender<PoolMessage>,
    tracks: TrackMap,
    /// The latest membership snapshot, kept for pool reconciliation.
    members_snapshot: Vec<JoinedMembership>,
    /// Every `member_id` of the latest snapshot, mapped or not.
    known_members: HashSet<String>,
    /// Transport identity → `member_id`.
    identity_map: HashMap<String, String>,
    /// `member_id` → transport identity (the reverse of `identity_map`),
    /// for pushing constraints at a member's connection.
    member_identities: HashMap<String, String>,
    /// Latest constraints per stream, with a generation counter that
    /// invalidates superseded debounce timers.
    constraints: HashMap<(String, MediaStreamKind), StoredConstraints>,
    /// Media that arrived before its membership, flushed when it lands.
    pending_tracks: PendingTracks,
    /// Imported key indices awaiting their membership, in arrival order.
    ///
    /// A `Vec`, not a single index: at join a member can be handed more than one
    /// index before their sticky membership lands (their first key plus a
    /// rotation, say), and keeping only the latest silently dropped the others —
    /// so the host saw one key import where the core had imported several, and
    /// could not tell which index the transport actually held.
    pending_keys: HashMap<String, Vec<u8>>,
    pool: HashMap<String, ManagedConnection>,
    connection_generation: u64,
    /// Connections currently impaired (reconnecting or failing to connect);
    /// media is reported degraded while this is non-empty.
    degraded_keys: HashSet<String>,
    /// Events produced by the call in progress, handed back when it returns.
    out: Vec<PoolEvent>,
}

impl MediaPool {
    /// A pool with no members and no connections, and the inbox its owner
    /// must drain into [`MediaPool::handle`].
    pub fn new(config: PoolConfig) -> (MediaPool, mpsc::UnboundedReceiver<PoolMessage>) {
        let (messages_tx, messages_rx) = mpsc::unbounded_channel();
        // On wasm32 the map is `!Send` (track handles hold JS values), but it
        // is still shared — pool and readers — so `Arc` stays, uncontended.
        #[cfg_attr(target_arch = "wasm32", expect(clippy::arc_with_non_send_sync))]
        let tracks: TrackMap = Arc::new(Mutex::new(HashMap::new()));
        let pool = MediaPool {
            transports: config.transports,
            ctx: config.ctx,
            own_connection_key: config.own_connection_key,
            messages_tx,
            tracks,
            members_snapshot: Vec::new(),
            known_members: HashSet::new(),
            identity_map: HashMap::new(),
            member_identities: HashMap::new(),
            constraints: HashMap::new(),
            pending_tracks: HashMap::new(),
            pending_keys: HashMap::new(),
            pool: HashMap::new(),
            connection_generation: 0,
            degraded_keys: HashSet::new(),
            out: Vec::new(),
        };
        (pool, messages_rx)
    }

    /// Read access to the subscribed tracks, for use outside the owner.
    pub fn remote_tracks(&self) -> RemoteTracks {
        RemoteTracks(self.tracks.clone())
    }

    /// Whether any backend can serve one of the member's advertised
    /// transports — i.e. whether their media can reach us at all.
    pub fn is_reachable(&self, member: &JoinedMembership) -> bool {
        select_transport(&self.transports, member).is_some()
    }

    /// The own-focus connection, while it is up.
    pub fn own_connection(&self) -> Option<Arc<dyn TransportConnection>> {
        self.pool
            .values()
            .find(|entry| entry.is_own)
            .and_then(|entry| match &entry.state {
                ConnState::Up { connection, .. } => Some(connection.clone()),
                _ => None,
            })
    }

    /// Apply the latest membership snapshot: map the members that joined
    /// (releasing their buffered media and keys), forget the ones that left,
    /// and open or idle connections to match.
    pub fn apply_snapshot(&mut self, snapshot: &[JoinedMembership]) -> Vec<PoolEvent> {
        let current: HashSet<String> = snapshot
            .iter()
            .map(|member| member.member_id.clone())
            .collect();
        // Departures first, so a member_id rejoining in the same snapshot
        // (new membership, same key space) starts clean.
        let departed: Vec<String> = self.known_members.difference(&current).cloned().collect();
        for member_id in departed {
            self.forget_member(&member_id);
        }
        for member in snapshot {
            if self.known_members.insert(member.member_id.clone()) {
                self.map_member(member);
            }
        }
        self.members_snapshot = snapshot.to_vec();
        self.reconcile_pool();
        std::mem::take(&mut self.out)
    }

    /// Take ownership of the owner-established own-focus connection: its
    /// events are consumed here from now on. Closing it stays with the owner,
    /// so a clean leave can report the close result.
    pub fn adopt_own_connection(
        &mut self,
        connection: Box<dyn TransportConnection>,
        events: mpsc::UnboundedReceiver<ConnectionEvent>,
    ) {
        let key = connection.connection_key().to_owned();
        self.drop_pending_from(&key);
        self.connection_generation += 1;
        let generation = self.connection_generation;
        let members = self.members_on_key(&key);
        self.invalidate_connection_constraints(&key);
        self.pool.insert(
            key.clone(),
            ManagedConnection {
                backend: None,
                members,
                is_own: true,
                state: ConnState::Up {
                    connection: Arc::from(connection),
                    constraints_serial: Arc::new(AsyncMutex::new(())),
                },
                generation,
                idle_generation: 0,
            },
        );
        self.spawn_forwarder(key.clone(), generation, events);
        self.reapply_connection_constraints(&key);
    }

    /// A media decryption key for a transport `identity` was imported.
    /// Reported against its member at once, or once their membership lands
    /// (to-device keys regularly beat sticky memberships).
    pub fn key_imported(&mut self, identity: String, key_index: u8) -> Vec<PoolEvent> {
        match self.identity_map.get(&identity) {
            Some(member_id) => vec![PoolEvent::KeyImported {
                member_id: member_id.clone(),
                identity,
                key_index,
            }],
            None => {
                let pending = self.pending_keys.entry(identity).or_default();
                if !pending.contains(&key_index) {
                    pending.push(key_index);
                }
                Vec::new()
            }
        }
    }

    /// Set the subscription constraints for one stream of one member.
    ///
    /// Applied after a short debounce (rapid changes coalesce, e.g. while
    /// scrolling a participant grid) and re-applied whenever the stream
    /// (re)appears or its connection resumes. They die with the membership.
    pub fn set_constraints(
        &mut self,
        member_id: String,
        kind: MediaStreamKind,
        constraints: MediaConstraints,
    ) {
        let entry = self
            .constraints
            .entry((member_id.clone(), kind))
            .or_insert_with(|| StoredConstraints {
                value: constraints,
                generation: 0,
                writer: ConstraintWriter::default(),
            });
        entry.value = constraints;
        entry.generation += 1;
        entry.writer.invalidate();
        let generation = entry.generation;
        let messages = self.messages_tx.clone();
        rt::spawn(async move {
            rt::sleep(CONSTRAINTS_DEBOUNCE).await;
            let _ = messages.send(PoolMessage(Message::ApplyConstraints {
                member_id,
                kind,
                generation,
            }));
        });
    }

    /// Close every peer-focus connection and forget them all. The adopted
    /// own-focus connection is never closed here (its owner closes it; at
    /// this point it is usually already gone).
    pub fn close(&mut self) {
        // Close queued writers before the connections are released. An already
        // entered apply still owns its original connection until it returns.
        self.constraints.clear();
        self.pending_tracks.clear();
        self.pending_keys.clear();
        for (_, entry) in self.pool.drain() {
            if let ConnState::Up { connection, .. } = entry.state
                && !entry.is_own
            {
                rt::spawn(async move {
                    if let Err(error) = connection.close().await {
                        log::debug!("closing connection on shutdown failed: {error}");
                    }
                });
            }
        }
    }

    /// Irreversibly retire this call's routes, including the adopted own
    /// connection and successful connects still queued in the actor inbox.
    /// The caller must close every returned connection; the inbox cannot reopen.
    pub fn retire(
        &mut self,
        inbox: &mut mpsc::UnboundedReceiver<PoolMessage>,
    ) -> Vec<Arc<dyn TransportConnection>> {
        inbox.close();
        let mut connections = Vec::new();
        while let Ok(PoolMessage(message)) = inbox.try_recv() {
            if let Message::ConnectFinished { result: Ok((connection, _events)), .. } = message {
                connections.push(Arc::from(connection));
            }
        }
        self.constraints.clear();
        self.pending_tracks.clear();
        self.pending_keys.clear();
        self.tracks.lock().expect("track map mutex poisoned").clear();
        self.members_snapshot.clear();
        self.known_members.clear();
        self.identity_map.clear();
        self.member_identities.clear();
        self.degraded_keys.clear();
        self.out.clear();
        for (_, entry) in self.pool.drain() {
            if let ConnState::Up { connection, .. } = entry.state {
                connections.push(connection);
            }
        }
        connections
    }

    /// Close a completed connection consumed after its call was refused.
    pub fn discard_retired(message: PoolMessage) {
        if let PoolMessage(Message::ConnectFinished {
            result: Ok((connection, _events)), ..
        }) = message {
            rt::spawn(async move { let _ = connection.close().await; });
        }
    }

    /// Process one message from the inbox.
    pub fn handle(&mut self, PoolMessage(message): PoolMessage) -> Vec<PoolEvent> {
        match message {
            Message::Connection {
                connection_key,
                generation,
                event,
            } => {
                let current = self.pool.get(&connection_key).is_some_and(|entry| {
                    entry.generation == generation && matches!(entry.state, ConnState::Up { .. })
                });
                if current {
                    self.handle_connection_event(&connection_key, generation, event);
                } else {
                    log::trace!("dropping event of replaced connection {connection_key}");
                }
            }
            Message::ConnectionEnded {
                connection_key,
                generation,
            } => {
                self.connection_down(&connection_key, generation, "connection event stream ended");
            }
            Message::ConnectFinished {
                connection_key,
                attempt,
                result,
            } => self.connect_finished(&connection_key, attempt, result),
            Message::RetryConnect {
                connection_key,
                attempt,
            } => self.retry_connect(&connection_key, attempt),
            Message::CloseIfIdle {
                connection_key,
                idle_generation,
            } => self.close_if_idle(&connection_key, idle_generation),
            Message::ApplyConstraints {
                member_id,
                kind,
                generation,
            } => {
                // Only the newest timer applies; older ones were superseded.
                if self
                    .constraints
                    .get(&(member_id.clone(), kind))
                    .is_some_and(|current| current.generation == generation)
                {
                    self.apply_constraints_now(&member_id, kind);
                }
            }
        }
        std::mem::take(&mut self.out)
    }

    // ---- members ------------------------------------------------------

    fn map_member(&mut self, member: &JoinedMembership) {
        let identity = self
            .transports
            .iter()
            .find_map(|backend| backend.remote_identity(member));
        let Some(identity) = identity else {
            // Their media arrives under an identity nothing maps back, so it
            // will be buffered forever rather than surfacing as their stream.
            log::warn!(
                "member {} ({}) has no transport identity; their media cannot be attributed",
                member.member_id,
                member.sender,
            );
            return;
        };
        log::debug!(
            "member {} ({}) maps to transport identity {identity}",
            member.member_id,
            member.sender,
        );
        self.identity_map
            .insert(identity.clone(), member.member_id.clone());
        self.member_identities
            .insert(member.member_id.clone(), identity.clone());

        // Media and keys that arrived before this membership.
        if let Some(pending) = self.pending_tracks.remove(&identity) {
            log::debug!(
                "flushing {} buffered track(s) onto member {}",
                pending.len(),
                member.member_id,
            );
            for (kind, track) in pending {
                if self.track_is_current(&track) {
                    self.add_track(&member.member_id, kind, track);
                }
            }
            // The owner clears member state on departure, but an unchanged
            // cryptor need not repeat its last transition. Replay only after
            // every winning receiver has been restored and agrees: an unknown
            // replacement must not inherit a different receiver's state.
            if let Some(state) = self.restored_encryption_state(&member.member_id) {
                self.out.push(PoolEvent::EncryptionStateChanged {
                    member_id: member.member_id.clone(),
                    state,
                });
            }
        }
        for key_index in self.pending_keys.remove(&identity).unwrap_or_default() {
            self.out.push(PoolEvent::KeyImported {
                member_id: member.member_id.clone(),
                identity: identity.clone(),
                key_index,
            });
        }
    }

    fn forget_member(&mut self, member_id: &str) {
        self.known_members.remove(member_id);
        let identity = self.member_identities.remove(member_id);
        // A stale key index could otherwise resurface against the identity.
        if let Some(identity) = &identity {
            self.pending_keys.remove(identity);
        }
        self.identity_map.retain(|_, mapped| mapped != member_id);
        // A rejoining member gets a fresh member_id, so their constraints
        // die with the membership.
        self.constraints
            .retain(|(member, _), _| member != member_id);
        // Their tracks go back to the buffer, not away: the same participation
        // can return under the same member_id (a membership that lapsed and is
        // republished, 034 R17/R19), and the SFU, which kept carrying their
        // media, will not announce it again. A removal or the connection going
        // clears the buffer as for any media that beat its membership.
        let departed: Vec<(MediaStreamKind, PooledTrack)> = {
            let mut tracks = self.tracks.lock().expect("track map mutex poisoned");
            let kinds: Vec<MediaStreamKind> = tracks
                .keys()
                .filter(|(track_member, _)| track_member == member_id)
                .map(|(_, kind)| *kind)
                .collect();
            kinds
                .into_iter()
                .filter_map(|kind| {
                    tracks
                        .remove(&(member_id.to_owned(), kind))
                        .map(|pooled| (kind, pooled))
                })
                .collect()
        };
        // An active handle can outlive its original focus in the public map.
        // Preserve that existing presentation lifetime, but never park it after
        // its exact connection incarnation has retired or been replaced.
        let departed: Vec<_> = departed
            .into_iter()
            .filter(|(_, pooled)| self.track_is_current(pooled))
            .collect();
        if let Some(identity) = identity
            && !departed.is_empty()
        {
            self.pending_tracks
                .entry(identity)
                .or_default()
                .extend(departed);
        }
    }

    /// Only an exact live connection incarnation can supply parked media.
    fn track_is_current(&self, pooled: &PooledTrack) -> bool {
        self.pool.get(&pooled.connection_key).is_some_and(|entry| {
            entry.generation == pooled.generation && matches!(entry.state, ConnState::Up { .. })
        })
    }

    /// Only agreeing observations owned by all current receivers can be replayed.
    fn restored_encryption_state(&self, member_id: &str) -> Option<FrameEncryptionState> {
        let tracks = self.tracks.lock().expect("track map mutex poisoned");
        let mut state: Option<FrameEncryptionState> = None;
        for ((track_member, _), pooled) in tracks.iter() {
            if track_member != member_id {
                continue;
            }
            if !self.track_is_current(pooled) {
                return None;
            }
            let observed = pooled.encryption_state?;
            if state.is_some_and(|state| state != observed) {
                return None;
            }
            state = Some(observed);
        }
        state
    }

    /// Media buffered from `key` is gone with its connection.
    fn drop_pending_from(&mut self, key: &str) {
        self.pending_tracks.retain(|_, pending| {
            pending.retain(|(_, pooled)| pooled.connection_key != key);
            !pending.is_empty()
        });
    }

    // ---- streams ------------------------------------------------------

    fn add_track(&mut self, member_id: &str, kind: MediaStreamKind, pooled: PooledTrack) {
        let replaced = self
            .tracks
            .lock()
            .expect("track map mutex poisoned")
            .insert((member_id.to_owned(), kind), pooled)
            .is_some();
        if replaced {
            // Same stream re-announced (e.g. events replayed on attach); the
            // handle is refreshed, but it is not a new stream.
            return;
        }
        self.out.push(PoolEvent::StreamAdded {
            member_id: member_id.to_owned(),
            kind,
        });
        // A fresh subscription starts with server-default settings; push the
        // stored constraints at it immediately (no debounce — nothing to
        // coalesce with).
        if self.constraints.contains_key(&(member_id.to_owned(), kind)) {
            self.apply_constraints_now(member_id, kind);
        }
    }

    /// Removes a member's stream, if it came over `connection_key`: a removal
    /// the old connection reports after the member moved is not theirs.
    fn remove_track(
        &mut self,
        member_id: &str,
        kind: MediaStreamKind,
        connection_key: &str,
        generation: u64,
    ) {
        let removed = {
            let mut tracks = self.tracks.lock().expect("track map mutex poisoned");
            let key = (member_id.to_owned(), kind);
            let ours = tracks.get(&key).is_some_and(|pooled| {
                pooled.connection_key == connection_key && pooled.generation == generation
            });
            ours && tracks.remove(&key).is_some()
        };
        if removed {
            self.out.push(PoolEvent::StreamRemoved {
                member_id: member_id.to_owned(),
                kind,
            });
        }
    }

    /// Push the resolved constraints for one stream to the connection its
    /// member lives on. No-op while the member, its identity, or its
    /// connection is missing — a new track and a reconnect re-apply.
    fn apply_constraints_now(
        &self,
        member_id: &str,
        kind: MediaStreamKind,
    ) -> Option<rt::JoinHandle<()>> {
        let constraints = self.constraints.get(&(member_id.to_owned(), kind))?;
        let identity = self.member_identities.get(member_id)?.clone();
        let (connection, serial) = self
            .pool
            .values()
            .find(|entry| entry.members.contains(member_id))
            .and_then(|entry| match &entry.state {
                ConnState::Up {
                    connection,
                    constraints_serial,
                } => Some((connection.clone(), constraints_serial.clone())),
                _ => None,
            })?;
        constraints.writer.submit(ConstraintApplication {
            connection,
            serial,
            identity,
            kind,
            resolved: constraints.value.resolve(kind),
            revision: 0,
        })
    }

    fn invalidate_connection_constraints(&self, key: &str) {
        let Some(entry) = self.pool.get(key) else {
            return;
        };
        for ((member_id, _), constraints) in &self.constraints {
            if entry.members.contains(member_id) {
                constraints.writer.invalidate();
            }
        }
    }

    /// Re-apply every stored constraint for members living on `key` (used
    /// after a transport-level reconnect: subscription settings are
    /// server-side state of the connection).
    fn reapply_connection_constraints(&self, key: &str) {
        let Some(entry) = self.pool.get(key) else {
            return;
        };
        for (member_id, kind) in self.constraints.keys() {
            if entry.members.contains(member_id) {
                self.apply_constraints_now(member_id, *kind);
            }
        }
    }

    // ---- connections --------------------------------------------------

    /// Sync the pool with the latest snapshot: update per-connection member
    /// sets, schedule idle closes, open connections for new focus groups.
    fn reconcile_pool(&mut self) {
        let mut desired: HashMap<String, (Arc<dyn MediaTransport>, HashSet<String>)> =
            HashMap::new();
        for member in &self.members_snapshot {
            if let Some((backend, key)) = select_transport(&self.transports, member) {
                desired
                    .entry(key)
                    .or_insert_with(|| (backend, HashSet::new()))
                    .1
                    .insert(member.member_id.clone());
            }
        }

        log::debug!(
            "focus grouping: {}",
            desired
                .iter()
                .map(|(key, (_, members))| format!("{key} -> {} member(s)", members.len()))
                .collect::<Vec<_>>()
                .join(", "),
        );

        for (key, entry) in &mut self.pool {
            entry.members = desired.remove(key).map(|(_, m)| m).unwrap_or_default();
            // Any member-set change invalidates pending idle timers; an empty
            // set (re)arms one.
            entry.idle_generation += 1;
            if entry.members.is_empty() && !entry.is_own {
                log::debug!("peer focus {key} has no members left; arming the idle timer");
                let idle_generation = entry.idle_generation;
                let messages = self.messages_tx.clone();
                let key = key.clone();
                rt::spawn(async move {
                    rt::sleep(IDLE_GRACE).await;
                    let _ = messages.send(PoolMessage(Message::CloseIfIdle {
                        connection_key: key,
                        idle_generation,
                    }));
                });
            }
        }

        for (key, (backend, members)) in desired {
            // The own focus is established by the owner and adopted; never
            // race it with a pool-initiated connect.
            if self.own_connection_key.as_deref() == Some(key.as_str()) {
                continue;
            }
            log::info!(
                "connecting to peer focus {key} for {} member(s)",
                members.len(),
            );
            self.pool.insert(
                key.clone(),
                ManagedConnection {
                    backend: Some(backend),
                    members,
                    is_own: false,
                    state: ConnState::Connecting { attempt: 0 },
                    generation: 0,
                    idle_generation: 0,
                },
            );
            self.start_connect(&key, 0);
        }
    }

    /// Spawn a connect attempt for an existing pool entry.
    fn start_connect(&mut self, key: &str, attempt: u32) {
        let Some(entry) = self.pool.get_mut(key) else {
            return;
        };
        let Some(backend) = entry.backend.clone() else {
            return;
        };
        entry.state = ConnState::Connecting { attempt };

        let ctx = self.ctx.clone();
        let messages = self.messages_tx.clone();
        let key = key.to_owned();
        rt::spawn(async move {
            let result = backend.connect(&key, &ctx).await;
            if let Err(error) = messages.send(PoolMessage(Message::ConnectFinished {
                connection_key: key,
                attempt,
                result,
            })) {
                if let PoolMessage(Message::ConnectFinished {
                    result: Ok((connection, _events)), ..
                }) = error.0 {
                    let _ = connection.close().await;
                }
            }
        });
    }

    fn retry_connect(&mut self, key: &str, attempt: u32) {
        let Some(entry) = self.pool.get(key) else {
            return;
        };
        if !matches!(entry.state, ConnState::Backoff { attempt: a } if a == attempt) {
            return;
        }
        if entry.members.is_empty() && !entry.is_own {
            self.drop_pending_from(key);
            self.pool.remove(key);
            self.clear_degraded(key);
            return;
        }
        self.start_connect(key, attempt);
    }

    fn close_if_idle(&mut self, key: &str, idle_generation: u64) {
        let Some(entry) = self.pool.get(key) else {
            return;
        };
        if entry.idle_generation != idle_generation || !entry.members.is_empty() || entry.is_own {
            return;
        }
        log::debug!("closing idle connection {key}");
        self.drop_pending_from(key);
        if let Some(entry) = self.pool.remove(key)
            && let ConnState::Up { connection, .. } = entry.state
        {
            rt::spawn(async move {
                if let Err(error) = connection.close().await {
                    log::debug!("closing idle connection failed: {error}");
                }
            });
        }
        self.clear_degraded(key);
    }

    fn connect_finished(&mut self, key: &str, attempt: u32, result: ConnectOutcome) {
        let close_stray = |result: ConnectOutcome| {
            if let Ok((connection, _events)) = result {
                rt::spawn(async move {
                    let _ = connection.close().await;
                });
            }
        };

        let (still_needed, matches_attempt) = match self.pool.get(key) {
            // The group emptied and was removed while connecting.
            None => (false, false),
            Some(entry) => (
                !entry.members.is_empty() || entry.is_own,
                matches!(entry.state, ConnState::Connecting { attempt: a } if a == attempt),
            ),
        };
        if !matches_attempt {
            close_stray(result);
            return;
        }
        if !still_needed {
            self.drop_pending_from(key);
            self.pool.remove(key);
            self.clear_degraded(key);
            close_stray(result);
            return;
        }

        match result {
            Ok((connection, events)) => {
                self.drop_pending_from(key);
                self.connection_generation += 1;
                let generation = self.connection_generation;
                let entry = self.pool.get_mut(key).expect("entry checked above");
                entry.generation = generation;
                entry.state = ConnState::Up {
                    connection: Arc::from(connection),
                    constraints_serial: Arc::new(AsyncMutex::new(())),
                };
                log::info!("media connection up: {key}");
                self.spawn_forwarder(key.to_owned(), generation, events);
                self.clear_degraded(key);
                self.reapply_connection_constraints(key);
            }
            Err(error) => {
                log::warn!("connecting to {key} failed (attempt {attempt}): {error}");
                self.mark_degraded(key);
                let next = attempt + 1;
                let entry = self.pool.get_mut(key).expect("entry checked above");
                entry.state = ConnState::Backoff { attempt: next };
                self.schedule_retry(key, next, backoff_delay(next));
            }
        }
    }

    fn schedule_retry(&self, key: &str, attempt: u32, delay: Duration) {
        let messages = self.messages_tx.clone();
        let key = key.to_owned();
        rt::spawn(async move {
            rt::sleep(delay).await;
            let _ = messages.send(PoolMessage(Message::RetryConnect {
                connection_key: key,
                attempt,
            }));
        });
    }

    /// A live connection is gone (transport `Closed` event or its event
    /// stream ending). Own focus ⇒ reported lost; peer focus ⇒ tear down its
    /// members' streams and reconnect while still needed.
    fn connection_down(&mut self, key: &str, generation: u64, message: &str) {
        let Some(entry) = self.pool.get(key) else {
            return;
        };
        if entry.generation != generation || !matches!(entry.state, ConnState::Up { .. }) {
            return;
        }

        let is_own = entry.is_own;
        let members = entry.members.clone();
        self.invalidate_connection_constraints(key);
        self.drop_pending_from(key);
        if is_own {
            log::warn!("own-focus connection {key} is gone: {message}");
            self.pool.remove(key);
            self.out.push(PoolEvent::OwnConnectionLost {
                message: message.to_owned(),
            });
            return;
        }

        log::warn!("peer-focus connection {key} is gone ({message}); reconnecting");
        let mut lost: Vec<(String, MediaStreamKind)> = self
            .tracks
            .lock()
            .expect("track map mutex poisoned")
            .iter()
            .filter(|(_, pooled)| pooled.connection_key == key)
            .map(|(stream, _)| stream.clone())
            .collect();
        // The map is a `HashMap`; the events derived from it must come out
        // in a stable order.
        lost.sort();
        for (member_id, kind) in lost {
            self.remove_track(&member_id, kind, key, generation);
        }
        self.mark_degraded(key);

        if members.is_empty() {
            self.pool.remove(key);
            self.clear_degraded(key);
            return;
        }
        let entry = self.pool.get_mut(key).expect("entry checked above");
        entry.state = ConnState::Backoff { attempt: 0 };
        self.schedule_retry(key, 0, backoff_delay(0));
    }

    fn spawn_forwarder(
        &self,
        connection_key: String,
        generation: u64,
        mut events: mpsc::UnboundedReceiver<ConnectionEvent>,
    ) {
        let forward = self.messages_tx.clone();
        rt::spawn(async move {
            while let Some(event) = events.recv().await {
                if forward
                    .send(PoolMessage(Message::Connection {
                        connection_key: connection_key.clone(),
                        generation,
                        event,
                    }))
                    .is_err()
                {
                    return;
                }
            }
            let _ = forward.send(PoolMessage(Message::ConnectionEnded {
                connection_key,
                generation,
            }));
        });
    }

    /// The members whose media lives on `key`, per the latest snapshot.
    fn members_on_key(&self, key: &str) -> HashSet<String> {
        self.members_snapshot
            .iter()
            .filter(|member| {
                select_transport(&self.transports, member)
                    .is_some_and(|(_, member_key)| member_key == key)
            })
            .map(|member| member.member_id.clone())
            .collect()
    }

    fn handle_connection_event(
        &mut self,
        connection_key: &str,
        generation: u64,
        event: ConnectionEvent,
    ) {
        match event {
            ConnectionEvent::RemoteJoined { identity } => {
                if !self.identity_map.contains_key(&identity) {
                    // Either their membership is still propagating (buffered
                    // media will flush when it lands) or the participant does
                    // not belong to this session. Diagnostics only.
                    log::debug!(
                        "remote participant {identity} on {connection_key} has no known membership"
                    );
                    self.out.push(PoolEvent::UnknownParticipant { identity });
                }
            }
            ConnectionEvent::RemoteLeft { .. } => {
                // Membership is the truth; a transport-level leave on its own
                // changes nothing (tracks get their own events).
            }
            ConnectionEvent::TrackAdded {
                identity,
                kind,
                track,
            } => {
                let pooled = PooledTrack {
                    connection_key: connection_key.to_owned(),
                    generation,
                    track,
                    encryption_state: None,
                };
                match self.identity_map.get(&identity).cloned() {
                    Some(member_id) => self.add_track(&member_id, kind, pooled),
                    None => {
                        log::debug!(
                            "buffering {kind:?} track of unknown identity {identity} on {connection_key}"
                        );
                        self.pending_tracks
                            .entry(identity)
                            .or_default()
                            .push((kind, pooled));
                    }
                }
            }
            ConnectionEvent::TrackRemoved { identity, kind } => {
                match self.identity_map.get(&identity).cloned() {
                    Some(member_id) => {
                        self.remove_track(&member_id, kind, connection_key, generation);
                    }
                    None => {
                        if let Some(pending) = self.pending_tracks.get_mut(&identity) {
                            pending.retain(|(pending_kind, pooled)| {
                                *pending_kind != kind
                                    || pooled.connection_key != connection_key
                                    || pooled.generation != generation
                            });
                        }
                    }
                }
            }
            ConnectionEvent::TrackMuted { identity, kind } => self.muted(&identity, kind, true),
            ConnectionEvent::TrackUnmuted { identity, kind } => self.muted(&identity, kind, false),
            ConnectionEvent::ActiveSpeakers { speakers } => {
                let speakers = speakers
                    .into_iter()
                    .filter_map(|speaker| {
                        self.identity_map
                            .get(&speaker.identity)
                            .map(|member_id| ActiveSpeaker {
                                member_id: member_id.clone(),
                                level: speaker.level,
                            })
                    })
                    .collect();
                self.out.push(PoolEvent::ActiveSpeakers { speakers });
            }
            ConnectionEvent::EncryptionStateChanged { identity, state } => {
                // The event is participant-level, not proof of a particular
                // cryptor. Bind it only to receivers already owned by this
                // current connection incarnation, including parked receivers.
                if let Some(member_id) = self.identity_map.get(&identity) {
                    let mut tracks = self.tracks.lock().expect("track map mutex poisoned");
                    for ((track_member, _), pooled) in tracks.iter_mut() {
                        if track_member == member_id
                            && pooled.connection_key == connection_key
                            && pooled.generation == generation
                        {
                            pooled.encryption_state = Some(state);
                        }
                    }
                }
                if let Some(pending) = self.pending_tracks.get_mut(&identity) {
                    for (_, pooled) in pending {
                        if pooled.connection_key == connection_key
                            && pooled.generation == generation
                        {
                            pooled.encryption_state = Some(state);
                        }
                    }
                }
                match self.identity_map.get(&identity).cloned() {
                    Some(member_id) => self
                        .out
                        .push(PoolEvent::EncryptionStateChanged { member_id, state }),
                    None => {
                        // No membership to attribute it to; `UnknownParticipant`
                        // already covers that case on its own.
                        log::debug!(
                            "encryption state {state:?} for unmapped identity {identity} on {connection_key}"
                        );
                    }
                }
            }
            ConnectionEvent::Reconnecting => self.mark_degraded(connection_key),
            ConnectionEvent::Reconnected => {
                self.clear_degraded(connection_key);
                // Subscription settings are server-side connection state; a
                // resumed connection may have lost them.
                self.reapply_connection_constraints(connection_key);
            }
            ConnectionEvent::Closed { message } => {
                let generation = self
                    .pool
                    .get(connection_key)
                    .map(|entry| entry.generation)
                    .unwrap_or_default();
                self.connection_down(connection_key, generation, &message);
            }
        }
    }

    fn muted(&mut self, identity: &str, kind: MediaStreamKind, muted: bool) {
        if let Some(member_id) = self.identity_map.get(identity) {
            self.out.push(PoolEvent::StreamMuted {
                member_id: member_id.clone(),
                kind,
                muted,
            });
        }
    }

    fn mark_degraded(&mut self, key: &str) {
        let was_clear = self.degraded_keys.is_empty();
        if self.degraded_keys.insert(key.to_owned()) && was_clear {
            self.out.push(PoolEvent::Degraded(true));
        }
    }

    fn clear_degraded(&mut self, key: &str) {
        if self.degraded_keys.remove(key) && self.degraded_keys.is_empty() {
            self.out.push(PoolEvent::Degraded(false));
        }
    }
}

/// First backend (in preference order) that can serve any of the member's
/// published transports, with the resulting connection key.
fn select_transport(
    transports: &[Arc<dyn MediaTransport>],
    member: &JoinedMembership,
) -> Option<(Arc<dyn MediaTransport>, String)> {
    for backend in transports {
        for transport in &member.transports {
            if let Some(key) = backend.connection_key(transport) {
                return Some((backend.clone(), key));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests;

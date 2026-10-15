//! Bounded, passive incoming-call discovery. No room/event fetch, join, key
//! verification or capture occurs here. Admission and Answer retain the actual
//! SDK receipt and current trust checks in IncomingOffer.

use super::{
    controller::{
        AcceptedOffer, ControllerFailure, IncomingController, InvalidationHandle, OfferPresentation,
    },
    membership_envelope,
};
use crate::voice_spaces::{IncomingPolicyRegistration, VoiceSpaces};
use futures_util::StreamExt;
use matrix_sdk::{
    Client, Room, RoomState,
    deserialized_responses::VerificationState,
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedEventId, OwnedRoomId,
        events::{AnyGlobalAccountDataEvent, AnySyncStateEvent},
    },
};
use morpheus_matrix::{AccountManager, AccountSelection, SyncBackend, SyncStatus};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    runtime::Handle,
    sync::{Notify, mpsc, oneshot, watch},
    task::JoinHandle,
    time,
};

#[cfg(test)]
mod diagnostics;
#[cfg(test)]
use diagnostics::{Counters, Diagnostic, Snapshot};

// Omitted entirely from non-test builds; arguments are never evaluated there.
macro_rules! diagnostic {
    ($gate:expr, $value:ident) => {{
        #[cfg(test)]
        $gate.diagnostics.record(Diagnostic::$value);
    }};
}

const COMMANDS: usize = 16;
const CANDIDATES: usize = 64;
const ROOMS: usize = 128;
const MEMBERS: usize = 512;
const RECEIPTS: usize = 128;
const KEY_BYTES: usize = 64;
const ROOM_BYTES: usize = 256 * 1024;
const CACHE_INTERVAL: Duration = Duration::from_secs(1);
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);

struct State {
    epoch: u64,
    stopped: bool,
    selected: Option<AccountSelection>,
    interests: BTreeMap<String, BTreeSet<String>>,
}
struct Gate {
    #[cfg(test)]
    diagnostics: Counters,
    state: Mutex<State>,
    invalidate: InvalidationHandle,
    offers: watch::Sender<Vec<OfferPresentation>>,
    wake: Notify,
}
impl Gate {
    fn invalidate(&self) {
        diagnostic!(self, GateInvalidation);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.invalidate.invalidate();
        if let Some(next) = state.epoch.checked_add(1) {
            state.epoch = next;
        } else {
            state.stopped = true;
            self.invalidate.shutdown();
        }
        self.offers.send_replace(Vec::new());
        drop(state);
        self.wake.notify_one();
    }
    fn select(&self, selection: Option<AccountSelection>) {
        self.invalidate();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.selected = selection;
        state.interests.clear();
    }
    fn interest(&self, room: &str, callers: BTreeSet<String>) {
        if let Ok(mut state) = self.state.lock() {
            if callers.is_empty() {
                state.interests.remove(room);
            } else if state.interests.contains_key(room) || state.interests.len() < ROOMS {
                state.interests.insert(room.to_owned(), callers);
            }
        }
    }
    fn room_changed(&self, room: &str) {
        let interested = self
            .state
            .lock()
            .map_or(true, |state| state.interests.contains_key(room));
        if interested {
            self.invalidate();
        }
    }
    fn interested_user(&self, user: &str, own: &str) -> bool {
        user == own
            || self.state.lock().map_or(true, |state| {
                state
                    .interests
                    .values()
                    .any(|callers| callers.contains(user))
            })
    }
    fn shutdown(&self) {
        diagnostic!(self, GateShutdown);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.stopped = true;
        self.invalidate.shutdown();
        self.offers.send_replace(Vec::new());
        drop(state);
        self.wake.notify_one();
    }
    fn current(&self) -> Option<u64> {
        let state = self.state.lock().ok()?;
        (!state.stopped
            && state
                .selected
                .as_ref()
                .is_some_and(AccountSelection::is_current))
        .then_some(state.epoch)
    }
    fn stopped(&self) -> bool {
        self.state.lock().map_or(true, |state| state.stopped)
    }
    fn publish(&self, epoch: u64, offers: Vec<OfferPresentation>) {
        let Ok(state) = self.state.lock() else {
            return;
        };
        if state.stopped
            || state.epoch != epoch
            || state.selected.as_ref().is_none_or(|s| !s.is_current())
        {
            diagnostic!(self, PublicationRejected);
            return;
        }
        if offers.iter().any(|offer| {
            Some(offer.account_id.as_str())
                != state.selected.as_ref().map(AccountSelection::account_id)
        }) {
            return;
        }
        if !offers.is_empty() {
            diagnostic!(self, Publication);
        }
        self.offers.send_replace(offers);
    }
    fn snapshot(&self) -> Vec<OfferPresentation> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        if state.stopped || state.selected.as_ref().is_none_or(|s| !s.is_current()) {
            return Vec::new();
        }
        self.offers.borrow().clone()
    }
}

enum Command {
    Answer {
        key: String,
        reply: oneshot::Sender<Result<AcceptedOffer, ControllerFailure>>,
    },
    Dismiss {
        key: String,
    },
}
#[derive(Clone)]
pub(crate) struct IncomingHandle {
    commands: mpsc::Sender<Command>,
    gate: Arc<Gate>,
}
impl IncomingHandle {
    #[cfg(test)]
    pub(crate) fn diagnostics(&self) -> Snapshot {
        self.gate.diagnostics.snapshot()
    }
    pub(crate) fn snapshot(&self) -> Vec<OfferPresentation> {
        self.gate.snapshot()
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<Vec<OfferPresentation>> {
        self.gate.offers.subscribe()
    }
    pub(crate) async fn answer(&self, key: String) -> Result<AcceptedOffer, ControllerFailure> {
        require_key(&key)?;
        if self.gate.current().is_none() {
            return Err(ControllerFailure::Unavailable);
        }
        let (reply, result) = oneshot::channel();
        self.commands
            .try_send(Command::Answer { key, reply })
            .map_err(|_| ControllerFailure::Capacity)?;
        result.await.map_err(|_| ControllerFailure::Unavailable)?
    }
    pub(crate) fn dismiss(&self, key: String) -> Result<(), ControllerFailure> {
        require_key(&key)?;
        if self.gate.current().is_none() {
            return Err(ControllerFailure::Unavailable);
        }
        self.commands
            .try_send(Command::Dismiss { key })
            .map_err(|_| ControllerFailure::Capacity)
    }
}
fn require_key(key: &str) -> Result<(), ControllerFailure> {
    if key.is_empty() || key.len() > KEY_BYTES || key.chars().any(char::is_control) {
        Err(ControllerFailure::Stale)
    } else {
        Ok(())
    }
}

pub(crate) struct IncomingWorker {
    handle: IncomingHandle,
    task: Option<JoinHandle<()>>,
    _policy: Option<IncomingPolicyRegistration>,
}
impl IncomingWorker {
    pub(crate) fn start(
        runtime: &Handle,
        manager: Arc<AccountManager>,
        preferences: Arc<VoiceSpaces>,
    ) -> Self {
        let controller = IncomingController::default();
        let (offers, _) = watch::channel(Vec::new());
        let gate = Arc::new(Gate {
            #[cfg(test)]
            diagnostics: Counters::default(),
            state: Mutex::new(State {
                epoch: 1,
                stopped: false,
                selected: None,
                interests: BTreeMap::new(),
            }),
            invalidate: controller.invalidation_handle(),
            offers,
            wake: Notify::new(),
        });
        let (commands, receiver) = mpsc::channel(COMMANDS);
        let handle = IncomingHandle {
            commands,
            gate: gate.clone(),
        };
        let changed = gate.clone();
        let policy =
            preferences.watch_incoming_invalidation(Arc::new(move || changed.invalidate()));
        let task = if policy.is_some() {
            Some(runtime.spawn(run(controller, receiver, gate, manager, preferences)))
        } else {
            gate.shutdown();
            None
        };
        Self {
            handle,
            task,
            _policy: policy,
        }
    }
    pub(crate) fn handle(&self) -> IncomingHandle {
        self.handle.clone()
    }
    pub(crate) fn shutdown(&self) {
        self.handle.gate.shutdown();
    }
    pub(crate) async fn finish(&mut self) {
        self.shutdown();
        if let Some(mut task) = self.task.take() {
            if time::timeout(Duration::from_secs(2), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }
}
impl Drop for IncomingWorker {
    fn drop(&mut self) {
        self.shutdown();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// Unexpected actor exit must clear published rows and revoke authority even
// while UI handles still retain the sender/snapshot channel.
struct ActorLifetime(Arc<Gate>);
impl Drop for ActorLifetime {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}

struct Tasks(Vec<JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
struct Handlers {
    client: Client,
    handles: Vec<EventHandlerHandle>,
}
impl Drop for Handlers {
    fn drop(&mut self) {
        for handle in self.handles.drain(..) {
            self.client.remove_event_handler(handle);
        }
    }
}
struct Scope {
    selection: AccountSelection,
    client: Client,
    rooms: BTreeMap<OwnedRoomId, JoinHandle<()>>,
    healthy: Arc<AtomicBool>,
    _tasks: Tasks,
    _handlers: Handlers,
}
impl Drop for Scope {
    fn drop(&mut self) {
        for task in self.rooms.values() {
            task.abort();
        }
    }
}
struct Candidate {
    epoch: u64,
    room: OwnedRoomId,
    event: OwnedEventId,
}

async fn run(
    mut controller: IncomingController,
    mut commands: mpsc::Receiver<Command>,
    gate: Arc<Gate>,
    manager: Arc<AccountManager>,
    preferences: Arc<VoiceSpaces>,
) {
    let _lifetime = ActorLifetime(gate.clone());
    let (candidates, mut incoming) = mpsc::channel(CANDIDATES);
    let _scope_watch = Tasks(vec![tokio::spawn(watch_scope(
        manager.clone(),
        preferences.clone(),
        gate.clone(),
    ))]);
    let mut scope: Option<Scope> = None;
    let mut tick = time::interval(CACHE_INTERVAL);
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        if gate.stopped() {
            break;
        }
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(command) => dispatch(command, &mut controller, &gate),
                None => break,
            },
            _ = gate.wake.notified() => { controller.reap(); publish(&mut controller, &gate); },
            _ = tick.tick() => {
                let current = manager.subscribe_active_account().borrow().clone();
                let sync_ok = sticky_running(&manager.subscribe_sync_status().borrow());
                let selection = current.as_ref().and_then(|account| manager.account_selection(&account.id));
                let usable = sync_ok && selection.as_ref().is_some_and(AccountSelection::is_current);
                let keep = usable && scope.as_ref().is_some_and(|scope| scope.selection.is_current() && scope.healthy.load(Ordering::Acquire) && scope._tasks.0.iter().all(|task| !task.is_finished())
                    && selection.as_ref().is_some_and(|selection| selection.account_id() == scope.selection.account_id()));
                if !keep {
                    if !usable { diagnostic!(gate, ScopeUnavailable); }
                    scope = None;
                    controller.select(None);
                    gate.select(None);
                    // A->B->A cannot resurrect any queued old candidate.
                    while incoming.try_recv().is_ok() {}
                    if usable && let Some(selection) = selection {
                        let prepared = time::timeout(SETUP_TIMEOUT, prepare_scope(&manager, selection.clone(), gate.clone())).await;
                        if let Ok(Some(prepared)) = prepared {
                            if selection.is_current() && !gate.stopped() {
                                controller.select(Some(selection.clone()));
                                gate.select(Some(selection));
                                scope = Some(prepared);
                            }
                        } else { diagnostic!(gate, ScopeSetupFailed); }
                    }
                }
                if let Some(scope) = scope.as_mut() { discover_rooms(scope, &gate, &candidates); }
                controller.reap(); publish(&mut controller, &gate);
            },
            candidate = incoming.recv() => {
                let Some(candidate) = candidate else { break; };
                if gate.current() != Some(candidate.epoch) { diagnostic!(gate, CandidateStaleEpoch); continue; }
                diagnostic!(gate, AdmissionStarted);
                enum Outcome { Command(Option<Command>), Changed, Finished }
                let outcome = {
                    let receive = controller.receive(&manager, &preferences, &candidate.room, &candidate.event);
                    tokio::pin!(receive);
                    tokio::select! {
                        biased;
                        command = commands.recv() => { diagnostic!(gate, CandidateInterrupted); Outcome::Command(command) },
                        _ = gate.wake.notified() => { diagnostic!(gate, CandidateInvalidated); Outcome::Changed },
                        _result = &mut receive => {
                            #[cfg(test)]
                            gate.diagnostics.result(&_result);
                            Outcome::Finished
                        },
                    }
                };
                match outcome {
                    Outcome::Command(Some(command)) => dispatch(command, &mut controller, &gate),
                    Outcome::Command(None) => break,
                    Outcome::Changed | Outcome::Finished => { controller.reap(); publish(&mut controller, &gate); },
                }
            },
        }
    }
    gate.shutdown();
    controller.shutdown();
    drop(scope);
}
fn sticky_running(status: &SyncStatus) -> bool {
    matches!(
        status,
        SyncStatus::Running {
            backend: SyncBackend::SlidingSticky
        }
    )
}
fn publish(controller: &mut IncomingController, gate: &Gate) {
    if let Some(epoch) = gate.current() {
        gate.publish(epoch, controller.presentations());
    }
}
fn dispatch(command: Command, controller: &mut IncomingController, gate: &Gate) {
    // Re-read current actor-owned offers at command execution, never parse text
    // as an action token or select by list position/current conversation.
    let offers = if gate.current().is_some() {
        controller.presentations()
    } else {
        Vec::new()
    };
    match command {
        Command::Answer { key, reply } => {
            if !reply.is_closed() {
                let result = lookup(&offers, &key)
                    .ok_or(ControllerFailure::Stale)
                    .and_then(|token| controller.accept(token));
                let _ = reply.send(result); // Dropped receiver drops/revokes AcceptedOffer.
            }
        }
        Command::Dismiss { key } => {
            if let Some(token) = lookup(&offers, &key) {
                let _ = controller.dismiss(token);
            }
        }
    }
    publish(controller, gate);
}
fn lookup(offers: &[OfferPresentation], key: &str) -> Option<super::controller::ActionToken> {
    let keys: Vec<_> = offers.iter().map(|offer| offer.token.ui_key()).collect();
    exact_index(keys.iter().map(String::as_str), key).map(|index| offers[index].token)
}
fn exact_index<'a>(keys: impl IntoIterator<Item = &'a str>, target: &str) -> Option<usize> {
    let mut matches = keys
        .into_iter()
        .enumerate()
        .filter(|(_, key)| *key == target);
    let first = matches.next()?.0;
    matches.next().is_none().then_some(first)
}

async fn watch_scope(manager: Arc<AccountManager>, preferences: Arc<VoiceSpaces>, gate: Arc<Gate>) {
    let _lifetime = ActorLifetime(gate.clone());
    let mut active = manager.subscribe_active_account();
    let mut sync = manager.subscribe_sync_status();
    let mut policy = preferences.subscribe_incoming_policy();
    loop {
        let selection = active
            .borrow()
            .as_ref()
            .and_then(|account| manager.account_selection(&account.id));
        tokio::select! {
            changed = active.changed() => { gate.invalidate(); if changed.is_err() { gate.shutdown(); break; } },
            changed = sync.changed() => { gate.invalidate(); if changed.is_err() { gate.shutdown(); break; } },
            changed = policy.changed() => { gate.invalidate(); if changed.is_err() { gate.shutdown(); break; } },
            _ = async { match selection { Some(selection) => selection.revoked().await, None => std::future::pending().await } } => {
                gate.invalidate();
                // Active-account publication follows revocation. Do not spin on
                // the same revoked token while its sync is being stopped.
                if active.changed().await.is_err() { gate.shutdown(); break; }
            },
        }
        if gate.stopped() {
            break;
        }
    }
}

async fn prepare_scope(
    manager: &AccountManager,
    selection: AccountSelection,
    gate: Arc<Gate>,
) -> Option<Scope> {
    let client = manager.active_client().await.ok()?;
    if !selection.is_current() {
        return None;
    }
    let devices = client.encryption().devices_stream().await.ok()?;
    let identities = client.encryption().user_identities_stream().await.ok()?;
    let mut handles = Vec::new();
    let changed = gate.clone();
    handles.push(
        client.add_event_handler(move |event: AnyGlobalAccountDataEvent| {
            if matches!(
                event.event_type().to_string().as_str(),
                "m.push_rules" | "m.ignored_user_list" | "m.direct"
            ) {
                changed.invalidate();
            }
            async {}
        }),
    );
    let changed = gate.clone();
    handles.push(
        client.add_event_handler(move |event: AnySyncStateEvent, room: Room| {
            // Matrix room state changes are independent of sticky own-membership
            // arrival. Unknown state is conservative; no raw state is published.
            let _ = event;
            changed.room_changed(room.room_id().as_str());
            async {}
        }),
    );
    let _handlers = Handlers {
        client: client.clone(),
        handles,
    };
    let own_user = client.user_id()?.to_string();
    let mut verification = client.encryption().verification_state();
    // The SDK uses subscribe_reset(): consume its current baseline before any
    // room intake can become usable. Subsequent version changes (including
    // coalesced A -> B -> A transitions) still invalidate pending authority.
    let _baseline = verification.next_now();
    let healthy = Arc::new(AtomicBool::new(true));
    let healthy_crypto = healthy.clone();
    let selected_crypto = selection.clone();
    let crypto_gate = gate.clone();
    let crypto = tokio::spawn(async move {
        tokio::pin!(devices);
        tokio::pin!(identities);
        loop {
            tokio::select! {
                item = devices.next() => {
                    if !selected_crypto.is_current() { break; }
                    let Some(update) = item else { crypto_gate.invalidate(); healthy_crypto.store(false, Ordering::Release); break; };
                    if update.new.keys().chain(update.changed.keys()).any(|user| crypto_gate.interested_user(user.as_str(), &own_user)) { crypto_gate.invalidate(); }
                },
                item = identities.next() => {
                    if !selected_crypto.is_current() { break; }
                    let Some(update) = item else { crypto_gate.invalidate(); healthy_crypto.store(false, Ordering::Release); break; };
                    if update.new.keys().chain(update.changed.keys()).any(|user| crypto_gate.interested_user(user.as_str(), &own_user)) { crypto_gate.invalidate(); }
                },
                item = verification.next() => {
                    if !selected_crypto.is_current() { break; }
                    crypto_gate.invalidate();
                    if item.is_none() { healthy_crypto.store(false, Ordering::Release); break; }
                },
            }
        }
    });
    let selection_gate = gate;
    let selected = selection.clone();
    let revoke = tokio::spawn(async move {
        selected.revoked().await;
        selection_gate.invalidate();
    });
    Some(Scope {
        selection,
        client,
        rooms: BTreeMap::new(),
        healthy,
        _tasks: Tasks(vec![crypto, revoke]),
        _handlers,
    })
}
fn discover_rooms(scope: &mut Scope, gate: &Arc<Gate>, candidates: &mpsc::Sender<Candidate>) {
    let mut joined: Vec<_> = scope
        .client
        .rooms()
        .into_iter()
        .filter(|room| room.state() == RoomState::Joined)
        .collect();
    joined.sort_by(|a, b| a.room_id().cmp(b.room_id()));
    joined.truncate(ROOMS);
    let ids: BTreeSet<_> = joined
        .iter()
        .map(|room| room.room_id().to_owned())
        .collect();
    scope.rooms.retain(|id, task| {
        if ids.contains(id) && !task.is_finished() {
            true
        } else {
            task.abort();
            gate.room_changed(id.as_str());
            gate.interest(id.as_str(), BTreeSet::new());
            false
        }
    });
    for room in joined {
        if scope.rooms.contains_key(room.room_id()) {
            continue;
        }
        let id = room.room_id().to_owned();
        let task = tokio::spawn(watch_room(
            room,
            scope.selection.clone(),
            gate.clone(),
            candidates.clone(),
        ));
        scope.rooms.insert(id, task);
    }
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct MemberStamp {
    sender: String,
    device: String,
    logical: String,
    mode: &'static str,
}
#[derive(Default)]
struct CachedRoom {
    generation: Option<u64>,
    receipts: BTreeMap<String, [u8; 32]>,
    members: Option<BTreeSet<MemberStamp>>,
    seen: BTreeSet<String>,
    unavailable: bool,
}
impl CachedRoom {
    fn reject(&mut self, room: &str, gate: &Gate) {
        if !self.unavailable && !self.receipts.is_empty() {
            gate.room_changed(room);
        }
        self.unavailable = true;
        gate.interest(room, BTreeSet::new());
    }
    fn changed(
        &mut self,
        generation: u64,
        receipts: BTreeMap<String, [u8; 32]>,
        members: BTreeSet<MemberStamp>,
    ) -> bool {
        let changed = self
            .generation
            .is_some_and(|previous| previous != generation)
            || self
                .receipts
                .iter()
                .any(|(id, raw)| receipts.get(id) != Some(raw))
            || self.members.as_ref().is_some_and(|old| old != &members);
        self.generation = Some(generation);
        self.unavailable = false;
        self.seen.retain(|id| receipts.contains_key(id));
        self.receipts = receipts;
        self.members = Some(members);
        changed
    }
}
async fn watch_room(
    room: Room,
    selection: AccountSelection,
    gate: Arc<Gate>,
    candidates: mpsc::Sender<Candidate>,
) {
    let sticky = room.sticky_events();
    let mut receipts = sticky.subscribe_keyless();
    let mut memberships = sticky.subscribe();
    let mut cached = CachedRoom::default();
    let mut tick = time::interval(CACHE_INTERVAL);
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    loop {
        if !selection.is_current() || gate.stopped() || room.state() != RoomState::Joined {
            gate.room_changed(room.room_id().as_str());
            gate.interest(room.room_id().as_str(), BTreeSet::new());
            break;
        }
        reconcile_room(&room, &gate, &candidates, &mut cached);
        tokio::select! {
            _ = selection.revoked() => { gate.invalidate(); break; },
            update = receipts.recv() => { if update.is_err() { cached.reject(room.room_id().as_str(), &gate); if matches!(update, Err(tokio::sync::broadcast::error::RecvError::Closed)) { break; } } },
            update = memberships.recv() => { if update.is_err() { cached.reject(room.room_id().as_str(), &gate); if matches!(update, Err(tokio::sync::broadcast::error::RecvError::Closed)) { break; } } },
            _ = tick.tick() => {},
        }
    }
}
fn reconcile_room(
    room: &Room,
    gate: &Gate,
    candidates: &mpsc::Sender<Candidate>,
    cached: &mut CachedRoom,
) {
    let retained = room.sticky_events().keyless_live();
    if retained.invalidated || retained.events.len() > RECEIPTS {
        if retained.invalidated {
            diagnostic!(gate, CacheInvalidated);
        } else {
            diagnostic!(gate, ReceiptCapacity);
        }
        cached.reject(room.room_id().as_str(), gate);
        return;
    }
    let own_user = room.client().user_id().map(ToString::to_string);
    let own_device = room.client().device_id().map(ToString::to_string);
    let Some((own_user, own_device)) = own_user.zip(own_device) else {
        diagnostic!(gate, MissingIdentity);
        cached.reject(room.room_id().as_str(), gate);
        return;
    };
    let mut raw_receipts = BTreeMap::new();
    let mut callers = BTreeSet::new();
    let mut bytes = 0usize;
    for event in &retained.events {
        let raw = event.raw().json().get();
        bytes = bytes.saturating_add(raw.len());
        if raw.len() > super::MAX_BYTES || bytes > ROOM_BYTES {
            diagnostic!(gate, ReceiptOversized);
            cached.reject(room.room_id().as_str(), gate);
            return;
        }
        let Some(encryption) = event.encryption_info() else {
            diagnostic!(gate, ReceiptUnverified);
            continue;
        };
        if encryption.verification_state != VerificationState::Verified
            || encryption.sender != event.sender
        {
            diagnostic!(gate, ReceiptUnverified);
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            diagnostic!(gate, ReceiptMalformed);
            continue;
        };
        let Ok(ring) =
            morpheus_rtc::ringing::RingNotification::parse(&value, room.room_id().as_str())
        else {
            diagnostic!(gate, ReceiptMalformed);
            continue;
        };
        if ring.sender() == own_user
            || ring.sender() != encryption.sender.as_str()
            || ring.event_id() != event.event_id.as_str()
        {
            diagnostic!(gate, ReceiptIdentity);
            continue;
        }
        callers.insert(ring.sender().to_owned());
        raw_receipts.insert(
            event.event_id.to_string(),
            Sha256::digest(raw.as_bytes()).into(),
        );
    }
    if raw_receipts.is_empty() {
        diagnostic!(gate, NoVerifiedReceipts);
        if !cached.unavailable
            && (!cached.receipts.is_empty()
                || cached
                    .generation
                    .is_some_and(|old| old != retained.generation))
        {
            gate.room_changed(room.room_id().as_str());
        }
        gate.interest(room.room_id().as_str(), BTreeSet::new());
        *cached = CachedRoom {
            generation: Some(retained.generation),
            ..Default::default()
        };
        return;
    }
    let mut members = BTreeSet::new();
    let live = room.sticky_events().live();
    if live.len() > MEMBERS {
        diagnostic!(gate, MemberCapacity);
        cached.reject(room.room_id().as_str(), gate);
        return;
    }
    for member in live {
        // Unrelated participants do not need to be locally verified for a
        // verified caller's offer. Track its callers and our own user only.
        if member.key.sender.as_str() != own_user && !callers.contains(member.key.sender.as_str()) {
            continue;
        }
        let raw = member.raw().json().get();
        bytes = bytes.saturating_add(raw.len());
        if bytes > ROOM_BYTES {
            diagnostic!(gate, MemberOversized);
            cached.reject(room.room_id().as_str(), gate);
            return;
        }
        if raw.len() > super::MAX_BYTES {
            diagnostic!(gate, MemberOversized);
            cached.reject(room.room_id().as_str(), gate);
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            diagnostic!(gate, MemberMalformed);
            cached.reject(room.room_id().as_str(), gate);
            return;
        };
        if !matches!(
            value.get("type").and_then(serde_json::Value::as_str),
            Some("m.rtc.member" | "org.matrix.msc4143.rtc.member")
        ) {
            continue;
        }
        let Some(encryption) = member.encryption_info() else {
            diagnostic!(gate, MemberUnverified);
            cached.reject(room.room_id().as_str(), gate);
            return;
        };
        let Some(device) = encryption.sender_device.as_deref() else {
            diagnostic!(gate, MemberUnverified);
            cached.reject(room.room_id().as_str(), gate);
            return;
        };
        if encryption.verification_state != VerificationState::Verified
            || encryption.sender != member.key.sender
        {
            diagnostic!(gate, MemberUnverified);
            cached.reject(room.room_id().as_str(), gate);
            return;
        }
        // A verified leave stays in the sticky cache until its lifetime ends.
        // It is the absence of a participant, not a malformed membership: our
        // own device's recent departure must not reject every later offer in
        // this room, and a departed caller simply has no live membership below.
        if departed_membership(&value) {
            diagnostic!(gate, MemberDeparted);
            continue;
        }
        let Ok(parsed) = membership_envelope(
            &value,
            room.room_id().as_str(),
            member.event_id.as_str(),
            encryption.sender.as_str(),
            device.as_str(),
        ) else {
            diagnostic!(gate, MemberMalformed);
            cached.reject(room.room_id().as_str(), gate);
            return;
        };
        if parsed.logical_id != member.key.sticky_key {
            diagnostic!(gate, MemberMismatch);
            cached.reject(room.room_id().as_str(), gate);
            return;
        }
        // Expected local arrival/refresh does not itself revoke Answer. This is
        // only an invalidation hint exception: prejoin forbids an existing own
        // participation, and postjoin still proves the exact own generation.
        if parsed.user == own_user && parsed.device == own_device {
            continue;
        }
        members.insert(MemberStamp {
            sender: parsed.user,
            device: parsed.device,
            logical: parsed.logical_id,
            mode: super::wire_slot(parsed.mode),
        });
    }
    // Do not spend the receipt's single admission attempt before its caller's
    // live membership is visible in the independently arriving sticky cache.
    if callers
        .iter()
        .any(|caller| !members.iter().any(|member| &member.sender == caller))
    {
        diagnostic!(gate, CallerMembershipMissing);
        cached.reject(room.room_id().as_str(), gate);
        return;
    }
    gate.interest(room.room_id().as_str(), callers);
    if cached.changed(retained.generation, raw_receipts, members) {
        diagnostic!(gate, CacheChanged);
        gate.room_changed(room.room_id().as_str());
    }
    for event in retained.events {
        if !cached.receipts.contains_key(event.event_id.as_str())
            || cached.seen.contains(event.event_id.as_str())
        {
            if cached.seen.contains(event.event_id.as_str()) {
                diagnostic!(gate, SeenRetained);
            }
            continue;
        }
        // The actual admission boundary determines encrypted trust/target/type.
        // Malformed/untrusted receipts cannot be turned into UI actions here.
        let Some(epoch) = gate.current() else {
            diagnostic!(gate, GateUnavailable);
            return;
        };
        if candidates
            .try_send(Candidate {
                epoch,
                room: room.room_id().to_owned(),
                event: event.event_id.clone(),
            })
            .is_err()
        {
            diagnostic!(gate, CandidateQueueFull);
            cached.reject(room.room_id().as_str(), gate);
            return;
        }
        diagnostic!(gate, CandidateQueued);
        cached.seen.insert(event.event_id.to_string());
    }
}

/// MSC4143 marks a departed participant with `member.membership: "leave"`;
/// the entry carries no application or transports. Admission never accepts
/// one as a referenced membership; discovery treats it as absent.
fn departed_membership(envelope: &serde_json::Value) -> bool {
    envelope
        .pointer("/content/member/membership")
        .and_then(serde_json::Value::as_str)
        == Some("leave")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gate() -> (IncomingController, Arc<Gate>) {
        let controller = IncomingController::default();
        let (offers, _) = watch::channel(Vec::new());
        let gate = Arc::new(Gate {
            #[cfg(test)]
            diagnostics: Counters::default(),
            state: Mutex::new(State {
                epoch: 1,
                stopped: false,
                selected: None,
                interests: BTreeMap::new(),
            }),
            invalidate: controller.invalidation_handle(),
            offers,
            wake: Notify::new(),
        });
        (controller, gate)
    }
    fn receipt() -> BTreeMap<String, [u8; 32]> {
        BTreeMap::from([("event-one".into(), [7; 32])])
    }
    fn member() -> MemberStamp {
        MemberStamp {
            sender: "caller".into(),
            device: "device-one".into(),
            logical: "join-one".into(),
            mode: "m.call#room",
        }
    }
    fn members() -> BTreeSet<MemberStamp> {
        BTreeSet::from([member()])
    }

    fn sticky_member(content: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "m.rtc.member",
            "event_id": "$member",
            "sender": "@own:example.org",
            "room_id": "!room:example.org",
            "content": content,
        })
    }

    #[test]
    fn departed_memberships_are_absent_participants_not_malformed_rooms() {
        // The pinned RTC core's leave: slot, sticky key, member id/leave and a
        // reason, with no application block.
        let leave = sticky_member(json!({
            "slot_id": "m.call#room",
            "msc4354_sticky_key": "join-one",
            "member": {"id": "join-one", "membership": "leave"},
            "leave_reason": {"code": "m.leave"},
        }));
        assert!(departed_membership(&leave));
        assert!(
            membership_envelope(
                &leave,
                "!room:example.org",
                "$member",
                "@own:example.org",
                "DEVICE",
            )
            .is_err(),
            "admission must still refuse a departed referenced membership"
        );
        let join = sticky_member(json!({
            "slot_id": "m.call#room",
            "msc4354_sticky_key": "join-one",
            "member": {"id": "join-one", "membership": "join"},
            "application": {"type": "m.call"},
        }));
        assert!(!departed_membership(&join));
        assert!(
            membership_envelope(
                &join,
                "!room:example.org",
                "$member",
                "@own:example.org",
                "DEVICE",
            )
            .is_ok()
        );
        for other in [
            sticky_member(json!({"slot_id": "m.call#room", "msc4354_sticky_key": "join-one"})),
            sticky_member(json!({
                "slot_id": "m.call#room",
                "msc4354_sticky_key": "join-one",
                "member": {"id": "join-one", "membership": ["leave"]},
            })),
            sticky_member(json!({
                "slot_id": "m.call#room",
                "msc4354_sticky_key": "join-one",
                "member": {"id": "join-one", "membership": "Leave"},
            })),
            json!({"member": {"membership": "leave"}}),
        ] {
            assert!(!departed_membership(&other));
        }
    }
    #[test]
    fn text_keys_are_exact_unique_and_cannot_be_interpreted_as_rows() {
        let keys = ["incoming-10", "incoming-20"];
        assert_eq!(exact_index(keys, "incoming-20"), Some(1));
        for stale in [
            "1",
            "incoming-1",
            " incoming-10",
            "incoming-10 ",
            "incoming-30",
        ] {
            assert_eq!(exact_index(keys, stale), None);
        }
        assert_eq!(
            exact_index(["incoming-10", "incoming-10"], "incoming-10"),
            None
        );
        assert!(require_key(&"x".repeat(KEY_BYTES + 1)).is_err());
        assert!(require_key("incoming-1\n").is_err());
    }
    #[test]
    fn logical_refresh_preserves_offer_but_device_generation_or_dialect_change_revokes() {
        let mut cache = CachedRoom::default();
        assert!(!cache.changed(1, receipt(), members()));
        assert!(!cache.changed(1, receipt(), members())); // event IDs are deliberately not authorization stamps
        for kind in 0..3 {
            let mut cache = CachedRoom::default();
            cache.changed(1, receipt(), members());
            let mut altered = member();
            match kind {
                0 => altered.device = "device-two".into(),
                1 => altered.logical = "join-two".into(),
                _ => altered.mode = "m.call#ROOM",
            }
            assert!(cache.changed(1, receipt(), BTreeSet::from([altered])));
        }
    }
    #[test]
    fn receipt_removal_replacement_generation_and_caller_departure_are_observed() {
        for kind in 0..4 {
            let mut cache = CachedRoom::default();
            cache.changed(1, receipt(), members());
            let (generation, events, roster) = match kind {
                0 => (1, BTreeMap::new(), members()),
                1 => (
                    1,
                    BTreeMap::from([("event-one".into(), [8; 32])]),
                    members(),
                ),
                2 => (2, receipt(), members()),
                _ => (1, receipt(), BTreeSet::new()),
            };
            assert!(cache.changed(generation, events, roster));
        }
    }
    #[test]
    fn unrelated_room_uncertainty_does_not_repeatedly_cancel_other_offers() {
        let (_controller, gate) = gate();
        let mut cache = CachedRoom::default();
        cache.reject("unrelated-room", &gate);
        cache.reject("unrelated-room", &gate);
        assert_eq!(gate.state.lock().unwrap().epoch, 1);
        gate.interest("relevant-room", BTreeSet::from(["caller".into()]));
        cache.changed(1, receipt(), members());
        cache.reject("relevant-room", &gate);
        assert_eq!(gate.state.lock().unwrap().epoch, 2);
        cache.reject("relevant-room", &gate);
        assert_eq!(gate.state.lock().unwrap().epoch, 2);
    }
    #[test]
    fn room_and_crypto_hints_only_invalidate_relevant_interest() {
        let (_controller, gate) = gate();
        gate.interest("relevant-room", BTreeSet::from(["caller".into()]));
        gate.room_changed("different-room");
        assert_eq!(gate.state.lock().unwrap().epoch, 1);
        assert!(!gate.interested_user("unrelated-user", "own-user"));
        assert!(gate.interested_user("caller", "own-user"));
        assert!(gate.interested_user("own-user", "own-user"));
        gate.room_changed("relevant-room");
        assert_eq!(gate.state.lock().unwrap().epoch, 2);
        gate.interest("relevant-room", BTreeSet::new());
        gate.room_changed("relevant-room");
        assert_eq!(gate.state.lock().unwrap().epoch, 2);
    }
    #[test]
    fn scope_replacement_and_epoch_exhaustion_cannot_revive_old_intake() {
        let (_controller, gate) = gate();
        gate.interest("old-room", BTreeSet::from(["old-caller".into()]));
        gate.select(None);
        assert!(gate.state.lock().unwrap().interests.is_empty());
        gate.state.lock().unwrap().epoch = u64::MAX;
        gate.invalidate();
        assert!(gate.stopped());
        assert!(gate.current().is_none());
        assert!(gate.snapshot().is_empty());
    }
    #[tokio::test]
    async fn bounded_commands_drop_unhandled_answer_reply_and_wake_waiter() {
        let (send, mut receive) = mpsc::channel(1);
        let (reply, result) = oneshot::channel();
        send.try_send(Command::Answer {
            key: "incoming-1".into(),
            reply,
        })
        .unwrap_or_else(|_| panic!("first command failed"));
        assert!(
            send.try_send(Command::Dismiss {
                key: "incoming-2".into()
            })
            .is_err()
        );
        drop(receive.recv().await);
        assert!(result.await.is_err());
    }
    #[tokio::test]
    async fn dropping_task_owner_aborts_waiting_subscription() {
        struct Mark(Arc<AtomicBool>);
        impl Drop for Mark {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let marker = Mark(dropped.clone());
        let (entered, ready) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _marker = marker;
            let _ = entered.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        drop(Tasks(vec![task]));
        time::timeout(Duration::from_secs(1), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[test]
    fn unexpected_actor_exit_revokes_authority_even_when_ui_handle_survives() {
        let (_controller, gate) = gate();
        let still_owned = gate.clone();
        drop(ActorLifetime(gate));
        assert!(still_owned.stopped());
        assert!(still_owned.snapshot().is_empty());
    }

    #[test]
    fn real_actor_future_is_send_with_send_only_offer_payloads() {
        fn require_send<T: Send>(_: T) {}
        let _check: fn(
            IncomingController,
            mpsc::Receiver<Command>,
            Arc<Gate>,
            Arc<AccountManager>,
            Arc<VoiceSpaces>,
        ) = |controller, commands, gate, manager, preferences| {
            require_send(run(controller, commands, gate, manager, preferences))
        };
    }
}

//! Bounded, passive incoming-call discovery. No room/event fetch, join, key
//! verification or capture occurs here. Admission and Answer retain the actual
//! SDK receipt and current trust checks in IncomingOffer. Declines from this
//! user's other devices are retained as bounded receipts (Gate::decisions) and
//! settled by the controller.

use super::{
    IncomingOffer,
    controller::{
        AcceptedOffer, ControllerFailure, DeclineCheck, DeclineClaim, DeclineReceipt,
        IncomingController, InvalidationHandle, MAX_DECLINE_RECEIPTS, OfferPresentation,
        ScopedReceipts,
    },
    membership_envelope,
};
use crate::voice_spaces::{IncomingPolicyRegistration, VoiceSpaces};
use futures_util::{StreamExt, stream::FuturesUnordered};
use matrix_sdk::{
    Client, Room, RoomState,
    deserialized_responses::{EncryptionInfo, VerificationState},
    event_handler::EventHandlerHandle,
    ruma::{
        OwnedEventId, OwnedRoomId,
        events::{AnyGlobalAccountDataEvent, AnySyncStateEvent, rtc::decline::SyncRtcDeclineEvent},
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
    time::{Duration, Instant},
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
/// Owned decline trust checks (DeclineCheck::trust), polled by the actor.
type Checks = FuturesUnordered<
    std::pin::Pin<Box<dyn std::future::Future<Output = (DeclineCheck, bool)> + Send>>,
>;
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
    /// Real invalidation only (account, policy, room, crypto, stop): it
    /// cancels an admission in progress.
    wake: Notify,
    /// A decline receipt arrived. Kept apart from `wake` on purpose: a
    /// receipt is evidence about one call, never an invalidation, so it must
    /// not drop another offer's admission (whose ledger observation is
    /// one-time). The actor services it in its main loop and, without
    /// interrupting, during an admission (race_admission).
    decision_wake: Notify,
    /// Receipt-checked declines from this user's other devices, retained
    /// briefly in case their offer is admitted after them (the notification
    /// and its decline may arrive in one sync), together with the scope that
    /// may hold them. All admission, insertion, restoration and clearing
    /// happen under this one lock (ScopedReceipts). Lock order where both are
    /// taken: state, then decisions.
    decisions: Mutex<ScopedReceipts<DeclineReceipt, AccountSelection>>,
}
impl Gate {
    fn decisions(
        &self,
    ) -> std::sync::MutexGuard<'_, ScopedReceipts<DeclineReceipt, AccountSelection>> {
        self.decisions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
    /// Retain one receipt-checked decline, only while the gate is not stopped
    /// and its own captured selection is the current scope, and tell the
    /// actor (decisions_arrived, never the invalidation wake).
    fn retain_decision(&self, receipt: DeclineReceipt) {
        if self.decisions().insert(receipt, Instant::now()) {
            self.decisions_arrived();
        }
    }
    /// Only the receipt wake: no admission is cancelled by it.
    fn decisions_arrived(&self) {
        self.decision_wake.notify_one();
    }
    /// The scope whose own declines may be retained, set before its handlers
    /// are registered; select() with the same selection keeps them.
    fn scope_decisions(&self, scope: Option<AccountSelection>) {
        self.decisions().reset(scope);
    }
    fn take_decisions(&self) -> Vec<DeclineReceipt> {
        self.decisions().take(Instant::now())
    }
    /// Admission is rechecked under the lock: a stop or scope change that
    /// happened meanwhile keeps them out.
    fn restore_decisions(&self, earlier: Vec<DeclineReceipt>) {
        if !earlier.is_empty() {
            self.decisions().restore(earlier, Instant::now());
        }
    }
    fn invalidate(&self) {
        diagnostic!(self, GateInvalidation);
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.invalidate.invalidate();
        if let Some(next) = state.epoch.checked_add(1) {
            state.epoch = next;
        } else {
            state.stopped = true;
            self.invalidate.shutdown();
            self.decisions().stop();
        }
        self.offers.send_replace(Vec::new());
        drop(state);
        self.wake.notify_one();
    }
    fn select(&self, selection: Option<AccountSelection>) {
        self.invalidate();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // No decline received under an earlier selection survives it, and
        // only the new selection's own receipts may enter from now on.
        self.decisions().reset(selection.clone());
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
        self.decisions().stop();
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
    Decline {
        key: String,
        reply: oneshot::Sender<Result<DeclineClaim, ControllerFailure>>,
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
    /// Takes the offer for one explicit Decline; the caller makes the bounded
    /// send off the actor. A dropped reply drops and revokes the claim.
    pub(crate) async fn decline(&self, key: String) -> Result<DeclineClaim, ControllerFailure> {
        require_key(&key)?;
        if self.gate.current().is_none() {
            return Err(ControllerFailure::Unavailable);
        }
        let (reply, result) = oneshot::channel();
        self.commands
            .try_send(Command::Decline { key, reply })
            .map_err(|_| ControllerFailure::Capacity)?;
        result.await.map_err(|_| ControllerFailure::Unavailable)?
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
            decision_wake: Notify::new(),
            decisions: Mutex::new(ScopedReceipts::default()),
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
    // Decline trust checks in flight, owned by this actor and polled after the
    // commands and wake-ups (biased below, and inside race_admission while an
    // admission runs), so a waiting check never holds up Answer, Decline,
    // Dismiss or stopping, and new declines are matched and checked, and
    // finished checks applied, even during another call's admission. Bounded
    // by MAX_DECLINE_RECEIPTS.
    let mut checks: Checks = FuturesUnordered::new();
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
            _ = gate.wake.notified() => { controller.reap(); start_declines(&mut controller, &gate, &mut checks); publish(&mut controller, &gate); },
            _ = gate.decision_wake.notified() => { controller.reap(); start_declines(&mut controller, &gate, &mut checks); publish(&mut controller, &gate); },
            finished = checks.next(), if !checks.is_empty() => {
                if let Some((check, trusted)) = finished {
                    controller.finish_decline(check, trusted);
                }
                controller.reap(); start_declines(&mut controller, &gate, &mut checks); publish(&mut controller, &gate);
            },
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
                    // No old check survives its scope (their offers go too).
                    checks.clear();
                    controller.select(None);
                    gate.select(None);
                    // A->B->A cannot resurrect any queued old candidate.
                    while incoming.try_recv().is_ok() {}
                    if usable && let Some(selection) = selection {
                        // Declines may arrive while the scope's handlers are
                        // being set up; only this selection's own are kept.
                        gate.scope_decisions(Some(selection.clone()));
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
                controller.reap(); start_declines(&mut controller, &gate, &mut checks); publish(&mut controller, &gate);
            },
            candidate = incoming.recv() => {
                let Some(candidate) = candidate else { break; };
                if gate.current() != Some(candidate.epoch) { diagnostic!(gate, CandidateStaleEpoch); continue; }
                diagnostic!(gate, AdmissionStarted);
                let ticket = match controller.begin_receive(&candidate.room, &candidate.event) {
                    Ok(ticket) => ticket,
                    Err(_failure) => {
                        #[cfg(test)]
                        gate.diagnostics.result(&Err::<(), _>(_failure));
                        controller.reap(); start_declines(&mut controller, &gate, &mut checks); publish(&mut controller, &gate);
                        continue;
                    }
                };
                // The one admission future, with the controller's shared
                // ledger: it holds no controller borrow, so declines that
                // arrive meanwhile are matched and checked, and finished
                // checks applied, at once (below) while it keeps its original
                // pinned future and one-time observation. A decline for this
                // admission's own call matches no offer yet and stays
                // retained, with its original deadline, until it is admitted.
                let ledger = controller.ledger();
                let outcome = race_admission(
                    IncomingOffer::admit(&manager, &preferences, &candidate.room, &candidate.event, &*ledger),
                    &mut commands,
                    &gate,
                    &mut checks,
                    |event, checks| {
                        if let AdmissionEvent::Completed((check, trusted)) = event {
                            controller.finish_decline(check, trusted);
                        }
                        controller.reap(); start_declines(&mut controller, &gate, checks); publish(&mut controller, &gate);
                    },
                )
                .await;
                match outcome {
                    Err(Interrupt::Command(command)) => {
                        diagnostic!(gate, CandidateInterrupted);
                        match command {
                            Some(command) => dispatch(command, &mut controller, &gate),
                            None => break,
                        }
                    }
                    Err(Interrupt::Changed) => {
                        diagnostic!(gate, CandidateInvalidated);
                        controller.reap(); start_declines(&mut controller, &gate, &mut checks); publish(&mut controller, &gate);
                    }
                    Ok(admitted) => {
                        let _result = controller.finish_receive(ticket, admitted);
                        #[cfg(test)]
                        gate.diagnostics.result(&_result);
                        controller.reap(); start_declines(&mut controller, &gate, &mut checks); publish(&mut controller, &gate);
                    }
                }
            },
        }
    }
    gate.shutdown();
    controller.shutdown();
    drop(scope);
}
/// What interrupted an admission before it finished.
enum Interrupt {
    Command(Option<Command>),
    Changed,
}
/// Useful work while an admission runs, handed to one callback (so only one
/// closure ever borrows the controller): decline receipts are ready to be
/// matched (`Gate::decision_wake`), or a decline check finished.
enum AdmissionEvent<C> {
    Receipts,
    Completed(C),
}
/// Drive one admission (its original future, polled until it finishes or is
/// interrupted, never rebuilt) while servicing decline work meanwhile: each
/// receipt arrival and each finished check is handed to `on_event` (which
/// matches receipts, starts checks within the existing bounds, applies
/// completions and publishes) and the same admission future keeps going. It
/// is interrupted only by a command (first) or real invalidation
/// (`Gate::wake`: account, policy, room, crypto, stop), as before. A receipt
/// arrival is readiness only, never cancellation: it can neither drop this
/// admission nor consume its one-time ledger observation, whatever that
/// receipt later proves.
async fn race_admission<F, Fut, C>(
    receive: F,
    commands: &mut mpsc::Receiver<Command>,
    gate: &Gate,
    checks: &mut FuturesUnordered<Fut>,
    mut on_event: impl FnMut(AdmissionEvent<C>, &mut FuturesUnordered<Fut>),
) -> Result<F::Output, Interrupt>
where
    F: std::future::Future,
    Fut: std::future::Future<Output = C>,
{
    tokio::pin!(receive);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => return Err(Interrupt::Command(command)),
            _ = gate.wake.notified() => return Err(Interrupt::Changed),
            _ = gate.decision_wake.notified() => on_event(AdmissionEvent::Receipts, checks),
            done = checks.next(), if !checks.is_empty() => {
                if let Some(done) = done {
                    on_event(AdmissionEvent::Completed(done), checks);
                }
            },
            result = &mut receive => return Ok(result),
        }
    }
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
/// Begin trust checks for retained own-device declines that match exact
/// offers, before publishing. An offer not yet shown is withheld while its
/// check runs, so a decline that arrived first (even in the same sync)
/// retires it, once authenticated, before it is ever shown or rung; if the
/// check fails it is shown then. At most MAX_DECLINE_RECEIPTS checks run at
/// once; receipts beyond that, or matching no offer yet, stay retained until
/// they expire.
fn start_declines(controller: &mut IncomingController, gate: &Gate, checks: &mut Checks) {
    let room = MAX_DECLINE_RECEIPTS.saturating_sub(checks.len());
    if room == 0 {
        return;
    }
    let receipts = gate.take_decisions();
    if receipts.is_empty() {
        return;
    }
    let (started, unmatched) = controller.match_declines(receipts, room);
    for check in started {
        checks.push(Box::pin(check.trust()));
    }
    gate.restore_decisions(unmatched);
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
        Command::Decline { key, reply } => {
            if !reply.is_closed() {
                let result = lookup(&offers, &key)
                    .ok_or(ControllerFailure::Stale)
                    .and_then(|token| controller.decline(token));
                let _ = reply.send(result); // Dropped receiver drops/revokes DeclineClaim.
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
    // This scope's own identity, before any handler exists (an early return
    // below must not leave a handler registered).
    let receipt_user = client.user_id()?.to_string();
    let receipt_device = client.device_id()?.to_string();
    let mut handles = Vec::new();
    // Declines (MSC4310) as this account's sync delivers them. The SDK runs
    // event handlers only for sync responses (sync and sliding sync), never
    // for fetched, backfilled or paginated events, and passes each event's
    // own decryption metadata: an undecryptable event keeps its encrypted
    // type and never reaches this handler, a plaintext one has none, and a
    // redacted one has no original content. Only receipt-checked declines
    // from this user's other devices are retained (bounded, short-lived);
    // the actor settles them against exact offers after checking the
    // device's current trust. Nothing is fetched or sent here. The legacy
    // sender-only Room::subscribe_to_call_decline_events is not used.
    let decisions = gate.clone();
    let receipt_selection = selection.clone();
    handles.push(client.add_event_handler(
        move |event: SyncRtcDeclineEvent, room: Room, encryption: Option<EncryptionInfo>| {
            if let Some(original) = event.as_original()
                && let Some(receipt) = DeclineReceipt::from_sync(
                    &receipt_selection,
                    &receipt_user,
                    &receipt_device,
                    room.room_id().as_str(),
                    original.sender.as_str(),
                    original.content.relates_to.event_id.as_str(),
                    encryption.as_ref(),
                    Instant::now(),
                )
            {
                decisions.retain_decision(receipt);
            }
            async {}
        },
    ));
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
        // A valid leave remains in the sticky cache until its TTL expires.
        // It is absence from the joined roster, not malformed join evidence.
        // Preserve all trust/bounds checks above; malformed pseudo-leaves still
        // reach the strict parser and reject this reconciliation below.
        if super::departed_membership_envelope(
            &value,
            room.room_id().as_str(),
            member.event_id.as_str(),
            encryption.sender.as_str(),
            device.as_str(),
            &member.key.sticky_key,
        ) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
            decision_wake: Notify::new(),
            decisions: Mutex::new(ScopedReceipts::default()),
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
    /// A decline receipt for call A arriving while call B's admission is held
    /// (say, waiting on trust or membership after its one-time ledger
    /// observation) does not interrupt it: B keeps its original admission
    /// future, which then finishes. The receipt's own result is irrelevant
    /// here (one that later fails current trust has no effect on B either):
    /// only its arrival is signalled, on its own wake. Real invalidation, a
    /// stop and a command still interrupt an admission as before.
    #[tokio::test]
    async fn a_decline_receipt_never_interrupts_another_calls_admission() {
        let (_controller, gate) = gate();
        let (send, mut commands) = mpsc::channel(1);
        // No decline check is in flight in this test.
        let mut idle = FuturesUnordered::<std::future::Ready<u8>>::new();

        let (release, held) = oneshot::channel::<u8>();
        let polled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let admission = {
            let polled = polled.clone();
            async move {
                polled.fetch_add(1, Ordering::SeqCst);
                held.await.unwrap_or(0)
            }
        };
        let ready = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        {
            let seen = ready.clone();
            let race = race_admission(admission, &mut commands, &gate, &mut idle, |event, _| {
                if matches!(event, AdmissionEvent::Receipts) {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            });
            tokio::pin!(race);
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            // Receipt A arrives (whether or not it will prove trusted).
            gate.decisions_arrived();
            assert!(
                futures_util::poll!(race.as_mut()).is_pending(),
                "B is not dropped"
            );
            release.send(7).unwrap();
            assert!(
                matches!(race.await, Ok(7)),
                "B finishes its original admission"
            );
        }
        assert_eq!(
            polled.load(Ordering::SeqCst),
            1,
            "never rebuilt or re-observed"
        );
        // V5: the receipt's readiness is serviced in the seam (matching and
        // checking run during B), not by cancelling B; it is consumed there.
        assert_eq!(
            ready.load(Ordering::SeqCst),
            1,
            "readiness serviced during B"
        );

        // Real invalidation still cancels an admission.
        {
            let race = race_admission(
                std::future::pending::<u8>(),
                &mut commands,
                &gate,
                &mut idle,
                |_, _| {},
            );
            tokio::pin!(race);
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            gate.invalidate();
            assert!(matches!(race.await, Err(Interrupt::Changed)));
        }
        // So does a command.
        send.try_send(Command::Dismiss {
            key: "incoming-1".into(),
        })
        .unwrap_or_else(|_| panic!("command failed"));
        {
            let race = race_admission(
                std::future::pending::<u8>(),
                &mut commands,
                &gate,
                &mut idle,
                |_, _| {},
            );
            assert!(matches!(race.await, Err(Interrupt::Command(Some(_)))));
        }
        // And a stop.
        {
            let race = race_admission(
                std::future::pending::<u8>(),
                &mut commands,
                &gate,
                &mut idle,
                |_, _| {},
            );
            tokio::pin!(race);
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            gate.shutdown();
            assert!(matches!(race.await, Err(Interrupt::Changed)));
        }
    }
    /// The actual admission seam (race_admission, as run() uses it) keeps
    /// servicing decline checks while call B's admission is held: call A's
    /// check that finishes meanwhile (here untrusted, as a failed or timed-out
    /// check would) is applied at once, not after B, so A is shown or retired
    /// without waiting for B's admission limit; a check that application
    /// starts is serviced too. B keeps its one original admission future,
    /// polled on, never rebuilt, and then finishes. A command still
    /// interrupts as before.
    #[tokio::test]
    async fn decline_checks_finish_while_another_calls_admission_is_held() {
        type Check =
            std::pin::Pin<Box<dyn std::future::Future<Output = (&'static str, bool)> + Send>>;
        let (_controller, gate) = gate();
        let (send, mut commands) = mpsc::channel(1);
        let (release_b, held_b) = oneshot::channel::<u8>();
        let (finish_a, check_a) = oneshot::channel::<bool>();
        let (finish_c, check_c) = oneshot::channel::<bool>();
        let mut checks: FuturesUnordered<Check> = FuturesUnordered::new();
        checks.push(Box::pin(
            async move { ("A", check_a.await.unwrap_or(false)) },
        ));
        let mut started: Option<Check> =
            Some(Box::pin(
                async move { ("C", check_c.await.unwrap_or(false)) },
            ));
        let applied = Arc::new(Mutex::new(Vec::new()));
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let admission = {
            let built = built.clone();
            built.fetch_add(1, Ordering::SeqCst);
            async move { held_b.await.unwrap_or(0) }
        };
        {
            let record = applied.clone();
            let race = race_admission(
                admission,
                &mut commands,
                &gate,
                &mut checks,
                |event, checks| {
                    if let AdmissionEvent::Completed(done) = event {
                        record.lock().unwrap().push(done);
                        // Applying a completion may start another check.
                        if let Some(next) = started.take() {
                            checks.push(next);
                        }
                    }
                },
            );
            tokio::pin!(race);
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            finish_a.send(false).unwrap();
            assert!(
                futures_util::poll!(race.as_mut()).is_pending(),
                "B is still admitting"
            );
            assert_eq!(
                *applied.lock().unwrap(),
                [("A", false)],
                "A applied during B"
            );
            finish_c.send(true).unwrap();
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            assert_eq!(*applied.lock().unwrap(), [("A", false), ("C", true)]);
            release_b.send(9).unwrap();
            assert!(
                matches!(race.await, Ok(9)),
                "B finishes its original admission"
            );
        }
        assert_eq!(
            built.load(Ordering::SeqCst),
            1,
            "B's admission was built once"
        );

        // A command still interrupts an admission while a check is held.
        let (_finish, held) = oneshot::channel::<bool>();
        checks.push(Box::pin(async move { ("D", held.await.unwrap_or(false)) }));
        send.try_send(Command::Dismiss {
            key: "incoming-1".into(),
        })
        .unwrap_or_else(|_| panic!("command failed"));
        let race = race_admission(
            std::future::pending::<u8>(),
            &mut commands,
            &gate,
            &mut checks,
            |_, _| {},
        );
        assert!(matches!(race.await, Err(Interrupt::Command(Some(_)))));
        assert_eq!(
            checks.len(),
            1,
            "the held check survives for the outer loop"
        );
    }
    /// A NEW decline that arrives while call B's admission is held is served
    /// in the same seam: its readiness begins checks (one per declining device
    /// and offer, in the real call book), they resolve and are applied with
    /// the real settlement rule BEFORE B is released, and B's one original
    /// admission future still finishes once. A trusted decline retires ringing
    /// call A; an untrusted one ends the withholding of never-shown call E,
    /// which is then shown normally; one resolving after its receipt's own
    /// original deadline settles nothing; a replayed arrival begins no second
    /// check. A command, real invalidation and a stop keep priority over
    /// receipt readiness.
    #[tokio::test]
    async fn a_new_decline_is_checked_and_applied_while_another_call_admits() {
        use super::super::controller::{ActionToken, Lease, OfferBook, decline_still_settles};
        type Check = std::pin::Pin<
            Box<dyn std::future::Future<Output = (ActionToken, bool, Instant)> + Send>,
        >;
        let (_controller, gate) = gate();
        let (send, mut commands) = mpsc::channel(1);
        let now = Instant::now();
        let lease = Arc::new(Lease::new(1, now + Duration::from_secs(30)));
        let book = Arc::new(Mutex::new(OfferBook::default()));
        let (ringing, unshown, late) = {
            let mut book = book.lock().unwrap();
            let ringing = book.insert(1u8, lease.clone()).unwrap();
            book.mark_presented(ringing);
            let unshown = book.insert(2u8, lease.clone()).unwrap();
            let late = book.insert(3u8, lease.clone()).unwrap();
            book.mark_presented(late);
            (ringing, unshown, late)
        };
        // Each offer's one check, reporting its trust result and its
        // receipt's original deadline (already passed for `late`).
        let (trust_ringing, ringing_trust) = oneshot::channel::<bool>();
        let (trust_unshown, unshown_trust) = oneshot::channel::<bool>();
        let (trust_late, late_trust) = oneshot::channel::<bool>();
        let later = now + Duration::from_secs(60);
        let mut ready: Vec<(ActionToken, Option<Check>)> =
            vec![
                (
                    ringing,
                    Some(Box::pin(
                        async move { (ringing, ringing_trust.await.unwrap_or(false), later) },
                    ) as Check),
                ),
                (
                    unshown,
                    Some(Box::pin(
                        async move { (unshown, unshown_trust.await.unwrap_or(false), later) },
                    ) as Check),
                ),
                (
                    late,
                    Some(
                        Box::pin(async move { (late, late_trust.await.unwrap_or(false), now) })
                            as Check,
                    ),
                ),
            ];
        let mut checks: FuturesUnordered<Check> = FuturesUnordered::new();
        let begun = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let settled = Arc::new(Mutex::new(Vec::new()));
        let (release_b, held_b) = oneshot::channel::<u8>();
        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let admission = {
            built.fetch_add(1, Ordering::SeqCst);
            async move { held_b.await.unwrap_or(0) }
        };
        {
            let (shared, count, done) = (book.clone(), begun.clone(), settled.clone());
            let race = race_admission(
                admission,
                &mut commands,
                &gate,
                &mut checks,
                |event, checks| {
                    let mut book = shared.lock().unwrap();
                    match event {
                        // Match each fresh decline to its exact offer; a device
                        // already checked for that offer begins nothing.
                        AdmissionEvent::Receipts => {
                            for (token, check) in ready.iter_mut() {
                                if book.begin_check(*token, "PHONE") {
                                    checks.push(check.take().expect("one check per device"));
                                    count.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                        }
                        AdmissionEvent::Completed((token, trusted, expires)) => {
                            book.end_check(token);
                            if decline_still_settles(
                                trusted,
                                true,
                                expires,
                                Instant::now(),
                                true,
                                true,
                            ) && book.take(token).is_ok()
                            {
                                done.lock().unwrap().push(token);
                            }
                        }
                    }
                },
            );
            tokio::pin!(race);
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            // Fresh declines arrive during B.
            gate.decisions_arrived();
            assert!(
                futures_util::poll!(race.as_mut()).is_pending(),
                "B keeps admitting"
            );
            assert_eq!(begun.load(Ordering::SeqCst), 3, "checks begun during B");
            assert!(
                book.lock().unwrap().withheld(unshown),
                "never shown: waits for its check"
            );
            assert!(
                !book.lock().unwrap().withheld(ringing),
                "ringing: keeps ringing"
            );
            // Trusted: the ringing call is retired, before B is released.
            trust_ringing.send(true).unwrap();
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            assert_eq!(*settled.lock().unwrap(), [ringing]);
            // Untrusted: the never-shown call is shown normally.
            trust_unshown.send(false).unwrap();
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            assert!(!book.lock().unwrap().withheld(unshown));
            // Trusted, but after its receipt's own deadline: nothing settles.
            trust_late.send(true).unwrap();
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            assert_eq!(
                *settled.lock().unwrap(),
                [ringing],
                "late settlement refused"
            );
            // A replayed arrival begins no second check.
            gate.decisions_arrived();
            assert!(futures_util::poll!(race.as_mut()).is_pending());
            assert_eq!(begun.load(Ordering::SeqCst), 3, "no second check");
            release_b.send(9).unwrap();
            assert!(
                matches!(race.await, Ok(9)),
                "B finishes its original admission"
            );
        }
        assert_eq!(
            built.load(Ordering::SeqCst),
            1,
            "B's admission was built once"
        );
        // Neither the untrusted nor the late decline removed its call.
        assert!(book.lock().unwrap().take(unshown).is_ok());
        assert!(book.lock().unwrap().take(late).is_ok());

        // Receipt readiness never outranks a command, real invalidation or a
        // stop.
        let mut idle = FuturesUnordered::<std::future::Ready<u8>>::new();
        gate.decisions_arrived();
        send.try_send(Command::Dismiss {
            key: "incoming-1".into(),
        })
        .unwrap_or_else(|_| panic!("command failed"));
        {
            let race = race_admission(
                std::future::pending::<u8>(),
                &mut commands,
                &gate,
                &mut idle,
                |_, _| {},
            );
            assert!(matches!(race.await, Err(Interrupt::Command(Some(_)))));
        }
        gate.decisions_arrived();
        gate.invalidate();
        {
            let race = race_admission(
                std::future::pending::<u8>(),
                &mut commands,
                &gate,
                &mut idle,
                |_, _| {},
            );
            assert!(matches!(race.await, Err(Interrupt::Changed)));
        }
        gate.decisions_arrived();
        gate.shutdown();
        {
            let race = race_admission(
                std::future::pending::<u8>(),
                &mut commands,
                &gate,
                &mut idle,
                |_, _| {},
            );
            assert!(matches!(race.await, Err(Interrupt::Changed)));
        }
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
        // A Decline command is bounded the same way: an unhandled command
        // drops its reply, so its waiter wakes with no claim and sends nothing.
        let (reply, result) = oneshot::channel();
        send.try_send(Command::Decline {
            key: "incoming-3".into(),
            reply,
        })
        .unwrap_or_else(|_| panic!("decline command failed"));
        assert!(
            send.try_send(Command::Dismiss {
                key: "incoming-4".into()
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

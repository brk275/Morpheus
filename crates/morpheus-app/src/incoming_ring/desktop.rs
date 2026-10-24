//! One silent native banner per admitted incoming call of the account on
//! screen, through the notification presenter (`notifications.rs`).
//!
//! Offers come only from the incoming worker's validated output
//! (`IncomingHandle::snapshot`), keyed by its opaque token: no Matrix event is
//! read or parsed here. Each banner keeps the account selection and security
//! generation captured when its offer was first seen, and the offer's own
//! deadline; later checks re-read them, never replace them. A click only
//! brings the Morpheus window back from the dock or taskbar: it never answers,
//! joins, declines, switches or unlocks an account, opens a device or captures
//! media. The banner is silent; `incoming_ring::audio` alone rings.
//!
//! Privacy: the account is read first and the window (its private screens)
//! last, as message banners do, both when a banner is planned and again after
//! the OS call returns. The caller is named only while the settings and the
//! window allow it; a banner that names more than is now allowed, or whose
//! alerts are off, is forgotten (its click invalidated) and withdrawn where
//! that is possible (see below). Text already handed to the OS cannot be
//! taken back.
//!
//! Delivery: the blocking OS call runs on one owned thread at a time (not the
//! async runtime's blocking pool), so it never holds the runtime's shutdown.
//! Right before the backend's actual send (`notifications::Handover`), which
//! on macOS is in the presenter's listener, that thread takes a fresh, bounded
//! sample of the window and checks it with the record, the original captures,
//! the offer, the settings and quitting. A send that fails there is reported,
//! not ignored; a guarded macOS banner with no free listener is not sent.
//!
//! Records hold identifiers only (tokens, the local account ID, the
//! selection and generations), never caller or room text, and nothing is
//! persisted or logged. Everything is bounded and ends with the app.
//!
//! When an offer ends (answered, dismissed, declined, expired, revoked, after
//! an account switch, removal or lock, or on quit) its record is forgotten and
//! its click invalidated on every platform. Withdrawal is possible only on
//! Linux, and there only when the presenter returned a withdrawal handle for
//! that banner (its listener slot was reserved and its thread started);
//! without one, and always on macOS and Windows, the OS may keep the banner
//! on screen, but its click opens nothing.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use morpheus_matrix::AccountSelection;
use slint::{ComponentHandle, Weak as UiWeak};
use tokio::{
    runtime::Handle,
    sync::{mpsc, oneshot},
    time,
};

use super::{controller::ActionToken, worker::IncomingHandle};
use crate::{
    AppWindow,
    live::LiveUi,
    notifications::{
        self, BannerUnavailable, DesktopNotifications, Guarded, Handover, NotificationDrainFailure,
        PresenterEvent, Withdrawal,
        owned::{Close, NotificationWork, Role},
    },
};

/// Banners remembered for a click or withdrawal (the worker keeps at most
/// four offers); past this the oldest is forgotten and withdrawn.
const MAX_BANNERS: usize = 8;
/// Shows waiting for the OS, handed over one at a time.
const PENDING_SHOWS: usize = 4;
/// No banner stays clickable longer than this, whatever its offer says.
const BANNER_TTL: Duration = Duration::from_secs(2 * 60);
/// Presenter reports waiting to be resolved; a full queue drops a report.
const EVENTS: usize = 8;
/// How often ended, expired and newly private banners are looked for between
/// updates; also the most often the window is read for them.
const TICK: Duration = Duration::from_secs(1);
const UI_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a show waits for the OS call. Past it the banner is given up;
/// the delivery thread is not stopped (it cannot be) and withdraws what it
/// shows late when it got a withdrawal handle, and no other delivery starts
/// until it ends.
const DELIVERY_WAIT: Duration = Duration::from_secs(10);
/// Shown in the incoming-call notice line, while the offer is still current,
/// when a banner was not confirmed: the OS failed it, no listener was free,
/// or the call did not return in time (and may still show it later). It
/// claims neither that a banner appeared nor that none did. No backend or
/// server text.
const UNAVAILABLE: &str =
    "Could not confirm a desktop alert for this call. It is still shown here.";

/// What a banner was captured under when its offer was first seen.
#[derive(Clone)]
struct Origin {
    selection: AccountSelection,
    security: u64,
}

/// One banner: identifiers only.
struct Banner<K, C> {
    offer: K,
    account_id: String,
    origin: C,
    deadline: Instant,
    /// Whether the text planned (and handed to the OS) names the caller.
    named: bool,
    /// `None` while the OS call is in flight; then the withdrawal handle the
    /// presenter returned, which exists only on Linux and only when its
    /// listener started.
    shown: Option<Option<Withdrawal>>,
}

impl<K, C> Banner<K, C> {
    /// Withdraw the banner if a handle was returned; otherwise nothing can.
    fn withdraw(&mut self) {
        if let Some(Some(withdrawal)) = self.shown.take() {
            withdrawal.withdraw();
        }
    }
}

/// The bounded record of banners, generic over the offer key and capture so
/// its rules can be exercised without an account or a window.
struct Registry<K, C> {
    /// The highest offer token seen. The worker issues tokens in increasing
    /// order, so an offer at or below it is a refresh, never a new banner.
    last_seen: Option<K>,
    banners: BTreeMap<u64, Banner<K, C>>,
    next: u64,
}

impl<K, C> Default for Registry<K, C> {
    fn default() -> Self {
        Self {
            last_seen: None,
            banners: BTreeMap::new(),
            next: 1,
        }
    }
}

impl<K: Ord + Copy, C: Clone> Registry<K, C> {
    /// Whether `offer` is seen for the first time. Offers are taken in
    /// increasing token order.
    fn first_sighting(&mut self, offer: K) -> bool {
        if self.last_seen.is_some_and(|last| offer <= last) {
            return false;
        }
        self.last_seen = Some(offer);
        true
    }

    /// Remember a banner about to be shown; returns its own token. The
    /// oldest record is forgotten and withdrawn past the bound.
    fn insert(&mut self, offer: K, account_id: &str, origin: C, deadline: Instant) -> u64 {
        while self.banners.len() >= MAX_BANNERS {
            let Some(oldest) = self.banners.keys().next().copied() else {
                break;
            };
            if let Some(mut banner) = self.banners.remove(&oldest) {
                banner.withdraw();
            }
        }
        let token = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.banners.insert(
            token,
            Banner {
                offer,
                account_id: account_id.to_owned(),
                origin,
                deadline,
                named: false,
                shown: None,
            },
        );
        token
    }

    fn is_empty(&self) -> bool {
        self.banners.is_empty()
    }

    /// A banner still waiting for the OS, before its deadline.
    fn pending(&self, token: u64, now: Instant) -> Option<(K, String, C)> {
        self.banners
            .get(&token)
            .filter(|entry| entry.shown.is_none() && now < entry.deadline)
            .map(|entry| (entry.offer, entry.account_id.clone(), entry.origin.clone()))
    }

    /// A remembered banner in any state, before its deadline.
    fn current(&self, token: u64, now: Instant) -> Option<(K, String, C)> {
        self.banners
            .get(&token)
            .filter(|entry| now < entry.deadline)
            .map(|entry| (entry.offer, entry.account_id.clone(), entry.origin.clone()))
    }

    /// Record what the planned text names; false when it is no longer waiting.
    fn planned(&mut self, token: u64, named: bool) -> bool {
        match self.banners.get_mut(&token) {
            Some(banner) if banner.shown.is_none() => {
                banner.named = named;
                true
            }
            _ => false,
        }
    }

    /// Forget a banner that was never shown.
    fn forget(&mut self, token: u64) {
        self.banners.remove(&token);
    }

    /// Forget, and withdraw where possible, every banner past its deadline or
    /// that `keep` no longer keeps.
    fn retire(&mut self, now: Instant, keep: impl Fn(&Banner<K, C>) -> bool) {
        self.banners.retain(|_, banner| {
            let kept = now < banner.deadline && keep(banner);
            if !kept {
                banner.withdraw();
            }
            kept
        });
    }

    /// The OS took banner `token`. It is kept only while still remembered as
    /// waiting and `keep`; otherwise the late banner is withdrawn at once,
    /// so a stale completion never brings a record back.
    fn shown(&mut self, token: u64, withdrawal: Option<Withdrawal>, keep: bool) -> bool {
        match self.banners.get_mut(&token) {
            Some(banner) if keep && banner.shown.is_none() => {
                banner.shown = Some(withdrawal);
                true
            }
            _ => {
                self.banners.remove(&token);
                if let Some(withdrawal) = withdrawal {
                    withdrawal.withdraw();
                }
                false
            }
        }
    }

    /// A clicked banner, once: only one the OS took, before its deadline.
    fn take_clicked(&mut self, token: u64, now: Instant) -> Option<Banner<K, C>> {
        if self.banners.get(&token)?.shown.is_none() {
            return None;
        }
        let banner = self.banners.remove(&token)?;
        (now < banner.deadline).then_some(banner)
    }

    /// The OS closed banner `token`: nothing is left to click or withdraw.
    fn closed(&mut self, token: u64) {
        if self
            .banners
            .get(&token)
            .is_some_and(|banner| banner.shown.is_some())
        {
            self.banners.remove(&token);
        }
    }

    /// Forget every banner, withdrawing what the platform allows.
    fn clear(&mut self) {
        for (_, mut banner) in std::mem::take(&mut self.banners) {
            banner.withdraw();
        }
    }
}

/// Whether the current policy still allows what a banner hands over:
/// `identity` as `DesktopNotifications::silent_banner_identity` reads it now
/// (`None`: alerts off; `Some(false)`: the account only; `Some(true)`: the
/// caller too). Account-only permission never allows a named banner.
fn permits(identity: Option<bool>, named: bool) -> bool {
    identity.is_some_and(|caller| caller || !named)
}

/// Read the asynchronous account state first and the window last, as message
/// banners do (`notifications::final_snapshot`): whatever the window shows
/// once the account read returned applies, including a private screen opened
/// while that read was waiting. Each source does nothing until awaited.
async fn account_then_window<A, W>(
    account: impl Future<Output = Option<A>>,
    window: impl Future<Output = Option<W>>,
) -> Option<(A, W)> {
    let account = account.await?;
    let window = window.await?;
    Some((account, window))
}

/// Ask for one answer through `post` and wait for it at most `wait`, blocking
/// this thread and holding nothing else. `None` when it could not be posted,
/// its answer was dropped (a window that is closing) or it did not come in
/// time; a late answer then finds nobody and is discarded.
fn sample<T: Send + 'static>(
    wait: Duration,
    post: impl FnOnce(std::sync::mpsc::SyncSender<T>) -> bool,
) -> Option<T> {
    let (answer, receive) = std::sync::mpsc::sync_channel(1);
    if !post(answer) {
        return None;
    }
    receive.recv_timeout(wait).ok()
}

/// Whether a fresh `screen` sample allows handing over a banner for
/// `account_id` that names the caller or not: the session is live, that
/// account is on screen, and the settings (`identity`, given the sample's
/// private screen) allow that much.
fn window_admits(
    screen: &Screen,
    account_id: &str,
    named: bool,
    identity: impl FnOnce(bool) -> Option<bool>,
) -> bool {
    screen.live && screen.account_id == account_id && permits(identity(screen.private), named)
}

/// The one delivery thread's slot: taken before it starts, released when it
/// ends (also by unwinding), so delivery threads never accumulate behind an
/// OS call that does not return.
struct DeliverySlot(Arc<AtomicBool>);

impl DeliverySlot {
    fn take(slot: &Arc<AtomicBool>) -> Option<Self> {
        slot.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(slot.clone()))
    }
}

impl Drop for DeliverySlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
type Shown = Result<Guarded, BannerUnavailable>;

/// Give the delivery thread's result to the show task. When nobody waits any
/// more (the show gave the banner up, or Morpheus is quitting), a banner
/// shown late is withdrawn at once if the presenter returned a handle for it
/// (Linux only); without one it stays, and its record is already gone.
#[cfg(test)]
fn hand_back(done: oneshot::Sender<Shown>, shown: Shown) {
    if let Err(Ok(Guarded::Handed(Some(withdrawal)))) = done.send(shown) {
        withdrawal.withdraw();
    }
}

/// What a show ended with.
enum Delivered {
    /// Nothing reached the OS.
    Refused,
    /// Handed to the OS, with the presenter's withdrawal handle if it returned
    /// one (Linux only, and only when its listener started).
    Handed(Option<Withdrawal>),
    /// The OS took no banner, or did not answer in time.
    Unavailable,
}

/// The text for the OS, held only until it is handed over.
struct Plan {
    summary: String,
    body: String,
    named: bool,
}

/// What the window shows: read on the UI thread.
struct Screen {
    live: bool,
    account_id: String,
    private: bool,
}

struct Shared {
    closing: Close,
    registry: Mutex<Registry<ActionToken, Origin>>,
    offers: IncomingHandle,
    live: Arc<LiveUi>,
    notifications: Arc<DesktopNotifications>,
    ui: UiWeak<AppWindow>,
    events: mpsc::Sender<PresenterEvent>,
    shows: mpsc::Sender<u64>,
    /// Taken by the one delivery thread while it runs.
    delivering: Arc<AtomicBool>,
    /// At most one click waits for the UI thread; another meanwhile is dropped.
    click_posted: AtomicBool,
    /// The offer the failure notice is about, and whether a post is queued.
    status: Mutex<Option<(ActionToken, String)>>,
    status_posted: AtomicBool,
}

impl Shared {
    fn registry(&self) -> MutexGuard<'_, Registry<ActionToken, Origin>> {
        self.registry
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn stopped(&self) -> bool {
        self.closing.is_closed()
    }

    /// The captures still hold: not quitting, the same account selection
    /// (never a newer one of the same account), no security step begun
    /// since, and the session live.
    fn holds(&self, origin: &Origin) -> bool {
        !self.stopped()
            && origin.selection.is_current()
            && self.live.security_gate.ringing_allowed(origin.security)
            && self.live.is_live.load(Ordering::Acquire)
    }

    /// Whether `offer` is still one of the worker's current offers for
    /// `account_id`, as the incoming card reads them.
    fn offered(&self, offer: ActionToken, account_id: &str) -> bool {
        self.offers
            .snapshot()
            .iter()
            .any(|current| current.token == offer && current.account_id == account_id)
    }

    /// Whether the settings now allow a banner for `account_id` that names
    /// the caller or not (`named`), with or without a private screen up.
    fn permits_now(&self, account_id: &str, named: bool, private: bool) -> bool {
        let identity = self
            .notifications
            .silent_banner_identity(account_id, private);
        permits(identity, named)
    }

    /// Follow the worker's current offers: retire the banners of ended
    /// offers, of broken captures and of settings that no longer allow them,
    /// and queue one show for each offer seen for the first time.
    fn observe(self: &Arc<Self>) {
        let mut offers = self.offers.snapshot();
        offers.sort_by_key(|offer| offer.token);
        let now = Instant::now();
        {
            let mut registry = self.registry();
            registry.retire(now, |banner| {
                offers.iter().any(|offer| {
                    offer.token == banner.offer && offer.account_id == banner.account_id
                }) && self.holds(&banner.origin)
                    && self.permits_now(&banner.account_id, banner.named, false)
            });
            for offer in &offers {
                if self.stopped() || !registry.first_sighting(offer.token) {
                    continue;
                }
                // Captured at the first sighting only: no later check adopts a
                // newer selection or security generation.
                let Some(selection) = self
                    .live
                    .manager
                    .account_selection(&offer.account_id)
                    .filter(AccountSelection::is_current)
                else {
                    continue;
                };
                let origin = Origin {
                    selection,
                    security: self.live.security_gate.ringing_generation(),
                };
                // Read only; the sound's own claim stays with audio.
                let deadline = offer
                    .audible
                    .deadline()
                    .min(now.checked_add(BANNER_TTL).unwrap_or(now));
                let token = registry.insert(offer.token, &offer.account_id, origin, deadline);
                if self.shows.try_send(token).is_err() {
                    registry.forget(token);
                }
            }
        }
        // A failure notice whose offer ended is cleared.
        let stale = self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|(offer, account_id)| !self.offered(*offer, account_id));
        if stale {
            self.post_status();
        }
    }

    /// Retire every banner that names more than `screen` now allows, or is not
    /// for the account it shows (see `observe` for the rest).
    fn retire_for(&self, screen: &Screen) {
        self.registry().retire(Instant::now(), |banner| {
            screen.live
                && screen.account_id == banner.account_id
                && self.permits_now(&banner.account_id, banner.named, screen.private)
        });
    }

    /// What the window shows, read on the UI thread.
    async fn screen(&self) -> Option<Screen> {
        let (send, receive) = oneshot::channel();
        self.ui
            .upgrade_in_event_loop(move |ui| {
                if ui.get_application_closing() {
                    return;
                }
                let _ = send.send(Screen {
                    live: ui.get_live_session(),
                    account_id: ui.get_active_account_local_id().to_string(),
                    private: notifications::private_screen(&ui),
                });
            })
            .ok()?;
        time::timeout(UI_QUERY_TIMEOUT, receive).await.ok()?.ok()
    }

    /// Plan banner `token` right before it is handed over: the account first,
    /// the window last, then everything else after that last await; `None`
    /// shows nothing. The caller's name lives only in the returned text,
    /// which goes to the OS and nowhere else.
    async fn plan(&self, token: u64) -> Option<Plan> {
        let (offer, account_id, origin) = self.registry().pending(token, Instant::now())?;
        let (active, screen) =
            account_then_window(self.live.manager.active_account(), self.screen()).await?;
        let named = self
            .notifications
            .silent_banner_identity(&account_id, screen.private)?;
        let caller = self
            .offers
            .snapshot()
            .into_iter()
            .find(|current| current.token == offer && current.account_id == account_id)?
            .caller_user_id;
        let current = self.registry().pending(token, Instant::now()).is_some()
            && self.holds(&origin)
            && screen.live
            && screen.account_id == account_id
            && active.id == account_id;
        if !current || !self.registry().planned(token, named) {
            return None;
        }
        let (summary, body) = notifications::call_banner_text(&active.user_id, &caller, named);
        Some(Plan {
            summary,
            body,
            named,
        })
    }

    /// After the OS call: the account first, the window last, then the
    /// captures, the offer, the settings and quitting, all as they are now.
    /// The banner is kept only if what it handed over is still allowed.
    async fn confirm(&self, token: u64, named: bool) -> bool {
        let Some((offer, account_id, origin)) = self.registry().pending(token, Instant::now())
        else {
            return false;
        };
        let Some((active, screen)) =
            account_then_window(self.live.manager.active_account(), self.screen()).await
        else {
            return false;
        };
        self.registry().pending(token, Instant::now()).is_some()
            && self.holds(&origin)
            && self.offered(offer, &account_id)
            && screen.live
            && screen.account_id == account_id
            && active.id == account_id
            && self.permits_now(&account_id, named, screen.private)
    }

    /// The check at the backend's actual send, for banner `token`.
    fn handover(self: &Arc<Self>, token: u64, named: bool) -> Handover {
        let shared = Arc::downgrade(self);
        Arc::new(move || {
            shared
                .upgrade()
                .is_some_and(|shared| shared.may_hand_over(token, named))
        })
    }

    /// On the thread making the send, right before it: a fresh sample of the
    /// window taken now (never the watcher's), then the record, the original
    /// captures, the offer and the settings with that sample's privacy, all
    /// as they are now. No lock is held while the sample is awaited; quitting,
    /// a closed window or no answer in time refuses. A refused banner is
    /// forgotten: nothing was sent for it.
    fn may_hand_over(&self, token: u64, named: bool) -> bool {
        if self.stopped() {
            return false;
        }
        let Some(screen) = self.screen_now() else {
            self.registry().forget(token);
            return false;
        };
        let current = self.registry().current(token, Instant::now());
        let admitted = current.is_some_and(|(offer, account_id, origin)| {
            self.holds(&origin)
                && self.offered(offer, &account_id)
                && window_admits(&screen, &account_id, named, |private| {
                    self.notifications
                        .silent_banner_identity(&account_id, private)
                })
        });
        if !admitted {
            self.registry().forget(token);
        }
        admitted
    }

    /// What the window shows, sampled from a delivery or listener thread
    /// (never the UI thread, never an async task): posted through the weak
    /// window handle and waited for at most `UI_QUERY_TIMEOUT`.
    fn screen_now(&self) -> Option<Screen> {
        let ui = self.ui.clone();
        sample(UI_QUERY_TIMEOUT, move |answer| {
            ui.upgrade_in_event_loop(move |ui| {
                if ui.get_application_closing() {
                    return;
                }
                let _ = answer.try_send(Screen {
                    live: ui.get_live_session(),
                    account_id: ui.get_active_account_local_id().to_string(),
                    private: notifications::private_screen(&ui),
                });
            })
            .is_ok()
        })
    }

    /// Hand `plan` to the OS on the one delivery thread and wait a bounded
    /// time for it. The thread is owned by its slot, not by the runtime:
    /// the exact presenter book retains it through actual joins.
    async fn deliver(self: &Arc<Self>, token: u64, plan: Plan) -> Delivered {
        let Some(slot) = DeliverySlot::take(&self.delivering) else {
            // The previous OS call has not returned: nothing more starts.
            return Delivered::Unavailable;
        };
        // The presenter book retains the real worker/TLS and supervisor;
        // timeout cancels this exact pending operation, never drops its owner.
        let _slot = slot;
        let handover = self.handover(token, plan.named);
        let shown = self.notifications.show_silent_banner(
            &plan.summary,
            &plan.body,
            token,
            self.events.clone(),
            Handle::current(),
            handover,
        );
        match time::timeout(DELIVERY_WAIT, shown).await {
            Ok(Ok(Guarded::Handed(withdrawal))) => Delivered::Handed(withdrawal),
            Ok(Ok(Guarded::Refused)) => Delivered::Refused,
            _ => Delivered::Unavailable,
        }
    }

    /// A banner was not confirmed (the OS failed it, no listener was free, the
    /// call timed out, or the macOS listener's send failed, before or after
    /// the banner was marked shown): forget it, and say so while its offer and
    /// captures still hold. An expired or retired banner is not revived.
    /// Returns whether a notice is due.
    fn failed(&self, token: u64) -> bool {
        let remembered = self.registry().current(token, Instant::now());
        self.registry().forget(token);
        let Some((offer, account_id, origin)) = remembered else {
            return false;
        };
        if !self.holds(&origin) || !self.offered(offer, &account_id) {
            return false;
        }
        *self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some((offer, account_id));
        true
    }

    /// Show the failure notice while its offer is current, never over
    /// another notice, and clear it only while it is still this one. One
    /// post at a time; it reads the state when it runs.
    fn post_status(self: &Arc<Self>) {
        if self.status_posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::downgrade(self);
        let posted = self.ui.upgrade_in_event_loop(move |ui| {
            let Some(this) = this.upgrade() else {
                return;
            };
            this.status_posted.store(false, Ordering::Release);
            if ui.get_application_closing() {
                return;
            }
            let mut status = this
                .status
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let show = status.as_ref().is_some_and(|(offer, account_id)| {
                !this.stopped()
                    && ui.get_active_account_local_id().as_str() == account_id
                    && this.offered(*offer, account_id)
            });
            let notice = ui.get_incoming_call_notice();
            if show {
                if notice.is_empty() {
                    ui.set_incoming_call_notice(UNAVAILABLE.into());
                }
            } else {
                *status = None;
                if notice.as_str() == UNAVAILABLE {
                    ui.set_incoming_call_notice("".into());
                }
            }
        });
        if posted.is_err() {
            self.status_posted.store(false, Ordering::Release);
        }
    }

    /// A banner was clicked: at most one reveal waits for the UI thread.
    fn activate(self: &Arc<Self>, token: u64) {
        let Some(banner) = self.registry().take_clicked(token, Instant::now()) else {
            return;
        };
        if !self.holds(&banner.origin) || self.click_posted.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::downgrade(self);
        let posted = self.ui.upgrade_in_event_loop(move |ui| {
            let Some(this) = this.upgrade() else {
                return;
            };
            this.click_posted.store(false, Ordering::Release);
            this.reveal(&ui, &banner);
        });
        if posted.is_err() {
            self.click_posted.store(false, Ordering::Release);
        }
    }

    /// UI thread, at the click: everything again, as it is now. Only the
    /// window comes back; the incoming card itself offers Answer and Dismiss.
    fn reveal(&self, ui: &AppWindow, banner: &Banner<ActionToken, Origin>) {
        let current = !ui.get_application_closing()
            && Instant::now() < banner.deadline
            && self.holds(&banner.origin)
            && ui.get_live_session()
            && ui.get_active_account_local_id().as_str() == banner.account_id
            && !notifications::private_screen(ui)
            && self
                .notifications
                .silent_banner_identity(&banner.account_id, false)
                .is_some()
            && self.offered(banner.offer, &banner.account_id);
        if current && ui.window().is_minimized() {
            ui.window().set_minimized(false);
        }
    }
}

/// Owned by main until shutdown. No task, record or delivery start outlives
/// it; a delivery thread already inside the OS call ends on its own.
pub(crate) struct CallBanners {
    shared: Arc<Shared>,
    work: NotificationWork,
}

impl CallBanners {
    /// Follow `offers` (the incoming worker's output, read only) and show its
    /// banners through `notifications`.
    pub(crate) fn start(
        ui: &AppWindow,
        runtime: &Handle,
        offers: IncomingHandle,
        live: Arc<LiveUi>,
        notifications: Arc<DesktopNotifications>,
    ) -> Self {
        let (events, event_receiver) = mpsc::channel(EVENTS);
        let (shows, show_receiver) = mpsc::channel(PENDING_SHOWS);
        let work = NotificationWork::incoming();
        let shared = Arc::new(Shared {
            closing: work.cancellation(),
            registry: Mutex::new(Registry::default()),
            offers,
            live,
            notifications,
            ui: ui.as_weak(),
            events,
            shows,
            delivering: Arc::new(AtomicBool::new(false)),
            click_posted: AtomicBool::new(false),
            status: Mutex::new(None),
            status_posted: AtomicBool::new(false),
        });
        work.spawn(Role::IncomingWatch, runtime, watch(shared.clone()));
        work.spawn(
            Role::IncomingShow,
            runtime,
            show(shared.clone(), show_receiver),
        );
        work.spawn(
            Role::IncomingEvents,
            runtime,
            resolve(shared.clone(), event_receiver),
        );
        Self { shared, work }
    }

    /// Revoke handovers and clicks synchronously, then wake idle application
    /// tasks. An entered delivery is not aborted; its async owner stays held.
    /// Native delivery/listener joins remain a separate presenter boundary.
    pub(crate) fn shutdown(&self) {
        self.work.close();
        self.shared.registry().clear();
    }

    /// Join this exact application's incoming tasks; includes real local
    /// fixture owners, but excludes the separate native presenter threads.
    pub(crate) async fn finish_app_work(&self) -> Result<(), NotificationDrainFailure> {
        self.shutdown();
        self.work.finish_app_work().await
    }
}

impl Drop for CallBanners {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn watch(shared: Arc<Shared>) {
    let mut updates = shared.offers.subscribe();
    let mut tick = time::interval(TICK);
    tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut read: Option<Instant> = None;
    loop {
        updates.borrow_and_update();
        if shared.stopped() {
            break;
        }
        shared.observe();
        // A private screen that opened retires the banners that name more
        // than it allows: at most one window read per tick, and none while
        // nothing is remembered.
        let due = read.is_none_or(|at| at.elapsed() >= TICK);
        if due && !shared.registry().is_empty() {
            read = Some(Instant::now());
            if let Some(Some(screen)) = shared.closing.guard(shared.screen()).await {
                if !shared.stopped() {
                    shared.retire_for(&screen);
                }
            }
        }
        tokio::select! {
            biased;
            _ = shared.closing.cancelled() => break,
            changed = updates.changed() => { if changed.is_err() { break; } },
            _ = tick.tick() => {},
        }
    }
    shared.registry().clear();
}

async fn show(shared: Arc<Shared>, mut shows: mpsc::Receiver<u64>) {
    while let Some(Some(token)) = shared.closing.guard(shows.recv()).await {
        if shared.stopped() {
            shared.registry().forget(token);
            continue;
        }
        let Some(Some(plan)) = shared.closing.guard(shared.plan(token)).await else {
            shared.registry().forget(token);
            continue;
        };
        let named = plan.named;
        match shared.deliver(token, plan).await {
            Delivered::Refused => shared.registry().forget(token),
            Delivered::Handed(withdrawal) => {
                let keep = shared
                    .closing
                    .guard(shared.confirm(token, named))
                    .await
                    .unwrap_or(false);
                let publication = withdrawal.as_ref().and_then(Withdrawal::publication);
                let admitted =
                    shared
                        .registry()
                        .shown(token, withdrawal, keep && !shared.stopped());
                // No book/registry mutex is held while queueing an early event.
                if admitted {
                    if let Some(publication) = publication {
                        publication.publish();
                    }
                }
            }
            Delivered::Unavailable => {
                if shared.failed(token) {
                    shared.post_status();
                }
            }
        }
    }
}

async fn resolve(shared: Arc<Shared>, mut events: mpsc::Receiver<PresenterEvent>) {
    while let Some(Some(event)) = shared.closing.guard(events.recv()).await {
        match event {
            PresenterEvent::Activated(token) => shared.activate(token),
            PresenterEvent::Closed(token) => shared.registry().closed(token),
            PresenterEvent::Failed(token) => {
                if shared.failed(token) {
                    shared.post_status();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn later() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn handed(registry: &mut Registry<u64, bool>, offer: u64) -> (u64, oneshot::Receiver<()>) {
        let token = registry.insert(offer, "a", true, later());
        let (withdrawal, receiver) = Withdrawal::for_test();
        assert!(registry.shown(token, Some(withdrawal), true));
        (token, receiver)
    }

    #[test]
    fn a_refreshed_offer_shows_once_and_only_newer_offers_show_again() {
        let mut registry = Registry::<u64, bool>::default();
        assert!(registry.first_sighting(3));
        assert!(!registry.first_sighting(3), "a refresh never shows twice");
        assert!(registry.first_sighting(5));
        assert!(!registry.first_sighting(4), "an older offer is not new");
        assert!(!registry.first_sighting(5));
    }

    #[test]
    fn ended_expired_or_revoked_offers_retire_and_withdraw_their_banners() {
        let mut registry = Registry::<u64, bool>::default();
        let mut receivers = Vec::new();
        for (offer, origin) in [(1, true), (2, true), (3, false)] {
            let token = registry.insert(offer, "a", origin, later());
            let (withdrawal, receiver) = Withdrawal::for_test();
            assert!(registry.shown(token, Some(withdrawal), true));
            receivers.push(receiver);
        }
        let expired = registry.insert(4, "a", true, Instant::now());
        let (withdrawal, mut expired_receiver) = Withdrawal::for_test();
        assert!(registry.shown(expired, Some(withdrawal), true));
        // Offer 1 ended, offer 2 is current, offer 3 lost its capture (another
        // account, a lock or a security step) and offer 4 passed its deadline.
        let current = [(2, "a"), (3, "a"), (4, "a")];
        registry.retire(Instant::now(), |banner| {
            banner.origin && current.contains(&(banner.offer, banner.account_id.as_str()))
        });
        assert_eq!(registry.banners.len(), 1);
        assert_eq!(receivers[0].try_recv(), Ok(()));
        assert!(receivers[1].try_recv().is_err(), "the current one stays");
        assert_eq!(receivers[2].try_recv(), Ok(()));
        assert_eq!(expired_receiver.try_recv(), Ok(()));
        // The same offer under another account is not the same offer.
        registry.retire(Instant::now(), |banner| banner.account_id == "b");
        assert!(registry.is_empty());
        assert_eq!(receivers[1].try_recv(), Ok(()));
    }

    #[test]
    fn a_stale_completion_never_brings_a_record_back() {
        let mut registry = Registry::<u64, bool>::default();
        let token = registry.insert(1, "a", true, later());
        // The offer ended while the OS call was blocked.
        registry.retire(Instant::now(), |_| false);
        let (withdrawal, mut receiver) = Withdrawal::for_test();
        assert!(!registry.shown(token, Some(withdrawal), true));
        assert!(registry.is_empty());
        assert_eq!(receiver.try_recv(), Ok(()), "the late banner is withdrawn");
        // Still remembered, but no longer kept after the call returned.
        let token = registry.insert(2, "a", true, later());
        let (withdrawal, mut receiver) = Withdrawal::for_test();
        assert!(!registry.shown(token, Some(withdrawal), false));
        assert!(registry.is_empty());
        assert_eq!(receiver.try_recv(), Ok(()));
        // Quit clears what is still waiting, too.
        let token = registry.insert(3, "a", true, later());
        registry.clear();
        assert!(!registry.shown(token, None, true));
        assert!(registry.pending(token, Instant::now()).is_none());
        assert!(registry.current(token, Instant::now()).is_none());
    }

    #[test]
    fn a_click_resolves_once_only_after_the_os_took_it_and_before_expiry() {
        let mut registry = Registry::<u64, bool>::default();
        let token = registry.insert(1, "a", true, later());
        assert!(registry.take_clicked(token, Instant::now()).is_none());
        assert!(registry.pending(token, Instant::now()).is_some());
        assert!(registry.shown(token, None, true));
        assert!(registry.pending(token, Instant::now()).is_none());
        assert!(registry.current(token, Instant::now()).is_some());
        assert!(registry.take_clicked(token, Instant::now()).is_some());
        assert!(registry.take_clicked(token, Instant::now()).is_none());
        let expiring = registry.insert(2, "a", true, later());
        assert!(registry.shown(expiring, None, true));
        assert!(registry.take_clicked(expiring, later()).is_none());
        assert!(registry.is_empty(), "an expired click is consumed");
        // A banner the OS closed can no longer be clicked.
        let closed = registry.insert(3, "a", true, later());
        assert!(registry.shown(closed, None, true));
        registry.closed(closed);
        assert!(registry.take_clicked(closed, Instant::now()).is_none());
    }

    #[test]
    fn records_are_bounded_and_the_oldest_is_withdrawn() {
        let mut registry = Registry::<u64, bool>::default();
        let (first, mut receiver) = handed(&mut registry, 0);
        for offer in 1..=MAX_BANNERS as u64 {
            registry.insert(offer, "a", true, later());
        }
        assert_eq!(registry.banners.len(), MAX_BANNERS);
        assert!(!registry.banners.contains_key(&first));
        assert_eq!(receiver.try_recv(), Ok(()));
    }

    /// INCALERT-01: a private screen (sign-in, Manage accounts) that opens
    /// while the account read is still waiting applies to the banner: the
    /// window is read only once that read returned, so the caller is not
    /// named. Pure ordering helper with in-test sources; no OS or window.
    #[tokio::test]
    async fn a_private_screen_opened_during_the_held_account_read_applies() {
        let private = Arc::new(AtomicBool::new(false));
        let (release, held) = oneshot::channel::<()>();
        let account = async move { held.await.ok().map(|()| "a") };
        let window = {
            let private = private.clone();
            async move { Some(private.load(Ordering::Acquire)) }
        };
        let read = tokio::spawn(account_then_window(account, window));
        tokio::task::yield_now().await;
        private.store(true, Ordering::Release);
        release.send(()).unwrap();
        let (account, private_now) = read.await.unwrap().unwrap();
        assert_eq!(account, "a");
        assert!(private_now, "the window is read after the account read");
        let identity = private_now.then_some(false).or(Some(true));
        assert!(!permits(identity, true), "the caller is not named");
        assert!(permits(identity, false), "an account-only banner still is");
    }

    /// INCALERT-01: account-only permission is never caller permission, and
    /// alerts turned off allow nothing.
    #[test]
    fn account_only_permission_never_allows_a_named_banner() {
        assert!(permits(Some(true), true) && permits(Some(true), false));
        assert!(!permits(Some(false), true) && permits(Some(false), false));
        assert!(!permits(None, true) && !permits(None, false));
    }

    /// INCALERT-01: the settings or the window turn more private while the OS
    /// call is held. Its completion is not accepted (no live token stays) and
    /// withdrawal is requested from the handle (`Withdrawal`'s own channel
    /// here: no OS banner is involved); a banner that named nothing stays.
    /// The watcher retires a named banner the same way once it shows.
    #[test]
    fn a_completion_that_named_more_than_now_allowed_is_withdrawn() {
        let mut registry = Registry::<u64, bool>::default();
        let named = registry.insert(1, "a", true, later());
        assert!(registry.planned(named, true));
        let plain = registry.insert(2, "a", true, later());
        assert!(registry.planned(plain, false));
        let now_allowed = Some(false);
        let (withdrawal, mut receiver) = Withdrawal::for_test();
        let keep = permits(now_allowed, true);
        assert!(!registry.shown(named, Some(withdrawal), keep));
        assert_eq!(receiver.try_recv(), Ok(()), "withdrawal requested");
        assert!(registry.current(named, Instant::now()).is_none());
        assert!(registry.shown(plain, None, permits(now_allowed, false)));
        // Already shown and named, then a private screen opens.
        let (shown, mut receiver) = handed(&mut registry, 3);
        registry.banners.get_mut(&shown).unwrap().named = true;
        registry.retire(Instant::now(), |banner| permits(now_allowed, banner.named));
        assert_eq!(receiver.try_recv(), Ok(()));
        assert!(registry.current(shown, Instant::now()).is_none());
        assert!(registry.current(plain, Instant::now()).is_some());
        // A shown banner cannot be planned again.
        assert!(!registry.planned(plain, true));
    }

    /// INCALERT-02: one delivery thread at a time; its slot is released when
    /// the thread's owner drops it, also when the thread unwinds.
    #[test]
    fn delivery_threads_never_accumulate() {
        let slot = Arc::new(AtomicBool::new(false));
        let first = DeliverySlot::take(&slot).unwrap();
        assert!(DeliverySlot::take(&slot).is_none(), "no second thread");
        drop(first);
        let second = DeliverySlot::take(&slot).unwrap();
        let unwound = std::thread::spawn(move || {
            let _slot = second;
            panic!("an OS call that unwinds");
        })
        .join();
        assert!(unwound.is_err());
        assert!(DeliverySlot::take(&slot).is_some());
    }

    /// INCALERT-02: a delivery that completes after the show gave it up (it
    /// timed out, or Morpheus is quitting) withdraws its own banner; one still
    /// awaited is handed back unchanged. Handles only; no OS call.
    #[test]
    fn a_late_delivery_nobody_waits_for_withdraws_itself() {
        let (done, result) = oneshot::channel();
        drop(result);
        let (withdrawal, mut receiver) = Withdrawal::for_test();
        hand_back(done, Ok(Guarded::Handed(Some(withdrawal))));
        assert_eq!(receiver.try_recv(), Ok(()));
        let (done, mut result) = oneshot::channel();
        let (withdrawal, mut receiver) = Withdrawal::for_test();
        hand_back(done, Ok(Guarded::Handed(Some(withdrawal))));
        assert!(receiver.try_recv().is_err(), "awaited: not withdrawn here");
        assert!(matches!(
            result.try_recv(),
            Ok(Ok(Guarded::Handed(Some(_))))
        ));
    }

    /// INCALERT-01: the handover's own window sample is bounded. Answered in
    /// time it is used; not postable (a closed window), dropped unanswered or
    /// late, it refuses, and the late answer finds nobody. An in-test stand-in
    /// answers for the window; no UI or OS is involved.
    #[test]
    fn the_handover_window_sample_is_bounded() {
        let wait = Duration::from_millis(50);
        assert_eq!(sample(wait, |answer| answer.try_send(7).is_ok()), Some(7));
        assert_eq!(sample::<u8>(wait, |_| false), None, "closed");
        let dropped = sample::<u8>(wait, |answer| {
            drop(answer);
            true
        });
        assert_eq!(dropped, None);
        let (late_tx, late_rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        let held = sample::<u8>(wait, |answer| {
            std::thread::spawn(move || {
                std::thread::sleep(wait * 4);
                let _ = late_tx.send(answer.try_send(1).is_err());
            });
            true
        });
        assert_eq!(held, None, "no answer in time refuses");
        assert!(started.elapsed() >= wait);
        assert_eq!(late_rx.recv(), Ok(true), "the late answer finds nobody");
    }

    /// INCALERT-01: a private screen that opens after the plan, before the
    /// actual send, refuses a banner that names the caller. The handover
    /// decides on its own fresh sample, not the plan's or the watcher's. An
    /// in-test stand-in answers for the window; no UI or OS is involved.
    #[test]
    fn a_private_screen_opened_after_the_plan_refuses_a_named_handover() {
        let private = Arc::new(AtomicBool::new(false));
        let window = |private: &Arc<AtomicBool>| {
            let private = private.clone();
            sample(Duration::from_secs(1), move |answer| {
                std::thread::spawn(move || {
                    let _ = answer.try_send(Screen {
                        live: true,
                        account_id: "a".into(),
                        private: private.load(Ordering::Acquire),
                    });
                });
                true
            })
        };
        // Room and sender shown; a private screen hides the caller.
        let settings = |private: bool| Some(!private);
        let planned = window(&private).unwrap();
        assert!(window_admits(&planned, "a", true, settings));
        private.store(true, Ordering::Release);
        let at_send = window(&private).unwrap();
        assert!(!window_admits(&at_send, "a", true, settings), "named");
        assert!(window_admits(&at_send, "a", false, settings), "unnamed");
        assert!(!window_admits(&at_send, "b", false, settings), "account");
        let closed = Screen {
            live: false,
            account_id: "a".into(),
            private: false,
        };
        assert!(!window_admits(&closed, "a", false, settings));
        assert!(!window_admits(&planned, "a", false, |_| None), "alerts off");
    }

    /// INCALERT-02: a late macOS failure is resolved for a banner still
    /// waiting or already handed over, never for one that expired, was
    /// retired or was forgotten: nothing is revived.
    #[test]
    fn a_late_failure_finds_waiting_and_handed_banners_only() {
        let mut registry = Registry::<u64, bool>::default();
        let waiting = registry.insert(1, "a", true, later());
        assert!(registry.current(waiting, Instant::now()).is_some());
        let (handed, _receiver) = handed(&mut registry, 2);
        assert!(registry.pending(handed, Instant::now()).is_none());
        assert!(registry.current(handed, Instant::now()).is_some(), "handed");
        let expired = registry.insert(3, "a", true, Instant::now());
        assert!(registry.current(expired, later()).is_none(), "expired");
        let retired = registry.insert(4, "a", true, later());
        registry.retire(Instant::now(), |banner| banner.offer != 4);
        assert!(registry.current(retired, Instant::now()).is_none());
        registry.forget(waiting);
        assert!(registry.current(waiting, Instant::now()).is_none());
    }
}

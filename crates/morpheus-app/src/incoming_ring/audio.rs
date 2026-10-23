//! Output-only sound for actor-admitted offers. UI labels/keys and raw network
//! events cannot create audible authority. First observed offers are consumed
//! once, including while suppressed; refreshes never replay or queue sounds.

use super::{
    controller::{ActionToken, AudibleOffer, OfferPresentation},
    worker::IncomingHandle,
};
use crate::{calls::CallController, live::LiveUi};
use morpheus_rtc::{RingAudioError, RingAudioReleaseError, RingPlayback};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    runtime::Handle,
    sync::watch,
    task::JoinHandle,
    time::{Duration, MissedTickBehavior},
};

struct Playing {
    token: ActionToken,
    authority: AudibleOffer,
    playback: RingPlayback,
}
impl Drop for Playing {
    fn drop(&mut self) {
        self.authority.stop();
        self.playback.stop();
    }
}

#[derive(Default)]
struct AudioSignals {
    stopped: AtomicBool,
    answering: AtomicBool,
}
pub(crate) struct IncomingAudio {
    current: Mutex<Option<Playing>>,
    signals: Arc<AudioSignals>,
    unavailable: Mutex<Option<ActionToken>>,
    changed: watch::Sender<()>,
}
impl IncomingAudio {
    pub(crate) fn start(
        runtime: &Handle,
        offers: IncomingHandle,
        live: Arc<LiveUi>,
        calls: Arc<CallController>,
    ) -> (Arc<Self>, JoinHandle<()>) {
        let owner = Arc::new(Self {
            current: Mutex::new(None),
            signals: Arc::new(AudioSignals::default()),
            unavailable: Mutex::new(None),
            changed: watch::channel(()).0,
        });
        let this = owner.clone();
        let output_runtime = runtime.clone();
        let task = runtime.spawn(async move {
            let mut updates = offers.subscribe();
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                updates.borrow_and_update();
                if this.signals.stopped.load(Ordering::Acquire) {
                    break;
                }
                // snapshot validates the live worker's exact selected epoch;
                // every row also holds its own permanently revocable selection.
                this.observe(offers.snapshot(), &live, &calls, &output_runtime);
                tokio::select! {
                    changed = updates.changed() => { if changed.is_err() { break; } },
                    _ = tick.tick() => {},
                }
            }
            this.stop_current();
        });
        (owner, task)
    }
    fn observe(
        self: &Arc<Self>,
        rows: Vec<OfferPresentation>,
        live: &Arc<LiveUi>,
        calls: &Arc<CallController>,
        output_runtime: &Handle,
    ) {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = current.as_ref() {
            let inactive = !value.playback.is_active();
            if value.playback.unavailable() {
                self.unavailable(value.token);
            }
            if inactive {
                current.take();
            }
        }
        for offer in rows {
            // Claim before policy checks so a later preference/call transition
            // cannot make an already observed quiet offer begin ringing.
            if !offer.audible.claim() {
                continue;
            }
            let (policy_epoch, enabled, speaker) =
                live.voice_spaces.ringing_policy(&offer.account_id);
            let call_epoch = calls.ringing_generation();
            let security_epoch = live.security_gate.ringing_generation();
            if current.is_some()
                || !enabled
                || !offer.audible.allowed()
                || !live.is_live.load(Ordering::Acquire)
                || !calls.ringing_allowed(call_epoch)
                || !live.security_gate.ringing_allowed(security_epoch)
                || self.signals.stopped.load(Ordering::Acquire)
                || self.signals.answering.load(Ordering::Acquire)
            {
                offer.audible.stop();
                continue;
            }
            let authority = offer.audible.clone();
            let sound_authority = authority.clone();
            let stopped = self.signals.clone();
            let live = live.clone();
            let calls = calls.clone();
            // Every path below is an atomic read or immutable Instant comparison.
            // No watch/config lock, allocation, SDK call or logging on CPAL.
            let allowed = Arc::new(move || {
                sound_authority.allowed()
                    && !stopped.stopped.load(Ordering::Acquire)
                    && !stopped.answering.load(Ordering::Acquire)
                    && live.is_live.load(Ordering::Acquire)
                    && live.voice_spaces.ringing_policy_current(policy_epoch)
                    && live.security_gate.ringing_allowed(security_epoch)
                    && calls.ringing_allowed(call_epoch)
            });
            let output_runtime = output_runtime.clone();
            match RingPlayback::start_after_output_release(
                speaker,
                authority.deadline(),
                allowed,
                move || {
                    // Runs on the retained std ring owner, never a UI/Tokio
                    // worker. Ring reservation already retired old PCM. New PCM
                    // clicks fail that same atomic output authority, so this
                    // actual attachment-thread/TLS wait has no ring wait cycle.
                    output_runtime
                        .block_on(crate::audio_attachments::wait_for_release())
                        .map_err(|_| RingAudioReleaseError::OutputOwnerUnconfirmed)
                },
            ) {
                Ok(playback) => {
                    *current = Some(Playing {
                        token: offer.token,
                        authority,
                        playback,
                    })
                }
                // Sound failure never removes or weakens the visual offer. No
                // automatic retry, fallback device, raw error or identifier.
                Err(error) => {
                    authority.stop();
                    if error != RingAudioError::Cancelled {
                        self.unavailable(offer.token);
                    }
                }
            }
        }
    }
    fn unavailable(&self, token: ActionToken) {
        let mut current = self.unavailable.lock().unwrap_or_else(|e| e.into_inner());
        if *current != Some(token) {
            *current = Some(token);
            self.changed.send_replace(());
        }
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }
    pub(crate) fn notice(&self, visible: &[OfferPresentation]) -> &'static str {
        let failed = *self.unavailable.lock().unwrap_or_else(|e| e.into_inner());
        if visible.iter().any(|offer| Some(offer.token) == failed) {
            "Incoming call sound is unavailable. You can still answer."
        } else {
            ""
        }
    }
    fn stop_current(&self) {
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
    /// Called synchronously on Answer/Dismiss, before async command admission.
    pub(crate) fn silence(&self, offers: &IncomingHandle) {
        for offer in offers.snapshot() {
            offer.audible.stop();
        }
        self.stop_current();
    }
    pub(crate) fn begin_answer(&self, offers: &IncomingHandle) {
        self.signals.answering.store(true, Ordering::Release);
        self.silence(offers);
    }
    pub(crate) fn end_answer(&self) {
        self.signals.answering.store(false, Ordering::Release);
    }
    pub(crate) fn shutdown(&self) {
        self.signals.stopped.store(true, Ordering::Release);
        self.stop_current();
    }
}
impl Drop for IncomingAudio {
    fn drop(&mut self) {
        self.shutdown();
    }
}

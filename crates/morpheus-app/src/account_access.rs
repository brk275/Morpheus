//! Local app-password prompts and direct removal of saved Matrix accounts.
//! The core owns authorization; UI state only controls presentation.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use morpheus_matrix::{ActivationCancellation, ActivationChallenge, MatrixCoreError};
use slint::{ComponentHandle, Model, ModelRc, VecModel, Weak};
use tokio::runtime::Handle;
use zeroize::Zeroizing;

use crate::{AppWindow, SavedAccountItem, live::LiveUi};

enum Action {
    Unlock(String),
    Enable,
    Change,
    Disable,
    Reset,
    Remove(String),
}

struct Pending {
    action: Action,
    challenge: Option<ActivationChallenge>,
    cancellation: Option<ActivationCancellation>,
}

pub(crate) struct AccountAccessUi {
    live: Arc<LiveUi>,
    ui: Weak<AppWindow>,
    runtime: Handle,
    can_change: Arc<dyn Fn() -> bool + Send + Sync>,
    pending: Mutex<Option<Pending>>,
    generation: Arc<AtomicU64>,
    accounts_refresh_generation: Arc<AtomicU64>,
    switch_generation: AtomicU64,
    busy: AtomicBool,
}

// Declared before the transition/security guards in request(): its completion
// runs after those guards have dropped, including early returns or cancellation.
// The generation controls presentation only; core activation grants remain the
// authority for protected accounts.
struct SwitchCompletion {
    access: Arc<AccountAccessUi>,
    generation: u64,
}

impl Drop for SwitchCompletion {
    fn drop(&mut self) {
        self.access.busy.store(false, Ordering::Release);
        self.access.show_switch_pending(self.generation, false);
    }
}

impl AccountAccessUi {
    pub(crate) fn register(
        ui: &AppWindow,
        live: Arc<LiveUi>,
        runtime: Handle,
        can_change: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Arc<Self> {
        let this = Arc::new(Self {
            live,
            ui: ui.as_weak(),
            runtime,
            can_change: Arc::new(can_change),
            pending: Mutex::new(None),
            generation: Arc::new(AtomicU64::new(0)),
            accounts_refresh_generation: Arc::new(AtomicU64::new(0)),
            switch_generation: AtomicU64::new(0),
            busy: AtomicBool::new(false),
        });
        let access = this.clone();
        ui.on_activate_saved_account(move |id| access.request(id.to_string()));
        let access = this.clone();
        ui.on_manage_accounts(move || {
            let access = access.clone();
            access.runtime.clone().spawn(async move {
                access.refresh_accounts().await;
            });
        });
        let access = this.clone();
        ui.on_open_app_password(move |mode| {
            if access.busy.load(Ordering::Acquire) {
                return;
            }
            let action = match mode {
                1 => Action::Enable,
                2 => Action::Change,
                3 => Action::Disable,
                5 => Action::Reset,
                _ => return,
            };
            access.open(action, None, "".into());
        });
        let access = this.clone();
        ui.on_remove_saved_account(move |id| {
            if access.busy.load(Ordering::Acquire) {
                return;
            }
            let label = access
                .ui
                .upgrade()
                .and_then(|ui| {
                    ui.get_saved_accounts()
                        .iter()
                        .find(|a| a.id == id)
                        .map(|a| a.handle.to_string())
                })
                .unwrap_or_default();
            access.open(Action::Remove(id.to_string()), None, label);
        });
        let access = this.clone();
        ui.on_cancel_account_access(move || access.cancel());
        let access = this.clone();
        ui.on_submit_account_access(move || {
            let Some(ui) = access.ui.upgrade() else {
                return;
            };
            // Clear toolkit properties immediately. Rust-owned copies are zeroized on drop.
            let current = Zeroizing::new(ui.get_access_current_password().to_string());
            let new = Zeroizing::new(ui.get_access_new_password().to_string());
            let confirmation = Zeroizing::new(ui.get_access_confirm_password().to_string());
            clear_passwords(&ui);
            if matches!(ui.get_access_mode(), 1 | 2) && *new != *confirmation {
                ui.set_access_error("The new passwords do not match.".into());
                return;
            }
            access.submit(current, new);
        });
        this
    }

    fn allowed(&self) -> bool {
        !self.live.security_gate.blocked() && (self.can_change)()
    }

    fn report(&self, message: impl Into<String>) {
        let message = message.into();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            ui.set_account_access_notice(message.into());
        });
    }

    fn cancel(&self) {
        if let Some(ui) = self.ui.upgrade() {
            if self.busy.load(Ordering::Acquire) && ui.get_access_mode() != 0 {
                return;
            }
            clear_passwords(&ui);
            ui.set_access_open(false);
        }
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(pending) = self.pending.lock().unwrap().take()
            && let Some(cancellation) = pending.cancellation
        {
            cancellation.cancel();
        }
    }

    fn open(&self, action: Action, challenge: Option<ActivationChallenge>, label: String) {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let current_generation = self.generation.clone();
        let mode = match &action {
            Action::Unlock(_) => 0,
            Action::Enable => 1,
            Action::Change => 2,
            Action::Disable => 3,
            Action::Reset => 5,
            Action::Remove(_) => 4,
        };
        let cancellation = challenge.as_ref().map(ActivationChallenge::cancellation);
        if let Some(old) = self.pending.lock().unwrap().replace(Pending {
            action,
            challenge,
            cancellation,
        }) && let Some(cancellation) = old.cancellation
        {
            cancellation.cancel();
        }
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            if current_generation.load(Ordering::Acquire) != generation {
                return;
            }
            clear_passwords(&ui);
            ui.set_access_mode(mode);
            ui.set_access_target(label.into());
            ui.set_access_error("".into());
            ui.set_access_busy(false);
            ui.set_access_open(true);
            ui.set_login_loading(false);
            ui.set_account_menu_open(false);
            ui.invoke_cancel_room_settings();
            ui.invoke_close_image_preview();
        });
    }

    fn show_switch_pending(self: &Arc<Self>, generation: u64, pending: bool) {
        if let Some(ui) = self.ui.upgrade() {
            if self.switch_generation.load(Ordering::Acquire) == generation {
                ui.set_account_switch_pending(pending);
            }
        } else {
            let access = self.clone();
            let _ = self.ui.upgrade_in_event_loop(move |ui| {
                // A completed request must not re-enable a newer switch.
                if access.switch_generation.load(Ordering::Acquire) == generation {
                    ui.set_account_switch_pending(pending);
                }
            });
        }
    }

    pub(crate) fn request(self: &Arc<Self>, id: String) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let generation = self
            .switch_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.show_switch_pending(generation, true);
        let completion = SwitchCompletion {
            access: self.clone(),
            generation,
        };
        let access = self.clone();
        self.runtime.spawn(async move {
            // Captured before spawn so even an unpolled, dropped future clears
            // presentation; initialized before either operation guard below.
            let _completion = completion;
            let _transition = access.live.account_transition.lock().await;
            if !access.allowed() {
                access.report(
                    "Leave the call and finish any security operation before changing accounts.",
                );
                return;
            }
            let Some(_security) = access.live.security_gate.try_account_access() else {
                access.report("Finish the current security operation before changing accounts.");
                return;
            };
            if let Err(error) = access.live.flush_drafts().await {
                access.report(error.to_string());
                return;
            }
            let result = access.live.manager.switch_to(&id).await;
            match result {
                Ok(_) => {
                    if let Err(error) = access.live.account_activated(access.ui.clone()).await {
                        access.report(error.to_string());
                    }
                }
                Err(MatrixCoreError::AppPasswordRequired) => {
                    match access.live.manager.begin_activation(&id).await {
                        Ok(challenge) => {
                            let label = access
                                .live
                                .manager
                                .accounts()
                                .await
                                .into_iter()
                                .find(|a| a.id == id)
                                .map(|a| a.user_id)
                                .unwrap_or_default();
                            access.open(Action::Unlock(id), Some(challenge), label);
                        }
                        Err(error) => access.report(error.to_string()),
                    }
                }
                Err(error) => access.report(error.to_string()),
            }
            if access.live.is_live.load(Ordering::Acquire)
                && access.live.manager.active_account().await.is_none()
            {
                access.live.account_detached(access.ui.clone()).await;
            }
            access.refresh_accounts().await;
        });
    }

    pub(crate) async fn restore(self: &Arc<Self>) {
        self.refresh_accounts().await;
        if let Some(id) = self.live.manager.last_active_account_id().await {
            self.request(id);
        }
        let _ = self
            .ui
            .upgrade_in_event_loop(|ui| ui.set_login_loading(false));
    }

    pub(crate) async fn refresh_accounts(&self) {
        let generation = self
            .accounts_refresh_generation
            .fetch_add(1, Ordering::AcqRel)
            + 1;
        let accounts = self.live.manager.accounts().await;
        let status = self.live.manager.app_password_status().await;
        let active = self.live.manager.active_account().await;
        let current_generation = self.accounts_refresh_generation.clone();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            // An earlier Manage refresh must not reinsert a row after removal.
            if current_generation.load(Ordering::Acquire) != generation {
                return;
            }
            // Generated rows contain a Slint Image even before an avatar is
            // attached. Construct the whole row on its UI thread; only the
            // core account metadata crosses from the worker.
            let rows = accounts
                .into_iter()
                .map(|account| SavedAccountItem {
                    id: account.id.as_str().into(),
                    handle: account.user_id.into(),
                    active: active.as_ref().is_some_and(|a| a.id == account.id),
                    removal_pending: status.pending_removals.contains(&account.id),
                    ..Default::default()
                })
                .map(crate::account_identity::with_saved_avatar)
                .collect::<Vec<_>>();
            ui.set_saved_accounts(ModelRc::new(VecModel::from(rows)));
            ui.set_app_password_enabled(status.enabled);
        });
    }

    fn submit(self: &Arc<Self>, current: Zeroizing<String>, new: Zeroizing<String>) {
        if self.busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let generation = self.generation.load(Ordering::Acquire);
        let pending = self.pending.lock().unwrap().take();
        let Some(mut pending) = pending else {
            self.busy.store(false, Ordering::Release);
            return;
        };
        // Keep cancellation reachable during the KDF and account-store open.
        let action = match &pending.action {
            Action::Unlock(id) => Action::Unlock(id.clone()),
            Action::Enable => Action::Enable,
            Action::Change => Action::Change,
            Action::Disable => Action::Disable,
            Action::Reset => Action::Reset,
            Action::Remove(id) => Action::Remove(id.clone()),
        };
        let challenge = pending.challenge.take();
        *self.pending.lock().unwrap() = Some(pending);
        if let Some(ui) = self.ui.upgrade() {
            ui.set_access_busy(true);
            ui.set_access_error("".into());
        }
        let access = self.clone();
        self.runtime.spawn(async move {
            let result: Result<String, MatrixCoreError> = async {
                if !access.allowed() { return Err(MatrixCoreError::SelectedAccountChanged); }
                let _security = access.live.security_gate.try_account_access().ok_or(MatrixCoreError::SelectedAccountChanged)?;
                match action {
                    Action::Unlock(_) => {
                        let challenge = challenge.ok_or(MatrixCoreError::ActivationExpired)?;
                        let grant = access.live.manager.authorize_activation(challenge, current).await?;
                        let _transition = access.live.account_transition.lock().await;
                        if access.generation.load(Ordering::Acquire) != generation || !(access.can_change)() {
                            return Err(MatrixCoreError::ActivationExpired);
                        }
                        access.live.flush_drafts().await?;
                        if let Err(error) = access.live.manager.activate(grant).await {
                            if access.live.manager.active_account().await.is_none() {
                                access.live.account_detached(access.ui.clone()).await;
                            }
                            return Err(error);
                        }
                        access.live.account_activated(access.ui.clone()).await?;
                        Ok(String::new())
                    }
                    Action::Enable | Action::Change | Action::Disable | Action::Reset => {
                        let _transition = access.live.account_transition.lock().await;
                        if !(access.can_change)() { return Err(MatrixCoreError::SelectedAccountChanged); }
                        match action {
                            Action::Enable => access.live.manager.set_app_password(None, new).await?,
                            Action::Change => access.live.manager.set_app_password(Some(current), new).await?,
                            Action::Disable => access.live.manager.disable_app_password(current).await?,
                            Action::Reset => access.live.manager.reset_app_password_after_removal().await?,
                            _ => unreachable!(),
                        }
                        Ok("App password settings saved.".into())
                    }
                    Action::Remove(id) => {
                        let _transition = access.live.account_transition.lock().await;
                        if !(access.can_change)() { return Err(MatrixCoreError::SelectedAccountChanged); }
                        if access.live.manager.active_account().await.is_some_and(|a| a.id == id) {
                            access.live.clear_feed().await;
                        }
                        access.live.voice_spaces.forget_account(&id).await;
                        let result = access.live.manager.remove_account(&id).await;
                        // A partial removal may already have revoked the active account.
                        if access.live.manager.active_account().await.is_none() {
                            access.live.account_detached(access.ui.clone()).await;
                        } else {
                            access.live.refresh_account_menu(access.ui.clone()).await;
                        }
                        if result.is_err() && access.live.manager.active_account().await.is_some() {
                            // Tombstone persistence may fail before any revocation.
                            // Restore the still-authorized account, never present it as removed.
                            access.live.account_activated(access.ui.clone()).await?;
                        }
                        let result = result?;
                        access.live.forget_account_drafts(&id);
                        access.live.voice_spaces.forget_account(&id).await;
                        if result.complete {
                            if let Err(error) = access.live.voice_spaces.clear_logged_out_account(&id) {
                                return Ok(format!("Account removed. Some appearance preferences could not be removed: {error}"));
                            }
                            Ok("Removed from this app. The homeserver session was not logged out; you can revoke that device from another client.".into())
                        } else {
                            Ok("Removal is incomplete. The account is blocked from use. Choose Retry removal; if files are still open, restart Morpheus first.".into())
                        }
                    }
                }
            }.await;
            access.busy.store(false, Ordering::Release);
            if access.generation.load(Ordering::Acquire) != generation { return; }
            let retry_id = {
                let mut pending = access.pending.lock().unwrap();
                if result.is_err() {
                    pending.as_ref().and_then(|p| match &p.action { Action::Unlock(id) => Some(id.clone()), _ => None })
                } else { pending.take(); None }
            };
            // A failed attempt consumes its grant; issue a new challenge for the exact same target.
            if let Some(id) = retry_id {
                if let Ok(challenge) = access.live.manager.begin_activation(&id).await {
                    if access.generation.load(Ordering::Acquire) == generation {
                        if let Some(pending) = access.pending.lock().unwrap().as_mut() {
                            pending.cancellation = Some(challenge.cancellation());
                            pending.challenge = Some(challenge);
                        }
                    }
                }
            }
            let current_access = access.clone();
            let _ = access.ui.upgrade_in_event_loop(move |ui| {
                if current_access.generation.load(Ordering::Acquire) != generation { return; }
                ui.set_access_busy(false);
                match result {
                    Ok(notice) => { ui.set_access_open(false); ui.set_account_access_notice(notice.into()); }
                    Err(error) => ui.set_access_error(error.to_string().into()),
                }
            });
            access.refresh_accounts().await;
        });
    }
}

fn clear_passwords(ui: &AppWindow) {
    ui.set_access_current_password("".into());
    ui.set_access_new_password("".into());
    ui.set_access_confirm_password("".into());
}

#[cfg(test)]
#[path = "account_access_ui_tests.rs"]
mod ui_tests;

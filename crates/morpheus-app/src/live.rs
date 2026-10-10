//! Thin presentation adapter between the account-scoped Matrix core and Slint.
//! All Matrix work runs on Tokio; UI models are created on Slint's event loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use morpheus_matrix::{
    AccountInfo, AccountManager, DownloadedAttachment, MatrixCoreError, MatrixLinkTarget,
    MessageInfo, MessageSendState, QueuedAttachment, QueuedMessage, RoomInfo, RoomMemberInfo,
    RoomMembership, SyncBackend, SyncStatus, TextFormat,
};
use slint::{Color, Model, ModelRc, StyledText, VecModel, Weak};
use tokio::{
    runtime::Handle,
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

use crate::attachments::save_download;
use crate::drafts::DraftStore;
use crate::voice_spaces::VoiceSpaces;
use crate::{
    AccountItem, AccountOption, AppWindow, MemberItem, MessageItem, PublicRoomItem, RoomItem,
    SpaceItem,
};

pub enum MessageTarget {
    Room,
    Reply(String),
    Edit(String),
    Thread(String),
}

#[derive(Clone, Copy)]
pub enum RoomAction {
    CreatePrivate,
    CreateSpace,
}

struct KnownSpace {
    name: String,
    recorded_at: Instant,
}

struct MemberRequest {
    account_id: String,
    room_id: String,
    started: Instant,
    epoch: u64,
}

pub struct LiveUi {
    pub manager: Arc<AccountManager>,
    pub(crate) voice_spaces: Arc<VoiceSpaces>,
    pub is_live: Arc<AtomicBool>,
    pub(crate) security_gate: Arc<crate::security::SecurityGate>,
    drafts: Arc<DraftStore>,
    draft_write_tx: mpsc::Sender<()>,
    generation: Arc<AtomicU64>,
    render_epoch: Arc<AtomicU64>,
    navigation_epoch: Arc<AtomicU64>,
    member_epoch: Arc<AtomicU64>,
    member_request: Mutex<Option<MemberRequest>>,
    feed: Mutex<Option<JoinHandle<()>>>,
    feed_key: Mutex<Option<(String, String)>>,
    thread_feed: Mutex<Option<JoinHandle<()>>>,
    thread_generation: Arc<AtomicU64>,
    selected_room: Mutex<Option<String>>,
    selected_space: Mutex<Option<String>>,
    known_spaces: Mutex<HashMap<String, HashMap<String, KnownSpace>>>,
    pub(crate) account_transition: Mutex<()>,
    space_children: Mutex<HashMap<String, HashSet<String>>>,
    space_scan_key: Mutex<Option<(String, Vec<String>)>>,
    space_scan_failed_at: Mutex<Option<Instant>>,
    space_scan_epoch: AtomicU64,
}

impl LiveUi {
    pub fn new(
        manager: Arc<AccountManager>,
        runtime: Handle,
        ui: Weak<AppWindow>,
        voice_spaces: Arc<VoiceSpaces>,
    ) -> Arc<Self> {
        let (draft_write_tx, mut draft_write_rx) = mpsc::channel(1);
        let drafts = Arc::new(DraftStore::default());
        let draft_worker_store = drafts.clone();
        let draft_worker_manager = manager.clone();
        runtime.spawn(async move {
            let mut error_reported = false;
            while draft_write_rx.recv().await.is_some() {
                // A single bounded worker coalesces typing bursts. Account
                // switches and app shutdown call flush_drafts directly.
                let mut closed = false;
                loop {
                    match tokio::time::timeout(Duration::from_millis(250), draft_write_rx.recv())
                        .await
                    {
                        Ok(Some(())) => {}
                        Ok(None) => {
                            closed = true;
                            break;
                        }
                        Err(_) => break,
                    }
                }
                let mut failed = false;
                for (account_id, room_id) in draft_worker_store.dirty_keys() {
                    match draft_worker_store
                        .persist_one(&draft_worker_manager, &account_id, &room_id)
                        .await
                    {
                        Ok(()) => {}
                        Err(MatrixCoreError::AccountNotFound(_)) => {}
                        Err(error) => {
                            failed = true;
                            if !error_reported {
                                show_error(
                                    &ui,
                                    format!("Could not save draft on this device: {error}"),
                                );
                            }
                        }
                    }
                }
                error_reported = failed;
                if closed {
                    break;
                }
            }
        });
        Arc::new(Self {
            manager,
            voice_spaces,
            is_live: Arc::new(AtomicBool::new(false)),
            security_gate: Arc::new(crate::security::SecurityGate::default()),
            drafts,
            draft_write_tx,
            generation: Arc::new(AtomicU64::new(0)),
            render_epoch: Arc::new(AtomicU64::new(0)),
            navigation_epoch: Arc::new(AtomicU64::new(0)),
            member_epoch: Arc::new(AtomicU64::new(0)),
            member_request: Mutex::new(None),
            feed: Mutex::new(None),
            feed_key: Mutex::new(None),
            thread_feed: Mutex::new(None),
            thread_generation: Arc::new(AtomicU64::new(0)),
            selected_room: Mutex::new(None),
            selected_space: Mutex::new(None),
            known_spaces: Mutex::new(HashMap::new()),
            account_transition: Mutex::new(()),
            space_children: Mutex::new(HashMap::new()),
            space_scan_key: Mutex::new(None),
            space_scan_failed_at: Mutex::new(None),
            space_scan_epoch: AtomicU64::new(0),
        })
    }

    /// Cancel queued presentation work as soon as the user requests a new
    /// account, room, or Space, before the async operation starts running.
    pub fn invalidate_navigation(&self) -> u64 {
        self.render_epoch.fetch_add(1, Ordering::AcqRel);
        self.member_epoch.fetch_add(1, Ordering::AcqRel);
        self.navigation_epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(crate) fn current_navigation_epoch(&self) -> u64 {
        self.navigation_epoch.load(Ordering::Acquire)
    }

    pub fn remember_draft(&self, account_id: &str, room_id: &str, body: &str) {
        if !account_id.is_empty() && !room_id.is_empty() {
            self.drafts.set(account_id, room_id, body);
            self.queue_draft_write();
        }
    }

    pub fn remember_markdown(&self, account_id: &str, room_id: &str, enabled: bool) {
        if !account_id.is_empty() && !room_id.is_empty() {
            self.drafts.set_markdown(account_id, room_id, enabled);
        }
    }

    pub fn draft_revision(&self, account_id: &str, room_id: &str) -> Option<u64> {
        self.drafts.revision(account_id, room_id)
    }

    pub fn mark_queued_draft(
        &self,
        account_id: &str,
        room_id: &str,
        submitted: &str,
        expected_revision: u64,
        transaction_id: &str,
    ) -> bool {
        self.drafts.mark_queued(
            account_id,
            room_id,
            submitted,
            expected_revision,
            transaction_id,
        )
    }

    pub fn resolve_queued_draft(
        &self,
        account_id: &str,
        room_id: &str,
        transaction_id: &str,
        sent: bool,
    ) -> bool {
        let changed = self
            .drafts
            .resolve_queued(account_id, room_id, transaction_id, sent);
        if changed && sent {
            self.queue_draft_write();
        }
        changed
    }

    pub fn forget_account_drafts(&self, account_id: &str) {
        self.drafts.forget_account(account_id);
    }

    pub async fn flush_drafts(&self) -> Result<(), MatrixCoreError> {
        self.drafts.flush_dirty(&self.manager).await
    }

    fn queue_draft_write(&self) {
        // One queued signal is enough: the worker scans the latest dirty
        // cache, coalescing fast typing without creating a task per keypress.
        let _ = self.draft_write_tx.try_send(());
    }

    pub async fn restore(self: &Arc<Self>, ui: Weak<AppWindow>) {
        match self.manager.restore_last_active().await {
            Ok(Some(_)) => {
                self.is_live.store(true, Ordering::Release);
                if let Err(error) = self.refresh(ui.clone()).await {
                    crate::show_login_error(&ui, error.to_string());
                } else {
                    self.clone().scan_spaces(ui).await;
                }
            }
            Ok(None) => {
                let _ = ui.upgrade_in_event_loop(|ui| ui.set_login_loading(false));
            }
            Err(error) => crate::show_login_error(&ui, format!("Sign in again: {error}")),
        }
    }

    pub async fn select_account(
        self: &Arc<Self>,
        index: usize,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        self.select_account_guarded(index, ui, || true).await?;
        Ok(())
    }

    /// Check the call switch gate while holding the same transition lock that
    /// call entry uses. A check before awaiting this lock can become stale.
    pub async fn select_account_guarded(
        self: &Arc<Self>,
        index: usize,
        ui: Weak<AppWindow>,
        can_switch: impl FnOnce() -> bool,
    ) -> Result<bool, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        if !can_switch() {
            return Ok(false);
        }
        self.invalidate_navigation();
        let accounts = self.manager.accounts().await;
        let account = accounts
            .get(index)
            .ok_or_else(|| MatrixCoreError::AccountNotFound(index.to_string()))?;
        self.flush_drafts().await?;
        self.clear_feed().await;
        self.manager.switch_to(&account.id).await?;
        *self.selected_room.lock().await = None;
        *self.selected_space.lock().await = None;
        self.invalidate_space_scan().await;
        self.space_children.lock().await.clear();
        self.is_live.store(true, Ordering::Release);
        self.refresh(ui.clone()).await?;
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(true)
    }

    /// Keep an action tied to the account visible when it was submitted. The
    /// same guard covers account switches, login, and logout in the app.
    pub async fn run_room_action(
        self: &Arc<Self>,
        account_id: &str,
        action: RoomAction,
        input: &str,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        self.require_active_account(account_id).await?;
        let input = input.trim();
        if input.is_empty() {
            return Err(MatrixCoreError::InvalidRegistry(
                "Enter a room or Space name".into(),
            ));
        }
        let room_id = match action {
            RoomAction::CreatePrivate => self.manager.create_private_room(input).await?,
            RoomAction::CreateSpace => self.manager.create_space(input).await?,
        };
        self.complete_room_action(
            account_id,
            room_id,
            Some(matches!(action, RoomAction::CreateSpace)),
            input,
            ui,
        )
        .await
    }

    /// Join a parsed room ID, alias, or Matrix link and navigate to it. The
    /// directory may already know this is a Space before local sync does.
    pub async fn join_matrix_target(
        self: &Arc<Self>,
        account_id: &str,
        target: &MatrixLinkTarget,
        space_hint: Option<bool>,
        display_name: &str,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        self.require_active_account(account_id).await?;
        let room_id = self
            .manager
            .join_matrix_room_for(account_id, target)
            .await?;
        self.complete_room_action(account_id, room_id, space_hint, display_name, ui)
            .await
    }

    async fn complete_room_action(
        self: &Arc<Self>,
        account_id: &str,
        room_id: String,
        space_hint: Option<bool>,
        display_name: &str,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        self.require_active_account(account_id).await?;
        let is_space = match space_hint {
            Some(true) => true,
            Some(false) | None => self
                .manager
                .rooms_snapshot()
                .await?
                .iter()
                .any(|room| room.room_id == room_id && room.is_space),
        };
        if is_space {
            self.known_spaces
                .lock()
                .await
                .entry(account_id.to_owned())
                .or_default()
                .insert(
                    room_id.clone(),
                    KnownSpace {
                        name: if display_name.is_empty() {
                            room_id.clone()
                        } else {
                            display_name.to_owned()
                        },
                        recorded_at: Instant::now(),
                    },
                );
        }
        self.clear_feed().await;
        if is_space {
            *self.selected_space.lock().await = Some(room_id);
            *self.selected_room.lock().await = None;
        } else {
            *self.selected_space.lock().await = None;
            *self.selected_room.lock().await = Some(room_id);
        }
        self.invalidate_space_scan().await;
        self.invalidate_navigation();
        self.refresh(ui.clone()).await?;
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(())
    }

    pub async fn respond_to_invite(
        self: &Arc<Self>,
        account_id: &str,
        room_id: &str,
        accept: bool,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        self.require_active_account(account_id).await?;
        if accept {
            // Invited Spaces carry the room type in their stripped state.
            // Check before joining: a newly joined room may not yet have its
            // full create event in the local room snapshot.
            let invite = self
                .manager
                .invites_snapshot()
                .await?
                .into_iter()
                .find(|room| room.room_id == room_id);
            let was_space = invite.as_ref().is_some_and(|room| room.is_space);
            self.manager.accept_invite(room_id).await?;
            let is_space = was_space
                || self
                    .manager
                    .rooms_snapshot()
                    .await?
                    .iter()
                    .any(|room| room.room_id == room_id && room.is_space);
            if is_space {
                self.known_spaces
                    .lock()
                    .await
                    .entry(account_id.to_owned())
                    .or_default()
                    .insert(
                        room_id.to_owned(),
                        KnownSpace {
                            name: invite
                                .as_ref()
                                .map_or_else(|| room_id.to_owned(), |room| room.name.clone()),
                            recorded_at: Instant::now(),
                        },
                    );
            }
            self.clear_feed().await;
            if is_space {
                *self.selected_space.lock().await = Some(room_id.to_owned());
                *self.selected_room.lock().await = None;
            } else {
                *self.selected_space.lock().await = None;
                *self.selected_room.lock().await = Some(room_id.to_owned());
            }
        } else {
            self.manager.reject_invite(room_id).await?;
        }
        self.require_active_account(account_id).await?;
        self.invalidate_space_scan().await;
        self.invalidate_navigation();
        self.refresh(ui.clone()).await?;
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(())
    }

    async fn require_active_account(&self, account_id: &str) -> Result<(), MatrixCoreError> {
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .is_none_or(|active| active.id != account_id)
        {
            Err(MatrixCoreError::SelectedAccountChanged)
        } else {
            Ok(())
        }
    }

    pub async fn select_room_for(
        self: &Arc<Self>,
        account_id: &str,
        room_id: String,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        if !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
        {
            return Ok(false);
        }
        self.clear_feed().await;
        {
            let mut selected = self.selected_room.lock().await;
            if !self.navigation_is_current(navigation_epoch) {
                return Ok(false);
            }
            *selected = Some(room_id);
        }
        if !self.navigation_is_current(navigation_epoch) {
            return Ok(false);
        }
        self.refresh(ui).await?;
        Ok(true)
    }

    pub async fn select_space_for(
        self: &Arc<Self>,
        account_id: &str,
        space_id: String,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) -> Result<bool, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        if !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
        {
            return Ok(false);
        }
        if !space_id.is_empty() {
            let rooms = self.manager.rooms_snapshot().await?;
            let known = self
                .known_spaces
                .lock()
                .await
                .get(account_id)
                .is_some_and(|spaces| spaces.contains_key(&space_id));
            if !known
                && !rooms
                    .iter()
                    .any(|room| room.is_space && room.room_id == space_id)
            {
                return Err(MatrixCoreError::RoomNotFound(space_id));
            }
        }
        if !self.navigation_is_current(navigation_epoch) {
            return Ok(false);
        }
        *self.selected_space.lock().await = (!space_id.is_empty()).then_some(space_id);
        *self.selected_room.lock().await = None;
        self.clear_feed().await;
        if !self.navigation_is_current(navigation_epoch) {
            return Ok(false);
        }
        self.refresh(ui).await?;
        Ok(true)
    }

    pub(crate) fn navigation_is_current(&self, navigation_epoch: u64) -> bool {
        self.is_live.load(Ordering::Acquire)
            && self.navigation_epoch.load(Ordering::Acquire) == navigation_epoch
    }

    pub async fn refresh(self: &Arc<Self>, ui: Weak<AppWindow>) -> Result<(), MatrixCoreError> {
        if !self.is_live.load(Ordering::Acquire) {
            return Ok(());
        }
        let render_epoch = self.render_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let navigation_epoch = self.navigation_epoch.load(Ordering::Acquire);
        let accounts = self.manager.accounts().await;
        let active = self
            .manager
            .active_account()
            .await
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        let rooms = self.manager.rooms_snapshot().await?;
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .map(|account| &account.id)
            != Some(&active.id)
            || self.render_epoch.load(Ordering::Acquire) != render_epoch
        {
            return Ok(());
        }
        let known_spaces = {
            let mut by_account = self.known_spaces.lock().await;
            let known = by_account.entry(active.id.clone()).or_default();
            known.retain(
                |id, value| match rooms.iter().find(|room| room.room_id == *id) {
                    Some(room) if room.is_space && room.membership == RoomMembership::Joined => {
                        false
                    }
                    _ => value.recorded_at.elapsed() < Duration::from_secs(60),
                },
            );
            known
                .iter()
                .map(|(id, value)| (id.clone(), value.name.clone()))
                .collect::<HashMap<_, _>>()
        };
        let selected_space = self.selected_space.lock().await.clone().filter(|space_id| {
            known_spaces.contains_key(space_id)
                || rooms.iter().any(|room| {
                    room.is_space
                        && room.room_id == *space_id
                        && room.membership == RoomMembership::Joined
                })
        });
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(());
        }
        *self.selected_space.lock().await = selected_space.clone();
        let space_children = self.space_children.lock().await.clone();
        let all_space_children = space_children
            .values()
            .flatten()
            .cloned()
            .collect::<HashSet<_>>();
        let visible = |room: &RoomInfo| {
            if room.is_space
                || known_spaces.contains_key(&room.room_id)
                || room.membership != RoomMembership::Joined
            {
                return false;
            }
            if let Some(space_id) = selected_space.as_deref() {
                space_children
                    .get(space_id)
                    .is_some_and(|children| children.contains(&room.room_id))
            } else {
                room.is_direct || !all_space_children.contains(&room.room_id)
            }
        };
        let current = self.selected_room.lock().await.clone();
        let selected = current
            .filter(|room_id| {
                rooms
                    .iter()
                    .any(|room| room.room_id == *room_id && visible(room))
            })
            .or_else(|| {
                rooms
                    .iter()
                    .find(|room| room.membership == RoomMembership::Joined && visible(room))
                    .map(|room| room.room_id.clone())
            });
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(());
        }
        *self.selected_room.lock().await = selected.clone();
        if let Some(room_id) = selected.as_deref() {
            if let Err(error) = self
                .drafts
                .load_if_missing(&self.manager, &active.id, room_id)
                .await
            {
                show_error(
                    &ui,
                    format!("Could not restore draft on this device: {error}"),
                );
            }
        }
        if self.render_epoch.load(Ordering::Acquire) != render_epoch {
            return Ok(());
        }
        let member_account_id = active.id.clone();
        let marker_rooms = rooms
            .iter()
            .filter(|room| room.membership == RoomMembership::Joined && !room.is_space)
            .map(|room| room.room_id.clone())
            .collect::<Vec<_>>();
        let view = Snapshot {
            accounts,
            active,
            rooms,
            selected: selected.clone(),
            selected_space,
            known_spaces,
            space_children,
            all_space_children,
            local_voice_rooms: self.voice_spaces.local_rooms(&member_account_id),
            legacy_element_call_rooms: self
                .voice_spaces
                .legacy_element_call_rooms(&member_account_id),
            shared_voice_rooms: self.voice_spaces.shared_rooms(&member_account_id).await,
        };
        let current_epoch = self.render_epoch.clone();
        let is_live = self.is_live.clone();
        let drafts = self.drafts.clone();
        let security_gate = self.security_gate.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if is_live.load(Ordering::Acquire)
                && current_epoch.load(Ordering::Acquire) == render_epoch
            {
                apply_snapshot(&ui, view, &drafts, &security_gate);
            }
        });
        if let Some(room_id) = selected {
            self.refresh_members_for(
                member_account_id.clone(),
                room_id.clone(),
                ui.clone(),
                false,
            )
            .await;
            self.open_room(&member_account_id, room_id, navigation_epoch, ui.clone())
                .await;
        }
        let marker_account = member_account_id;
        let voice_spaces = self.voice_spaces.clone();
        let live = self.clone();
        tokio::spawn(async move {
            if voice_spaces
                .scan_shared(&live.manager, &marker_account, &marker_rooms)
                .await
            {
                let shared_rooms = voice_spaces.shared_rooms(&marker_account).await;
                let _ = ui.upgrade_in_event_loop(move |window| {
                    if window.get_active_account_local_id().as_str() == marker_account {
                        apply_shared_voice_labels(&window, &shared_rooms);
                    }
                });
            }
        });
        Ok(())
    }

    /// Fetch members for the visible account and room. Sync can trigger many
    /// presentation refreshes, so refresh at most once per short interval
    /// unless the user explicitly requests it.
    pub async fn refresh_members_for(
        self: &Arc<Self>,
        account_id: String,
        room_id: String,
        ui: Weak<AppWindow>,
        force: bool,
    ) {
        if !self.is_live.load(Ordering::Acquire)
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
            || self
                .manager
                .active_account()
                .await
                .is_none_or(|account| account.id != account_id)
        {
            return;
        }
        let epoch = {
            let mut request = self.member_request.lock().await;
            if !force
                && request.as_ref().is_some_and(|previous| {
                    previous.account_id == account_id
                        && previous.room_id == room_id
                        && previous.epoch == self.member_epoch.load(Ordering::Acquire)
                        && previous.started.elapsed() < Duration::from_secs(30)
                })
            {
                return;
            }
            let epoch = self.member_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            *request = Some(MemberRequest {
                account_id: account_id.clone(),
                room_id: room_id.clone(),
                started: Instant::now(),
                epoch,
            });
            epoch
        };
        let loading_account = account_id.clone();
        let loading_room = room_id.clone();
        let _ = ui.upgrade_in_event_loop(move |window| {
            if window.get_active_account_local_id().as_str() == loading_account
                && window.get_active_room_id().as_str() == loading_room
            {
                window.set_member_loading(true);
                window.set_member_error("".into());
            }
        });
        let live = self.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                live.manager.room_members_for(&account_id, &room_id),
            )
            .await;
            if !live.is_live.load(Ordering::Acquire)
                || live.member_epoch.load(Ordering::Acquire) != epoch
                || live.selected_room.lock().await.as_deref() != Some(room_id.as_str())
                || live
                    .manager
                    .active_account()
                    .await
                    .is_none_or(|account| account.id != account_id)
            {
                return;
            }
            let (items, error) = match result {
                Ok(Ok(members)) => (
                    Some(members.iter().map(member_item).collect::<Vec<_>>()),
                    None,
                ),
                Ok(Err(error)) => (
                    None,
                    Some(crate::directory_plain_text(
                        &format!("Could not load members: {error}"),
                        240,
                    )),
                ),
                Err(_) => (
                    None,
                    Some("Loading members timed out. Retry to refresh.".to_owned()),
                ),
            };
            let _ = ui.upgrade_in_event_loop(move |window| {
                if live.member_epoch.load(Ordering::Acquire) != epoch
                    || window.get_active_account_local_id().as_str() != account_id
                    || window.get_active_room_id().as_str() != room_id
                {
                    return;
                }
                window.set_member_loading(false);
                if let Some(items) = items {
                    window.set_members(ModelRc::new(VecModel::from(items)));
                    window.set_member_error("".into());
                } else if let Some(error) = error {
                    window.set_member_error(error.into());
                }
            });
        });
    }

    /// Load the account's Space graph after showing its cached rooms. A room
    /// may be present in several returned sets; Home excludes their union.
    pub async fn scan_spaces(self: Arc<Self>, ui: Weak<AppWindow>) {
        if !self.is_live.load(Ordering::Acquire) {
            return;
        }
        let Some(account) = self.manager.active_account().await else {
            return;
        };
        let rooms = match self.manager.rooms_snapshot().await {
            Ok(rooms) => rooms,
            Err(error) => {
                show_error(&ui, error.to_string());
                return;
            }
        };
        let known_ids = self
            .known_spaces
            .lock()
            .await
            .get(&account.id)
            .map(|spaces| spaces.keys().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();
        let mut space_ids = rooms
            .iter()
            .filter(|room| {
                (room.is_space || known_ids.contains(&room.room_id))
                    && room.membership == RoomMembership::Joined
            })
            .map(|room| room.room_id.clone())
            .collect::<Vec<_>>();
        space_ids.sort();
        let key = (account.id.clone(), space_ids.clone());
        {
            let mut scanned = self.space_scan_key.lock().await;
            if scanned.as_ref() == Some(&key) {
                let failure = *self.space_scan_failed_at.lock().await;
                if failure.is_none_or(|at| at.elapsed() < Duration::from_secs(30)) {
                    return;
                }
            }
            *scanned = Some(key);
        }
        let scan_epoch = self.space_scan_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        *self.space_scan_failed_at.lock().await = None;
        let mut graph = HashMap::new();
        let mut failed = false;
        for space_id in space_ids {
            if self
                .manager
                .active_account()
                .await
                .is_none_or(|active| active.id != account.id)
            {
                return;
            }
            let mut descendants = HashSet::new();
            let mut from = None;
            for _ in 0..20 {
                let result = tokio::time::timeout(
                    Duration::from_secs(10),
                    self.manager
                        .space_hierarchy(&space_id, from.as_deref(), 100),
                )
                .await;
                let page = match result {
                    Ok(Ok(page)) => page,
                    Ok(Err(error)) => {
                        show_error(&ui, format!("Space {}: {error}", space_id));
                        failed = true;
                        break;
                    }
                    Err(_) => {
                        show_error(&ui, format!("Space {} timed out", space_id));
                        failed = true;
                        break;
                    }
                };
                for node in page.nodes {
                    if node.room_id != space_id && !node.is_space {
                        descendants.insert(node.room_id);
                    }
                }
                from = page.next_batch;
                if from.is_none() {
                    break;
                }
            }
            graph.insert(space_id, descendants);
            if failed {
                break;
            }
        }
        if self.space_scan_epoch.load(Ordering::Acquire) != scan_epoch {
            return;
        }
        if failed {
            *self.space_scan_failed_at.lock().await = Some(Instant::now());
            return;
        }
        if self
            .manager
            .active_account()
            .await
            .is_none_or(|active| active.id != account.id)
        {
            return;
        }
        *self.space_children.lock().await = graph;
        if let Err(error) = self.refresh(ui.clone()).await {
            show_error(&ui, error.to_string());
        }
    }

    async fn invalidate_space_scan(&self) {
        self.space_scan_epoch.fetch_add(1, Ordering::AcqRel);
        *self.space_scan_key.lock().await = None;
        *self.space_scan_failed_at.lock().await = None;
    }

    async fn open_room(
        self: &Arc<Self>,
        account_id: &str,
        room_id: String,
        navigation_epoch: u64,
        ui: Weak<AppWindow>,
    ) {
        if !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
        {
            return;
        }
        let requested_key = (account_id.to_owned(), room_id.clone());
        {
            let key = self.feed_key.lock().await;
            let feed = self.feed.lock().await;
            if key.as_ref() == Some(&requested_key)
                && feed.as_ref().is_some_and(|task| !task.is_finished())
            {
                return;
            }
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let feed = match self.manager.message_feed(&room_id).await {
            Ok(feed) => feed,
            Err(error) => {
                if self.navigation_is_current(navigation_epoch)
                    && self.require_active_account(account_id).await.is_ok()
                {
                    show_error(&ui, error.to_string());
                }
                return;
            }
        };
        if self.generation.load(Ordering::Acquire) != generation
            || !self.navigation_is_current(navigation_epoch)
            || self.require_active_account(account_id).await.is_err()
            || self.selected_room.lock().await.as_deref() != Some(room_id.as_str())
        {
            return;
        }
        if let Some(previous) = self.feed.lock().await.take() {
            previous.abort();
        }
        *self.feed_key.lock().await = Some(requested_key);
        let mut updates = feed.subscribe();
        let feed_generation = self.generation.clone();
        let feed_navigation_epoch = self.navigation_epoch.clone();
        let is_live = self.is_live.clone();
        let feed_account_id = account_id.to_owned();
        let feed_room_id = room_id;
        let handle = tokio::spawn(async move {
            let _feed = feed;
            loop {
                let messages = updates.borrow_and_update().clone();
                let ui_for_update = ui.clone();
                let current_generation = feed_generation.clone();
                let current_navigation_epoch = feed_navigation_epoch.clone();
                let live_flag = is_live.clone();
                let account_id = feed_account_id.clone();
                let room_id = feed_room_id.clone();
                let _ = ui_for_update.upgrade_in_event_loop(move |ui| {
                    if live_flag.load(Ordering::Acquire)
                        && current_generation.load(Ordering::Acquire) == generation
                        && current_navigation_epoch.load(Ordering::Acquire) == navigation_epoch
                        && visible_feed_target(
                            ui.get_active_account_local_id().as_str(),
                            ui.get_active_room_id().as_str(),
                            &account_id,
                            &room_id,
                        )
                    {
                        ui.set_messages(ModelRc::new(VecModel::from(
                            messages.iter().map(message_item).collect::<Vec<_>>(),
                        )));
                    }
                });
                if updates.changed().await.is_err() {
                    break;
                }
            }
        });
        *self.feed.lock().await = Some(handle);
    }

    pub async fn clear_feed(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Some(task) = self.feed.lock().await.take() {
            task.abort();
        }
        *self.feed_key.lock().await = None;
        self.clear_thread().await;
    }

    pub async fn open_thread(
        self: &Arc<Self>,
        account_id: String,
        room_id: String,
        root_event_id: String,
        generation: u64,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        if self.thread_generation.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        self.require_selected_room(&room_id).await?;
        {
            let mut feed = self.thread_feed.lock().await;
            if self.thread_generation.load(Ordering::Acquire) != generation {
                return Ok(());
            }
            if let Some(previous) = feed.take() {
                previous.abort();
            }
        }
        let feed = self
            .manager
            .thread_message_feed_for(&account_id, &room_id, &root_event_id)
            .await?;
        if self.thread_generation.load(Ordering::Acquire) != generation {
            return Ok(());
        }
        let mut updates = feed.subscribe();
        let current_generation = self.thread_generation.clone();
        let is_live = self.is_live.clone();
        let handle = tokio::spawn(async move {
            let _feed = feed;
            loop {
                let messages = updates.borrow_and_update().clone();
                let ui_for_update = ui.clone();
                let current_generation = current_generation.clone();
                let is_live = is_live.clone();
                let account_id = account_id.clone();
                let room_id = room_id.clone();
                let root_event_id = root_event_id.clone();
                let _ = ui_for_update.upgrade_in_event_loop(move |ui| {
                    if is_live.load(Ordering::Acquire)
                        && current_generation.load(Ordering::Acquire) == generation
                        && ui.get_active_account_local_id().as_str() == account_id
                        && ui.get_active_room_id().as_str() == room_id
                        && ui.get_thread_root_id().as_str() == root_event_id
                    {
                        ui.set_thread_messages(ModelRc::new(VecModel::from(
                            messages
                                .iter()
                                .filter(|message| {
                                    message.event_id.as_deref() != Some(root_event_id.as_str())
                                })
                                .map(message_item)
                                .collect::<Vec<_>>(),
                        )));
                    }
                });
                if updates.changed().await.is_err() {
                    break;
                }
            }
        });
        let mut current = self.thread_feed.lock().await;
        if self.thread_generation.load(Ordering::Acquire) == generation {
            *current = Some(handle);
        } else {
            handle.abort();
        }
        Ok(())
    }

    pub fn begin_thread_request(&self) -> u64 {
        self.thread_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn thread_request_is_current(&self, generation: u64) -> bool {
        self.thread_generation.load(Ordering::Acquire) == generation
    }

    pub async fn clear_thread(&self) {
        let generation = self.begin_thread_request();
        self.clear_thread_if_current(generation).await;
    }

    pub async fn clear_thread_if_current(&self, generation: u64) {
        let mut feed = self.thread_feed.lock().await;
        if self.thread_generation.load(Ordering::Acquire) == generation {
            if let Some(task) = feed.take() {
                task.abort();
            }
        }
    }

    async fn require_selected_room(&self, room_id: &str) -> Result<(), MatrixCoreError> {
        if !self.is_live.load(Ordering::Acquire) {
            return Err(MatrixCoreError::NoActiveAccount);
        }
        if self.selected_room.lock().await.as_deref() != Some(room_id) {
            return Err(MatrixCoreError::SelectedRoomChanged);
        }
        Ok(())
    }

    pub async fn is_selected_room_for(&self, account_id: &str, room_id: &str) -> bool {
        self.is_live.load(Ordering::Acquire)
            && self.selected_room.lock().await.as_deref() == Some(room_id)
            && self
                .manager
                .active_account()
                .await
                .is_some_and(|account| account.id == account_id)
    }

    pub async fn require_media_context(
        &self,
        account_id: &str,
        room_id: &str,
    ) -> Result<(), MatrixCoreError> {
        self.require_selected_room(room_id).await?;
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .map(|account| account.id.as_str())
            != Some(account_id)
        {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        Ok(())
    }

    pub async fn send_attachment(
        &self,
        account_id: &str,
        room_id: &str,
        path: PathBuf,
        mime_type: &str,
    ) -> Result<QueuedAttachment, MatrixCoreError> {
        self.require_media_context(account_id, room_id).await?;
        let queued = self
            .manager
            .send_attachment_for(account_id, room_id, path, mime_type)
            .await?;
        if let Err(error) = self.require_media_context(account_id, room_id).await {
            let _ = queued.abort().await;
            return Err(error);
        }
        Ok(queued)
    }

    pub async fn download_attachment(
        &self,
        account_id: &str,
        room_id: &str,
        event_id: &str,
    ) -> Result<DownloadedAttachment, MatrixCoreError> {
        self.require_media_context(account_id, room_id).await?;
        let downloaded = self
            .manager
            .download_attachment_for(account_id, room_id, event_id)
            .await?;
        self.require_media_context(account_id, room_id).await?;
        Ok(downloaded)
    }

    /// Commit decrypted bytes only while this exact account and room remain
    /// selected. Navigation and account switching wait for the bounded local
    /// write, preventing cross-account saves after an asynchronous download.
    pub async fn save_download_to(
        &self,
        account_id: &str,
        room_id: &str,
        path: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<PathBuf, MatrixCoreError> {
        let _transition = self.account_transition.lock().await;
        let selected = self.selected_room.lock().await;
        if !self.is_live.load(Ordering::Acquire) || selected.as_deref() != Some(room_id) {
            return Err(MatrixCoreError::SelectedRoomChanged);
        }
        if self
            .manager
            .active_account()
            .await
            .as_ref()
            .map(|account| account.id.as_str())
            != Some(account_id)
        {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        tokio::task::spawn_blocking(move || save_download(&path, &bytes))
            .await
            .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?
            .map_err(MatrixCoreError::Io)
    }

    pub async fn send(
        &self,
        account_id: String,
        room_id: String,
        body: String,
        target: MessageTarget,
        format: TextFormat,
    ) -> Result<Option<QueuedMessage>, MatrixCoreError> {
        self.require_selected_room(&room_id).await?;
        match target {
            MessageTarget::Room => {
                // Keep the original text durable until the send queue reports
                // a server acknowledgement or rejection.
                self.drafts
                    .persist_one(&self.manager, &account_id, &room_id)
                    .await?;
                self.manager
                    .send_formatted_tracked_for(&account_id, &room_id, body, format)
                    .await
                    .map(Some)
            }
            MessageTarget::Reply(event_id) => self
                .manager
                .send_reply_formatted_for(&account_id, &room_id, &event_id, body, format)
                .await
                .map(|_| None),
            MessageTarget::Edit(event_id) => self
                .manager
                .edit_formatted_for(&account_id, &room_id, &event_id, body, format)
                .await
                .map(|_| None),
            MessageTarget::Thread(root_event_id) => self
                .manager
                .send_thread_formatted_for(&account_id, &room_id, &root_event_id, body, format)
                .await
                .map(|_| None),
        }
    }

    pub async fn toggle_reaction(
        &self,
        account_id: String,
        room_id: String,
        event_id: String,
        reaction: String,
    ) -> Result<bool, MatrixCoreError> {
        self.require_selected_room(&room_id).await?;
        self.manager
            .toggle_reaction_for(&account_id, &room_id, &event_id, &reaction)
            .await
    }

    pub async fn watch_updates(self: Arc<Self>, ui: Weak<AppWindow>) {
        let mut active = self.manager.subscribe_active_account();
        loop {
            let room_subscription = self.manager.subscribe_room_updates().await;
            match room_subscription {
                Ok((account_id, mut rooms)) => loop {
                    tokio::select! {
                        result = active.changed() => {
                            if result.is_err() { return; }
                            self.invalidate_navigation();
                            if self.is_live.load(Ordering::Acquire) {
                                self.clear_feed().await;
                                *self.selected_room.lock().await = None;
                                if let Err(error) = self.refresh(ui.clone()).await {
                                    show_error(&ui, error.to_string());
                                } else {
                                    tokio::spawn(self.clone().scan_spaces(ui.clone()));
                                }
                            }
                            break;
                        }
                        result = rooms.recv() => {
                            let Ok(update) = result else { break; };
                            if morpheus_matrix::room_updates_change_space_graph(&update) {
                                self.invalidate_space_scan().await;
                            }
                            if self.is_live.load(Ordering::Acquire)
                                && self.manager.active_account().await.is_some_and(|current| current.id == account_id)
                            {
                                if let Err(error) = self.refresh(ui.clone()).await {
                                    show_error(&ui, error.to_string());
                                } else {
                                    tokio::spawn(self.clone().scan_spaces(ui.clone()));
                                }
                            }
                        }
                    }
                },
                Err(_) => {
                    if active.changed().await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    pub async fn watch_sync(self: Arc<Self>, ui: Weak<AppWindow>) {
        let mut status = self.manager.subscribe_sync_status();
        while status.changed().await.is_ok() {
            if !self.is_live.load(Ordering::Acquire) {
                continue;
            }
            let label = match &*status.borrow_and_update() {
                SyncStatus::Stopped => "Sync stopped".to_owned(),
                SyncStatus::Starting => "Connecting to homeserver…".to_owned(),
                SyncStatus::Running {
                    backend: SyncBackend::SlidingSticky,
                } => String::new(),
                SyncStatus::Running {
                    backend: SyncBackend::Classic,
                } => "Chat connected; native calls require sliding sync on this homeserver"
                    .to_owned(),
                SyncStatus::Retrying { reason, .. } => format!("Reconnecting: {reason}"),
            };
            let _ = ui.upgrade_in_event_loop(move |ui| ui.set_session_error(label.into()));
        }
    }
}

struct Snapshot {
    accounts: Vec<AccountInfo>,
    active: AccountInfo,
    rooms: Vec<RoomInfo>,
    selected: Option<String>,
    selected_space: Option<String>,
    known_spaces: HashMap<String, String>,
    space_children: HashMap<String, HashSet<String>>,
    all_space_children: HashSet<String>,
    local_voice_rooms: HashSet<String>,
    legacy_element_call_rooms: HashSet<String>,
    shared_voice_rooms: HashSet<String>,
}

fn apply_snapshot(
    ui: &AppWindow,
    view: Snapshot,
    drafts: &DraftStore,
    security_gate: &crate::security::SecurityGate,
) {
    let next_room_id = view.selected.as_deref().unwrap_or("");
    let account_changed = ui.get_active_account_local_id().as_str() != view.active.id;
    let close_sign_in = snapshot_closes_sign_in(ui.get_live_session(), account_changed);
    let selection_changed = account_changed || ui.get_active_room_id().as_str() != next_room_id;
    if account_changed {
        crate::security::clear_ui(ui, security_gate);
        ui.set_room_actions_open(false);
        ui.set_room_action_busy(false);
        ui.set_room_action_error("".into());
        ui.set_invite_error("".into());
        ui.set_room_action_operation(ui.get_room_action_operation().wrapping_add(1));
        ui.set_directory_operation(ui.get_directory_operation().wrapping_add(1));
        ui.set_directory_join_operation(ui.get_directory_join_operation().wrapping_add(1));
        ui.set_directory_loading(false);
        ui.set_directory_join_busy(false);
        ui.set_directory_joining_room_id("".into());
        ui.set_directory_searched(false);
        ui.set_directory_results(ModelRc::new(VecModel::from(Vec::<PublicRoomItem>::new())));
        ui.set_directory_next_batch("".into());
        ui.set_directory_error("".into());
        ui.set_directory_join_error("".into());
        ui.set_directory_search("".into());
        ui.set_directory_server("".into());
        ui.set_directory_join_input("".into());
    }
    if selection_changed {
        ui.set_room_admin_open(false);
        ui.set_room_admin_operation(ui.get_room_admin_operation().wrapping_add(1));
        ui.set_room_admin_busy(false);
        ui.set_room_admin_error("".into());
        ui.set_room_admin_notice("".into());
        ui.set_room_admin_event_id("".into());
        ui.set_voice_designation_operation(ui.get_voice_designation_operation().wrapping_add(1));
        ui.set_voice_designation_busy(false);
        ui.set_voice_designation_error("".into());
        ui.set_voice_designation_notice("".into());
        ui.set_history_operation(ui.get_history_operation().wrapping_add(1));
        ui.set_history_loading(false);
        ui.set_history_at_start(false);
        ui.set_history_error("".into());
        ui.set_members(ModelRc::new(VecModel::<MemberItem>::from(Vec::new())));
        ui.set_member_loading(!next_room_id.is_empty());
        ui.set_member_error("".into());
        ui.set_member_action_operation(ui.get_member_action_operation().wrapping_add(1));
        ui.set_member_action_busy(false);
        ui.set_member_action_error("".into());
        ui.set_member_action_notice("".into());
        ui.set_member_invite_user_id("".into());
        ui.set_member_selected_user_id("".into());
        ui.set_member_selected_name("".into());
        ui.set_member_selected_membership("".into());
        ui.set_member_selected_power("".into());
        ui.set_member_selected_self(false);
        ui.set_member_selected_creator(false);
        ui.set_member_confirm_open(false);
        crate::reset_attachment_view(ui);
        crate::reset_room_search(ui);
        ui.set_element_call_compatibility(false);
        ui.set_composer_text(drafts.get(&view.active.id, next_room_id).into());
        ui.set_composer_markdown(drafts.markdown(&view.active.id, next_room_id));
        ui.set_composer_saved_markdown(false);
        ui.set_composer_mode(0);
        ui.set_composer_target_id("".into());
        ui.set_composer_target_label("".into());
        ui.set_composer_saved_draft("".into());
        ui.set_composer_busy(false);
        ui.set_composer_operation(ui.get_composer_operation().wrapping_add(1));
        ui.set_thread_root_id("".into());
        ui.set_thread_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
        ui.set_thread_composer_text("".into());
    }
    let active_index = view
        .accounts
        .iter()
        .position(|account| account.id == view.active.id)
        .unwrap_or(0);
    let items = view
        .accounts
        .iter()
        .enumerate()
        .map(|(index, account)| account_item(account, index == active_index))
        .collect::<Vec<_>>();
    let options = std::iter::once(active_index)
        .chain((0..view.accounts.len()).filter(|index| *index != active_index))
        .map(|index| {
            let item = &items[index];
            AccountOption {
                index: index as i32,
                name: item.name.clone(),
                handle: item.handle.clone(),
                initials: item.initials.clone(),
                accent: item.accent,
            }
        })
        .collect::<Vec<_>>();
    let account = &items[active_index];
    ui.set_accounts(ModelRc::new(VecModel::from(items.clone())));
    ui.set_account_options(ModelRc::new(VecModel::from(options)));
    ui.set_active_account_index(active_index as i32);
    ui.set_active_account_local_id(view.active.id.as_str().into());
    ui.set_active_account_name(account.name.clone());
    ui.set_active_account_handle(account.handle.clone());
    ui.set_active_account_initials(account.initials.clone());
    ui.set_active_account_status(account.status.clone());
    ui.set_active_account_accent(account.accent);
    let active_space_name = view
        .selected_space
        .as_deref()
        .and_then(|id| view.rooms.iter().find(|room| room.room_id == id))
        .map(|room| room.name.as_str())
        .or_else(|| {
            view.selected_space
                .as_deref()
                .and_then(|id| view.known_spaces.get(id).map(String::as_str))
        })
        .unwrap_or("Home");
    ui.set_active_space_id(view.selected_space.as_deref().unwrap_or("").into());
    ui.set_active_space_name(active_space_name.into());
    let mut spaces = view
        .rooms
        .iter()
        .filter(|room| {
            (room.is_space || view.known_spaces.contains_key(&room.room_id))
                && room.membership == RoomMembership::Joined
        })
        .map(|room| SpaceItem {
            id: room.room_id.as_str().into(),
            name: view
                .known_spaces
                .get(&room.room_id)
                .map_or(room.name.as_str(), String::as_str)
                .into(),
            initials: initials(
                view.known_spaces
                    .get(&room.room_id)
                    .map_or(room.name.as_str(), String::as_str),
            )
            .into(),
            accent: accent(&room.room_id),
        })
        .collect::<Vec<_>>();
    for (id, name) in &view.known_spaces {
        if !spaces.iter().any(|space| space.id.as_str() == id) {
            spaces.push(SpaceItem {
                id: id.as_str().into(),
                name: name.as_str().into(),
                initials: initials(name).into(),
                accent: accent(id),
            });
        }
    }
    ui.set_spaces(ModelRc::new(VecModel::from(spaces)));
    let visible = |room: &RoomInfo| {
        if room.is_space
            || view.known_spaces.contains_key(&room.room_id)
            || room.membership != RoomMembership::Joined
        {
            return false;
        }
        if let Some(space_id) = view.selected_space.as_deref() {
            view.space_children
                .get(space_id)
                .is_some_and(|children| children.contains(&room.room_id))
        } else {
            room.is_direct || !view.all_space_children.contains(&room.room_id)
        }
    };
    ui.set_invitations(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| room.membership == RoomMembership::Invited)
            .map(|room| room_item(room, false, false))
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| !room.is_direct && visible(room))
            .map(|room| {
                room_item(
                    room,
                    view.local_voice_rooms.contains(&room.room_id),
                    view.shared_voice_rooms.contains(&room.room_id),
                )
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| room.is_direct && visible(room))
            .map(|room| {
                room_item(
                    room,
                    view.local_voice_rooms.contains(&room.room_id),
                    view.shared_voice_rooms.contains(&room.room_id),
                )
            })
            .collect::<Vec<_>>(),
    )));
    crate::apply_room_sort(ui);
    ui.set_stress_timeline_active(false);
    if let Some(room_id) = view.selected {
        if let Some(room) = view.rooms.iter().find(|room| room.room_id == room_id) {
            if selection_changed {
                ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
            }
            ui.set_active_room_id(room.room_id.as_str().into());
            ui.set_active_room_name(room.name.as_str().into());
            ui.set_active_room_topic_raw(room.topic.as_deref().unwrap_or("").into());
            ui.set_element_call_compatibility(
                view.legacy_element_call_rooms.contains(&room.room_id),
            );
            ui.set_active_voice_local(view.local_voice_rooms.contains(&room.room_id));
            ui.set_active_voice_shared(view.shared_voice_rooms.contains(&room.room_id));
            ui.set_active_room_topic(if room.membership == RoomMembership::Invited {
                "Invitation · join to read messages".into()
            } else {
                room.topic.as_deref().unwrap_or("No room topic").into()
            });
        }
    } else {
        ui.set_active_room_id("".into());
        ui.set_active_room_name("No rooms yet".into());
        ui.set_active_room_topic_raw("".into());
        ui.set_element_call_compatibility(false);
        ui.set_active_voice_local(false);
        ui.set_active_voice_shared(false);
        ui.set_active_room_topic("Join or create a Matrix room to begin".into());
        ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
    }
    ui.set_live_session(true);
    if account_changed && ui.get_page() == 1 {
        ui.invoke_refresh_security();
    }
    if close_sign_in {
        ui.set_login_loading(false);
        ui.set_login_error("".into());
        ui.set_show_sign_in(false);
    }
}

fn account_item(account: &AccountInfo, selected: bool) -> AccountItem {
    let name = display_user(&account.user_id);
    let account_initials = initials(&name);
    AccountItem {
        name: name.into(),
        handle: account.user_id.as_str().into(),
        initials: account_initials.into(),
        status: if selected { "Connected" } else { "Paused" }.into(),
        accent: accent(&account.id),
    }
}

fn room_item(room: &RoomInfo, voice_local: bool, voice_shared: bool) -> RoomItem {
    RoomItem {
        id: room.room_id.as_str().into(),
        name: room.name.as_str().into(),
        unread: room.unread_notifications.min(i32::MAX as u64) as i32,
        private_room: room.is_direct,
        voice_space: voice_local || voice_shared,
        voice_local,
        voice_shared,
    }
}

fn apply_shared_voice_labels(ui: &AppWindow, shared_rooms: &HashSet<String>) {
    let update = |mut room: RoomItem| {
        room.voice_shared = shared_rooms.contains(room.id.as_str());
        room.voice_space = room.voice_local || room.voice_shared;
        room
    };
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(
        ui.get_unsorted_channels()
            .iter()
            .map(update)
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(
        ui.get_unsorted_direct_messages()
            .iter()
            .map(update)
            .collect::<Vec<_>>(),
    )));
    ui.set_active_voice_shared(shared_rooms.contains(ui.get_active_room_id().as_str()));
    crate::apply_room_sort(ui);
}

fn member_item(info: &RoomMemberInfo) -> MemberItem {
    let user_id = crate::directory_plain_text(&info.user_id, 260);
    let name = info
        .display_name
        .as_deref()
        .map(|value| crate::directory_plain_text(value, 100))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| crate::directory_plain_text(&display_user(&info.user_id), 100));
    let membership = match info.membership.as_str() {
        "join" => match info.power_level {
            None => "Creator".to_owned(),
            Some(level) if level >= 100 => format!("Admin ({level})"),
            Some(level) if level >= 50 => format!("Moderator ({level})"),
            Some(level) if level > 0 => format!("Power {level}"),
            _ => "Member".to_owned(),
        },
        "invite" => "Invited".to_owned(),
        "ban" => "Banned".to_owned(),
        _ => "Member".to_owned(),
    };
    let detail = format!(
        "{} · {}{}",
        user_id,
        if info.is_account_user { "You · " } else { "" },
        membership
    );
    MemberItem {
        user_id: user_id.into(),
        name: name.clone().into(),
        initials: initials(&name).into(),
        detail: detail.into(),
        membership: info.membership.as_str().into(),
        power_level_text: info
            .power_level
            .map_or_else(String::new, |level| level.to_string())
            .into(),
        is_account_user: info.is_account_user,
        is_creator: info.membership == "join" && info.power_level.is_none(),
        accent: accent(&info.user_id),
        online: false,
    }
}

fn message_item(info: &MessageInfo) -> MessageItem {
    let sender = display_user(&info.sender);
    let sender_initials = initials(&sender);
    let time = match info.send_state {
        MessageSendState::Pending => "Sending…".to_owned(),
        MessageSendState::Failed => "Failed to send".to_owned(),
        MessageSendState::Sent => Local
            .timestamp_millis_opt(info.timestamp_ms as i64)
            .single()
            .map(|time| time.format("%-I:%M %p").to_string())
            .unwrap_or_default(),
    };
    let display_body = if info.attachment_name.as_deref() == Some(info.body.as_str()) {
        ""
    } else {
        info.body.as_str()
    };
    let styled_markdown = if display_body.is_empty() {
        None
    } else {
        info.rich_markdown
            .as_deref()
            .and_then(|markdown| StyledText::from_markdown(markdown).ok())
    };
    let rich_accessible_body = if styled_markdown.is_some() {
        info.rich_accessible
            .as_deref()
            .filter(|text| !text.is_empty())
            .unwrap_or(display_body)
    } else {
        display_body
    };
    // Use the accepted formatted projection as the edit source. The plain
    // Matrix body may be an unrelated fallback, so editing it would silently
    // discard visible styling when an unchanged message is saved.
    let edit_markdown = styled_markdown.is_some();
    let edit_source = if edit_markdown {
        info.rich_markdown.as_deref().unwrap_or(display_body)
    } else {
        display_body
    };
    let rich_body = styled_markdown.unwrap_or_else(|| StyledText::from_plain_text(display_body));
    MessageItem {
        event_id: info.event_id.as_deref().unwrap_or("").into(),
        is_own: info.is_own,
        sender: sender.into(),
        initials: sender_initials.into(),
        time: time.into(),
        body: display_body.into(),
        rich_body,
        rich_accessible_body: rich_accessible_body.into(),
        edit_source: edit_source.into(),
        edit_markdown,
        reply_to: info.reply_preview.as_deref().unwrap_or("").into(),
        attachment: info.attachment_name.as_deref().unwrap_or("").into(),
        reaction: info.reaction_summary.as_str().into(),
        accent: accent(&info.sender),
    }
}

fn display_user(id: &str) -> String {
    id.trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(id)
        .to_owned()
}

fn initials(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_alphanumeric())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

fn accent(value: &str) -> Color {
    const COLORS: [(u8, u8, u8); 5] = [
        (168, 140, 255),
        (120, 200, 177),
        (243, 159, 120),
        (100, 165, 232),
        (231, 139, 164),
    ];
    let hash = value.bytes().fold(0usize, |current, byte| {
        current.wrapping_mul(31).wrapping_add(byte as usize)
    });
    let (red, green, blue) = COLORS[hash % COLORS.len()];
    Color::from_rgb_u8(red, green, blue)
}

pub fn show_error(ui: &Weak<AppWindow>, error: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| ui.set_session_error(error.into()));
}

fn visible_feed_target(
    visible_account: &str,
    visible_room: &str,
    expected_account: &str,
    expected_room: &str,
) -> bool {
    visible_account == expected_account && visible_room == expected_room
}

fn snapshot_closes_sign_in(was_live: bool, account_changed: bool) -> bool {
    !was_live || account_changed
}

#[cfg(test)]
mod member_tests {
    use super::*;

    #[test]
    fn member_rows_sanitize_untrusted_identity_and_do_not_claim_presence() {
        let item = member_item(&RoomMemberInfo {
            user_id: "@alice:example.org".into(),
            display_name: Some("Alice\u{202e}Admin\u{200b}".into()),
            avatar_mxc: None,
            membership: "join".into(),
            power_level: Some(50),
            is_account_user: true,
        });
        assert_eq!(item.name.as_str(), "Alice Admin");
        assert_eq!(
            item.detail.as_str(),
            "@alice:example.org · You · Moderator (50)"
        );
        assert!(!item.online);

        let invited = member_item(&RoomMemberInfo {
            user_id: "@bob:example.org".into(),
            display_name: None,
            avatar_mxc: None,
            membership: "invite".into(),
            power_level: Some(0),
            is_account_user: false,
        });
        assert_eq!(invited.name.as_str(), "bob");
        assert_eq!(invited.detail.as_str(), "@bob:example.org · Invited");
    }

    #[test]
    fn feed_updates_require_the_visible_account_and_room() {
        assert!(visible_feed_target(
            "account-a",
            "!room-a",
            "account-a",
            "!room-a"
        ));
        assert!(!visible_feed_target(
            "account-b",
            "!room-a",
            "account-a",
            "!room-a"
        ));
        assert!(!visible_feed_target(
            "account-a",
            "!room-b",
            "account-a",
            "!room-a"
        ));
    }

    #[test]
    fn background_refresh_keeps_add_account_form_open() {
        assert!(!snapshot_closes_sign_in(true, false));
        assert!(snapshot_closes_sign_in(true, true));
        assert!(snapshot_closes_sign_in(false, false));
    }

    #[test]
    fn formatted_edit_uses_visible_source_and_matching_accessibility_text() {
        let info = MessageInfo {
            key: "message".into(),
            event_id: Some("$event".into()),
            sender: "@alice:example.org".into(),
            body: "different plain fallback".into(),
            rich_markdown: Some("Visible **bold** text".into()),
            rich_accessible: Some("Visible bold text".into()),
            attachment_name: None,
            reply_preview: None,
            reaction_summary: String::new(),
            timestamp_ms: 0,
            is_own: true,
            is_local_echo: false,
            send_state: MessageSendState::Sent,
        };
        let item = message_item(&info);
        assert!(item.edit_markdown);
        assert_eq!(item.edit_source.as_str(), "Visible **bold** text");
        assert_eq!(item.rich_accessible_body.as_str(), "Visible bold text");

        let plain = message_item(&MessageInfo {
            rich_markdown: None,
            rich_accessible: None,
            ..info
        });
        assert!(!plain.edit_markdown);
        assert_eq!(plain.edit_source.as_str(), "different plain fallback");
        assert_eq!(
            plain.rich_accessible_body.as_str(),
            "different plain fallback"
        );
    }
}

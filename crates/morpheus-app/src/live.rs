//! Thin presentation adapter between the account-scoped Matrix core and Slint.
//! All Matrix work runs on Tokio; UI models are created on Slint's event loop.

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use chrono::{Local, TimeZone};
use morpheus_matrix::{
    AccountInfo, AccountManager, MatrixCoreError, MessageInfo, MessageSendState, RoomInfo,
    RoomMembership, SyncBackend, SyncStatus,
};
use slint::{Color, ModelRc, SharedString, VecModel, Weak};
use tokio::{sync::Mutex, task::JoinHandle};

use crate::{AccountItem, AccountOption, AppWindow, MemberItem, MessageItem, RoomItem, SpaceItem};

pub enum MessageTarget {
    Room,
    Reply(String),
    Edit(String),
    Thread(String),
}

#[derive(Clone, Copy)]
pub enum RoomAction {
    CreatePrivate,
    Join,
    CreateSpace,
}

struct KnownSpace {
    name: String,
    recorded_at: Instant,
}

pub struct LiveUi {
    pub manager: Arc<AccountManager>,
    pub is_live: Arc<AtomicBool>,
    drafts: Arc<DraftStore>,
    generation: Arc<AtomicU64>,
    render_epoch: Arc<AtomicU64>,
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
    pub fn new(manager: Arc<AccountManager>) -> Arc<Self> {
        Arc::new(Self {
            manager,
            is_live: Arc::new(AtomicBool::new(false)),
            drafts: Arc::new(DraftStore::default()),
            generation: Arc::new(AtomicU64::new(0)),
            render_epoch: Arc::new(AtomicU64::new(0)),
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
    pub fn invalidate_navigation(&self) {
        self.render_epoch.fetch_add(1, Ordering::AcqRel);
    }

    pub fn remember_draft(&self, account_id: &str, room_id: &str, body: &str) {
        if !account_id.is_empty() && !room_id.is_empty() {
            self.drafts.set(account_id, room_id, body);
        }
    }

    pub fn clear_draft_if_unchanged(&self, account_id: &str, room_id: &str, submitted: &str) {
        self.drafts
            .clear_if_unchanged(account_id, room_id, submitted);
    }

    pub fn forget_account_drafts(&self, account_id: &str) {
        self.drafts.forget_account(account_id);
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
        let _transition = self.account_transition.lock().await;
        self.invalidate_navigation();
        let accounts = self.manager.accounts().await;
        let account = accounts
            .get(index)
            .ok_or_else(|| MatrixCoreError::AccountNotFound(index.to_string()))?;
        self.clear_feed().await;
        self.manager.switch_to(&account.id).await?;
        *self.selected_room.lock().await = None;
        *self.selected_space.lock().await = None;
        self.invalidate_space_scan().await;
        self.space_children.lock().await.clear();
        self.is_live.store(true, Ordering::Release);
        self.refresh(ui.clone()).await?;
        tokio::spawn(self.clone().scan_spaces(ui));
        Ok(())
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
            return Err(MatrixCoreError::InvalidRegistry(match action {
                RoomAction::Join => "Enter a complete room ID or alias".into(),
                _ => "Enter a room or Space name".into(),
            }));
        }
        let room_id = match action {
            RoomAction::CreatePrivate => self.manager.create_private_room(input).await?,
            RoomAction::Join => self.manager.join_room(input).await?,
            RoomAction::CreateSpace => self.manager.create_space(input).await?,
        };
        self.require_active_account(account_id).await?;
        let is_space = match action {
            RoomAction::CreateSpace => true,
            RoomAction::CreatePrivate => false,
            RoomAction::Join => self
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
                        name: input.to_owned(),
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

    pub async fn select_space(
        self: &Arc<Self>,
        space_id: String,
        ui: Weak<AppWindow>,
    ) -> Result<(), MatrixCoreError> {
        self.invalidate_navigation();
        if !space_id.is_empty() {
            let account = self
                .manager
                .active_account()
                .await
                .ok_or(MatrixCoreError::NoActiveAccount)?;
            let rooms = self.manager.rooms_snapshot().await?;
            let known = self
                .known_spaces
                .lock()
                .await
                .get(&account.id)
                .is_some_and(|spaces| spaces.contains_key(&space_id));
            if !known
                && !rooms
                    .iter()
                    .any(|room| room.is_space && room.room_id == space_id)
            {
                return Err(MatrixCoreError::RoomNotFound(space_id));
            }
        }
        *self.selected_space.lock().await = (!space_id.is_empty()).then_some(space_id);
        *self.selected_room.lock().await = None;
        self.clear_feed().await;
        self.refresh(ui).await
    }

    pub async fn refresh(self: &Arc<Self>, ui: Weak<AppWindow>) -> Result<(), MatrixCoreError> {
        if !self.is_live.load(Ordering::Acquire) {
            return Ok(());
        }
        let render_epoch = self.render_epoch.fetch_add(1, Ordering::AcqRel) + 1;
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
        let view = Snapshot {
            accounts,
            active,
            rooms,
            selected: selected.clone(),
            selected_space,
            known_spaces,
            space_children,
            all_space_children,
        };
        let current_epoch = self.render_epoch.clone();
        let is_live = self.is_live.clone();
        let drafts = self.drafts.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if is_live.load(Ordering::Acquire)
                && current_epoch.load(Ordering::Acquire) == render_epoch
            {
                apply_snapshot(&ui, view, &drafts);
            }
        });
        if let Some(room_id) = selected {
            self.open_room(room_id, ui).await;
        }
        Ok(())
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

    pub async fn open_room(self: &Arc<Self>, room_id: String, ui: Weak<AppWindow>) {
        if self.selected_room.lock().await.as_deref() != Some(room_id.as_str()) {
            self.clear_thread().await;
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.set_thread_root_id("".into());
                ui.set_thread_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
                ui.set_thread_composer_text("".into());
            });
        }
        *self.selected_room.lock().await = Some(room_id.clone());
        let account_id = match self.manager.active_account().await {
            Some(account) => account.id,
            None => return,
        };
        let requested_key = (account_id, room_id.clone());
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
                show_error(&ui, error.to_string());
                return;
            }
        };
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        if let Some(previous) = self.feed.lock().await.take() {
            previous.abort();
        }
        *self.feed_key.lock().await = Some(requested_key);
        let mut updates = feed.subscribe();
        let feed_generation = self.generation.clone();
        let is_live = self.is_live.clone();
        let handle = tokio::spawn(async move {
            let _feed = feed;
            loop {
                let messages = updates.borrow_and_update().clone();
                let ui_for_update = ui.clone();
                let current_generation = feed_generation.clone();
                let live_flag = is_live.clone();
                let _ = ui_for_update.upgrade_in_event_loop(move |ui| {
                    if live_flag.load(Ordering::Acquire)
                        && current_generation.load(Ordering::Acquire) == generation
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

    pub async fn send(
        &self,
        account_id: String,
        room_id: String,
        body: String,
        target: MessageTarget,
    ) -> Result<(), MatrixCoreError> {
        self.require_selected_room(&room_id).await?;
        match target {
            MessageTarget::Room => {
                self.manager
                    .send_text_for(&account_id, &room_id, body)
                    .await
            }
            MessageTarget::Reply(event_id) => {
                self.manager
                    .send_reply_for(&account_id, &room_id, &event_id, body)
                    .await
            }
            MessageTarget::Edit(event_id) => {
                self.manager
                    .edit_text_for(&account_id, &room_id, &event_id, body)
                    .await
            }
            MessageTarget::Thread(root_event_id) => {
                self.manager
                    .send_thread_text_for(&account_id, &room_id, &root_event_id, body)
                    .await
            }
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
}

/// Drafts contain message text only and live in process memory. In particular,
/// they never enter config.toml or the account credential vault.
#[derive(Default)]
struct DraftStore {
    by_room: StdMutex<HashMap<(String, String), String>>,
}

impl DraftStore {
    fn set(&self, account_id: &str, room_id: &str, body: &str) {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let key = (account_id.to_owned(), room_id.to_owned());
        if body.is_empty() {
            drafts.remove(&key);
        } else {
            drafts.insert(key, body.to_owned());
        }
    }

    fn get(&self, account_id: &str, room_id: &str) -> String {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .get(&(account_id.to_owned(), room_id.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn clear_if_unchanged(&self, account_id: &str, room_id: &str, submitted: &str) {
        let mut drafts = self.by_room.lock().expect("draft mutex poisoned");
        let key = (account_id.to_owned(), room_id.to_owned());
        if drafts.get(&key).is_some_and(|draft| draft == submitted) {
            drafts.remove(&key);
        }
    }

    fn forget_account(&self, account_id: &str) {
        self.by_room
            .lock()
            .expect("draft mutex poisoned")
            .retain(|(id, _), _| id != account_id);
    }
}

fn apply_snapshot(ui: &AppWindow, view: Snapshot, drafts: &DraftStore) {
    let next_room_id = view.selected.as_deref().unwrap_or("");
    let account_changed = ui.get_active_account_local_id().as_str() != view.active.id;
    let selection_changed = account_changed || ui.get_active_room_id().as_str() != next_room_id;
    if account_changed {
        ui.set_room_actions_open(false);
        ui.set_room_action_busy(false);
        ui.set_room_action_error("".into());
        ui.set_invite_error("".into());
        ui.set_room_action_operation(ui.get_room_action_operation().wrapping_add(1));
    }
    if selection_changed {
        ui.set_composer_text(drafts.get(&view.active.id, next_room_id).into());
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
            .map(room_item)
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_channels(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| !room.is_direct && visible(room))
            .map(room_item)
            .collect::<Vec<_>>(),
    )));
    ui.set_unsorted_direct_messages(ModelRc::new(VecModel::from(
        view.rooms
            .iter()
            .filter(|room| room.is_direct && visible(room))
            .map(room_item)
            .collect::<Vec<_>>(),
    )));
    crate::apply_room_sort(ui);
    ui.set_members(ModelRc::new(VecModel::<MemberItem>::from(Vec::new())));
    ui.set_stress_timeline_active(false);
    if let Some(room_id) = view.selected {
        if let Some(room) = view.rooms.iter().find(|room| room.room_id == room_id) {
            if selection_changed {
                ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
            }
            ui.set_active_room_id(room.room_id.as_str().into());
            ui.set_active_room_name(room.name.as_str().into());
            ui.set_active_room_topic(if room.membership == RoomMembership::Invited {
                "Invitation · join to read messages".into()
            } else {
                "Matrix room".into()
            });
        }
    } else {
        ui.set_active_room_id("".into());
        ui.set_active_room_name("No rooms yet".into());
        ui.set_active_room_topic("Join or create a Matrix room to begin".into());
        ui.set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new())));
    }
    ui.set_login_loading(false);
    ui.set_login_error("".into());
    ui.set_live_session(true);
    ui.set_show_sign_in(false);
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

fn room_item(room: &RoomInfo) -> RoomItem {
    RoomItem {
        id: room.room_id.as_str().into(),
        name: room.name.as_str().into(),
        unread: room.unread_notifications.min(i32::MAX as u64) as i32,
        private_room: room.is_direct,
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
    MessageItem {
        event_id: info.event_id.as_deref().unwrap_or("").into(),
        is_own: info.is_own,
        sender: sender.into(),
        initials: sender_initials.into(),
        time: time.into(),
        body: info.body.as_str().into(),
        reply_to: info.reply_preview.as_deref().unwrap_or("").into(),
        attachment: SharedString::default(),
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

#[cfg(test)]
mod tests {
    use super::DraftStore;

    #[test]
    fn drafts_are_scoped_to_account_and_room_and_preserve_new_edits() {
        let drafts = DraftStore::default();
        drafts.set("account-a", "room-1", "first message");
        drafts.set("account-a", "room-2", "another room");
        drafts.set("account-b", "room-1", "another account");

        assert_eq!(drafts.get("account-a", "room-1"), "first message");
        assert_eq!(drafts.get("account-a", "room-2"), "another room");
        assert_eq!(drafts.get("account-b", "room-1"), "another account");

        drafts.set("account-a", "room-1", "edited while sending");
        drafts.clear_if_unchanged("account-a", "room-1", "first message");
        assert_eq!(drafts.get("account-a", "room-1"), "edited while sending");
        drafts.clear_if_unchanged("account-a", "room-1", "edited while sending");
        assert_eq!(drafts.get("account-a", "room-1"), "");

        drafts.forget_account("account-a");
        assert_eq!(drafts.get("account-a", "room-2"), "");
        assert_eq!(drafts.get("account-b", "room-1"), "another account");
    }
}

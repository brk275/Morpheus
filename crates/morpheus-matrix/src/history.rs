//! Account-selection ownership for read-only timeline requests.
//!
//! Keep the manager state lock out of homeserver waits so a stalled history
//! or timeline-open request cannot prevent account controls from acquiring it.
//! Revocation drops the read future; an HTTP request already handed to a server
//! cannot be recalled.

use std::{future::Future, pin::Pin};

use matrix_sdk_ui::timeline::TimelineEventFocusThreadMode;

use super::*;

/// Most history pages one reveal may read, and the events per page.
const MAX_REVEAL_PAGES: u8 = 8;
const REVEAL_PAGE_EVENTS: u16 = 50;
/// Most events a focused conversation view loads around its event.
const MAX_FOCUS_CONTEXT: u16 = 50;

/// One conversation view focused on an event, owned by whoever loaded it.
/// Nothing about it is cached or shared: loading another view never
/// replaces it, paging acts on exactly this view, and dropping the last
/// clone ends it.
#[derive(Clone)]
pub struct FocusedView {
    /// The exact selection that loaded it: its timeline belongs to that
    /// selection's client and to nothing selected later.
    selection: AccountSelection,
    room_id: String,
    timeline: Arc<Timeline>,
}

impl FocusedView {
    pub fn room_id(&self) -> &str {
        &self.room_id
    }

    /// Only the selection that loaded the view, while current, may act on
    /// it; a later selection of the same account ID may not.
    pub(crate) fn owned_by(&self, selection: &AccountSelection) -> bool {
        view_owned_by(&self.selection, selection)
    }

    /// Its rows are only in this timeline, so actions on them use it.
    pub(crate) fn timeline(&self) -> &Arc<Timeline> {
        &self.timeline
    }
}

/// A view loaded under `view` may be used under `selection` only when that
/// is the same selection epoch and it is still current: A -> B -> A, removal
/// and signing in again all create a new epoch for the same account ID.
fn view_owned_by(view: &AccountSelection, selection: &AccountSelection) -> bool {
    selection.is_current() && view.same_epoch(selection)
}

/// How far a bounded backward reveal through history got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryReveal {
    /// The event is among the timeline's loaded items.
    pub found: bool,
    /// The start of the room or thread was reached.
    pub start_reached: bool,
}

impl AccountManager {
    /// Load older items for the account selected when this future is polled.
    /// Returns true when the start of the room was reached.
    pub async fn paginate_backwards(&self, room_id: &str, count: u16) -> Result<bool> {
        let account_id = {
            let active = self.active_tx.borrow();
            active
                .as_ref()
                .map(|account| account.id.clone())
                .ok_or(MatrixCoreError::NoActiveAccount)?
        };
        self.paginate_backwards_for(&account_id, room_id, count)
            .await
    }

    /// Account-bound compatibility entry point. Desktop clicks should capture
    /// an AccountSelection synchronously and use paginate_backwards_selected,
    /// so queued work also rejects an account A -> B -> A sequence.
    pub async fn paginate_backwards_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        count: u16,
    ) -> Result<bool> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        self.paginate_backwards_selected(&selection, room_id, count)
            .await
    }

    /// Read older history under the exact selection captured by the caller.
    /// Switching, removal and stop revoke the wait and its result, including
    /// when the same local account is selected again before work starts.
    pub async fn paginate_backwards_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        count: u16,
    ) -> Result<bool> {
        self.read_selected_timeline(selection, |loaded| async move {
            let timeline = loaded.timeline(room_id).await?;
            timeline
                .paginate_backwards(count)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
        })
        .await
    }

    /// Load older replies of one thread under the exact selection captured
    /// by the caller. Returns true when the start of the thread was reached.
    pub async fn paginate_thread_backwards_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        root_event_id: &str,
        count: u16,
    ) -> Result<bool> {
        self.read_selected_timeline(selection, |loaded| async move {
            let timeline = loaded.thread_timeline(room_id, root_event_id).await?;
            timeline
                .paginate_backwards(count)
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))
        })
        .await
    }

    /// Page back through a room's conversation, or one of its threads, until
    /// `event_id` is loaded, the start is reached or `max_pages` pages were
    /// read. The feeds of that same timeline show what was loaded.
    pub async fn reveal_event_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
        event_id: &str,
        max_pages: u8,
    ) -> Result<HistoryReveal> {
        let target = parse_event_id(event_id)?;
        self.read_selected_timeline(selection, |loaded| async move {
            let timeline = match thread_root {
                Some(root) => loaded.thread_timeline(room_id, root).await?,
                None => loaded.timeline(room_id).await?,
            };
            let mut start_reached = false;
            for _ in 0..max_pages.min(MAX_REVEAL_PAGES) {
                if timeline.item_by_event_id(&target).await.is_some() {
                    return Ok(HistoryReveal {
                        found: true,
                        start_reached,
                    });
                }
                if start_reached {
                    break;
                }
                start_reached = timeline
                    .paginate_backwards(REVEAL_PAGE_EVENTS)
                    .await
                    .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
            }
            let found = timeline.item_by_event_id(&target).await.is_some();
            Ok(HistoryReveal {
                found,
                start_reached,
            })
        })
        .await
    }

    /// Load a conversation view of one room focused on `event_id`, with up
    /// to `num_context_events` around it, for showing an older message in
    /// context. The view and its feed belong to the caller alone: nothing is
    /// cached, so a superseded load can never replace a view that is shown.
    /// The feed has the same ownership rules as live feeds.
    pub async fn focused_view_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        event_id: &str,
        num_context_events: u16,
    ) -> Result<(FocusedView, MessageFeed)> {
        let target = parse_event_id(event_id)?;
        self.read_selected_timeline(selection, |loaded| async move {
            let id = RoomId::parse(room_id)
                .map_err(|error| MatrixCoreError::InvalidRoomId(error.to_string()))?;
            let room = loaded
                .client
                .get_room(&id)
                .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))?;
            let timeline = room
                .timeline_builder()
                .with_focus(TimelineFocus::Event {
                    target,
                    num_context_events: num_context_events.min(MAX_FOCUS_CONTEXT),
                    thread_mode: TimelineEventFocusThreadMode::Automatic {
                        hide_threaded_events: true,
                    },
                })
                .build()
                .await
                .map_err(|error| MatrixCoreError::Sdk(error.to_string()))?;
            let view = FocusedView {
                selection: selection.clone(),
                room_id: room_id.to_owned(),
                timeline: Arc::new(timeline),
            };
            let account_id = selection.account_id().to_owned();
            let timeline = view.timeline.clone();
            let feed = message_feed_from_timeline(account_id, room_id, timeline).await;
            Ok((view, feed))
        })
        .await
    }

    /// Page exactly `view` to older (`backwards`) or newer messages under the
    /// caller's selection, which must be the very selection epoch that loaded
    /// the view and still current. Returns true when that end of the room
    /// was reached.
    pub async fn paginate_focused_selected(
        &self,
        selection: &AccountSelection,
        view: &FocusedView,
        backwards: bool,
        count: u16,
    ) -> Result<bool> {
        if !view.owned_by(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let timeline = view.timeline.clone();
        self.read_selected_timeline(selection, |_| async move {
            let reached = if backwards {
                timeline.paginate_backwards(count).await
            } else {
                timeline.paginate_forwards(count).await
            };
            reached.map_err(|error| MatrixCoreError::Sdk(error.to_string()))
        })
        .await
    }
    fn current_timeline_selection(&self) -> Result<AccountSelection> {
        let account_id = self
            .active_tx
            .borrow()
            .as_ref()
            .map(|account| account.id.clone())
            .ok_or(MatrixCoreError::NoActiveAccount)?;
        self.account_selection(&account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)
    }

    /// Native UI friendly message snapshot for the account selected when polled.
    /// The manager lock is released before timeline loading or subscription.
    pub async fn messages_snapshot(&self, room_id: &str) -> Result<Vec<MessageInfo>> {
        let selection = self.current_timeline_selection()?;
        self.read_selected_timeline(&selection, |loaded| async move {
            let timeline = loaded.timeline(room_id).await?;
            let (items, _) = timeline.subscribe().await;
            Ok(items.iter().filter_map(|item| message_info(item)).collect())
        })
        .await
    }

    /// Live message snapshots for the account selected when polled. Desktop
    /// navigation should retain its exact lease and use message_feed_selected.
    pub async fn message_feed(&self, room_id: &str) -> Result<MessageFeed> {
        let selection = self.current_timeline_selection()?;
        self.message_feed_selected(&selection, room_id).await
    }

    /// Open a room feed under the exact account selection captured by the caller.
    /// A switch, removal or stop cancels loading, even for A -> B -> A.
    /// Discard the returned feed when that selection or room changes.
    pub async fn message_feed_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> Result<MessageFeed> {
        self.read_selected_timeline(selection, |loaded| async move {
            let timeline = loaded.timeline(room_id).await?;
            Ok(
                message_feed_from_timeline(selection.account_id().to_owned(), room_id, timeline)
                    .await,
            )
        })
        .await
    }

    /// Account-bound compatibility entry point for a thread feed. Desktop
    /// navigation should use thread_message_feed_selected to retain its lease.
    pub async fn thread_message_feed_for(
        &self,
        expected_account_id: &str,
        room_id: &str,
        root_event_id: &str,
    ) -> Result<MessageFeed> {
        let selection = self
            .account_selection(expected_account_id)
            .ok_or(MatrixCoreError::SelectedAccountChanged)?;
        self.thread_message_feed_selected(&selection, room_id, root_event_id)
            .await
    }

    /// Open a thread feed under one exact selection, with the same cancellation
    /// and lock ownership as room feeds. Discard it when its target changes.
    pub async fn thread_message_feed_selected(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        root_event_id: &str,
    ) -> Result<MessageFeed> {
        self.read_selected_timeline(selection, |loaded| async move {
            let timeline = loaded.thread_timeline(room_id, root_event_id).await?;
            Ok(
                message_feed_from_timeline(selection.account_id().to_owned(), room_id, timeline)
                    .await,
            )
        })
        .await
    }

    fn owns_timeline_selection(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| selection.same_epoch(&current))
    }

    // Private to this module: read-only operations only. A mutation already
    // submitted to an OS service or homeserver cannot be undone by cancellation.
    async fn read_selected_timeline<T, F, Wait>(
        &self,
        selection: &AccountSelection,
        request: F,
    ) -> Result<T>
    where
        F: FnOnce(Arc<LoadedAccount>) -> Wait,
        Wait: Future<Output = Result<T>>,
    {
        if !self.owns_timeline_selection(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let loaded = {
            let state = selection
                .guard(self.state.lock())
                .await
                .ok_or(MatrixCoreError::SelectedAccountChanged)?;
            // The lock acquisition can itself wait through account transitions.
            if !self.owns_timeline_selection(selection) {
                return Err(MatrixCoreError::SelectedAccountChanged);
            }
            selected_loaded_for(&state, selection.account_id())?.clone()
        };
        selection
            .guard(async move {
                if !selection.is_current() {
                    return Err(MatrixCoreError::SelectedAccountChanged);
                }
                start_on_heap(request, loaded).await
            })
            .await
            .ok_or(MatrixCoreError::SelectedAccountChanged)?
    }
}

/// Own a started timeline request on the heap. Held inline, the SDK timeline
/// open was embedded in every public read future and its callers; unoptimized
/// poll frames then exhausted the 2 MiB test-thread stack. Building it in this
/// short-lived frame keeps the temporary out of the frame that polls it.
/// Dropping the box cancels the request and releases its loaded-client hold.
fn start_on_heap<F, Wait>(request: F, loaded: Arc<LoadedAccount>) -> Pin<Box<Wait>>
where
    F: FnOnce(Arc<LoadedAccount>) -> Wait,
{
    Box::pin(request(loaded))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    async fn fixture(account: Option<AccountInfo>) -> (PathBuf, Arc<AccountManager>, AccountInfo) {
        let directory =
            std::env::temp_dir().join(format!("morpheus-history-read-{}", Uuid::new_v4()));
        let manager = Arc::new(AccountManager::open(&directory).unwrap());
        let account = account.unwrap_or_else(|| AccountInfo {
            id: Uuid::new_v4().to_string(),
            user_id: "@casey:example.org".into(),
            device_id: "HISTORY-FIXTURE".into(),
            homeserver: "http://127.0.0.1:9".into(),
        });
        // No authenticated session, vault entry, sync or persistence task.
        let client = Client::builder()
            .homeserver_url(&account.homeserver)
            .build()
            .await
            .unwrap();
        client.send_queue().set_enabled(false).await;
        let loaded = Arc::new(LoadedAccount {
            client,
            timelines: Mutex::new(HashMap::new()),
            composed_edits: Mutex::new(HashMap::new()),
            pending_sends: Mutex::new(HashMap::new()),
            originals: Mutex::new(HashMap::new()),
            recovered: Mutex::new(Vec::new()),
            recovery_gate: Mutex::new(RecoveryGate::default()),
            recovery_ready: watch::channel(false).0,
            incoming_verification_tx: watch::channel(Vec::new()).0,
            notification_handler_registered: Mutex::new(false),
            token_task: Mutex::new(None),
        });
        {
            let mut state = manager.state.lock().await;
            state.registry.accounts.push(account.clone());
            state.registry.active_account_id = Some(account.id.clone());
            state.active_account_id = Some(account.id.clone());
            state.loaded.insert(account.id.clone(), loaded);
        }
        manager.publish_active_account(Some(account.clone()));
        (directory, manager, account)
    }

    #[test]
    fn stalled_history_does_not_lock_account_controls_and_revocation_releases_its_hold() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let lifetime = {
                let state = manager.state.lock().await;
                Arc::downgrade(state.loaded.get(&account.id).unwrap())
            };
            assert_eq!(lifetime.strong_count(), 1);
            let (started, received) = oneshot::channel();
            let worker_manager = manager.clone();
            let task = tokio::spawn(async move {
                worker_manager
                    .read_selected_timeline(&selection, move |loaded| async move {
                        let _held_client = loaded;
                        started.send(()).unwrap();
                        std::future::pending::<Result<bool>>().await
                    })
                    .await
            });
            tokio::time::timeout(Duration::from_secs(2), received)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(lifetime.strong_count(), 2);
            let accounts = tokio::time::timeout(Duration::from_secs(2), manager.accounts())
                .await
                .expect("history network wait must not retain the manager state lock");
            assert_eq!(accounts, [account.clone()]);
            // Publish a transition while holding the same state mutex account
            // activation/removal need. No history event will ever arrive.
            {
                let mut state = manager.state.lock().await;
                state.active_account_id = None;
                manager.publish_active_account(None);
            }
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), task)
                    .await
                    .unwrap()
                    .unwrap(),
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert_eq!(
                lifetime.strong_count(),
                1,
                "the revoked read drops its loaded-client hold"
            );
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn history_rejects_foreign_and_aba_selections_before_starting_a_request() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let (foreign_directory, foreign, _) = fixture(Some(account.clone())).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let called = calls.clone();
            assert!(matches!(
                foreign
                    .read_selected_timeline(&selection, move |_| {
                        called.fetch_add(1, Ordering::SeqCst);
                        async { Ok(true) }
                    })
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            {
                let mut state = manager.state.lock().await;
                let other = AccountInfo {
                    id: Uuid::new_v4().to_string(),
                    ..account.clone()
                };
                state.active_account_id = Some(other.id.clone());
                manager.publish_active_account(Some(other));
                state.active_account_id = Some(account.id.clone());
                manager.publish_active_account(Some(account.clone()));
            }
            assert!(matches!(
                manager
                    .paginate_backwards_selected(&selection, "!unused:example.org", 50)
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            let called = calls.clone();
            assert!(matches!(
                manager
                    .read_selected_timeline(&selection, move |_| {
                        called.fetch_add(1, Ordering::SeqCst);
                        async { Ok(true) }
                    })
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            drop(foreign);
            drop(manager);
            fs::remove_dir_all(foreign_directory).unwrap();
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn search_rejects_an_aba_selection_and_keeps_the_current_one() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let stale = manager.account_selection(&account.id).unwrap();
            // A fresh selection of the same local account is a new epoch.
            manager.publish_active_account(None);
            manager.publish_active_account(Some(account.clone()));
            let (room, event) = ("!unused:example.org", "$event:example.org");
            assert!(matches!(
                manager
                    .search_all_messages_page_selected(&stale, "plans", 10, 0)
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert!(matches!(
                manager
                    .search_messages_page_selected(&stale, room, "plans", 10, 0)
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert!(matches!(
                manager
                    .search_result_context_selected(&stale, room, event, 3)
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            // The current selection searches its own (empty) room set and
            // reports no further page.
            let current = manager.account_selection(&account.id).unwrap();
            let page = manager
                .search_all_messages_page_selected(&current, "plans", 10, 0)
                .await
                .unwrap();
            assert_eq!(page, MessageSearchPage::default());
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn a_focused_view_is_owned_only_by_the_selection_that_loaded_it() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let loading = manager.account_selection(&account.id).unwrap();
            assert!(view_owned_by(&loading, &loading));
            // The same account selected again (A -> B -> A, removal and
            // signing in again) is a new epoch: it cannot use the old view.
            manager.publish_active_account(None);
            manager.publish_active_account(Some(account.clone()));
            let fresh = manager.account_selection(&account.id).unwrap();
            assert!(!view_owned_by(&loading, &fresh));
            assert!(!view_owned_by(&loading, &loading));
            assert!(view_owned_by(&fresh, &fresh));
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn history_revalidates_selection_after_waiting_for_the_state_lock() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let called = calls.clone();
            let state = manager.state.lock().await;
            let mut wait = Box::pin(manager.read_selected_timeline(&selection, move |_| {
                called.fetch_add(1, Ordering::SeqCst);
                async { Ok(true) }
            }));
            let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(wait.as_mut().poll(&mut context).is_pending());
            // A fresh selection of the same ID still invalidates the queued
            // request that is demonstrably waiting on the state lock.
            manager.publish_active_account(None);
            manager.publish_active_account(Some(account));
            drop(state);
            assert!(matches!(
                wait.await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn history_keeps_current_results_and_errors_but_discards_a_revoked_completion() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            assert!(
                manager
                    .read_selected_timeline(&selection, |_| async { Ok(true) })
                    .await
                    .unwrap()
            );
            assert!(matches!(
                manager
                    .read_selected_timeline(&selection, |_| async {
                        Err::<bool, _>(MatrixCoreError::RoomNotFound("!missing:example.org".into()))
                    })
                    .await,
                Err(MatrixCoreError::RoomNotFound(_))
            ));
            let revoked = manager.clone();
            assert!(matches!(
                manager
                    .read_selected_timeline(&selection, move |_| async move {
                        revoked.publish_active_account(None);
                        Ok(true)
                    })
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert!(matches!(
                manager.paginate_backwards("!unused:example.org", 50).await,
                Err(MatrixCoreError::NoActiveAccount)
            ));
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn stalled_room_thread_and_snapshot_opens_release_account_controls() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let loaded = {
                let state = manager.state.lock().await;
                state.loaded.get(&account.id).unwrap().clone()
            };
            // Stall at the real LoadedAccount timeline cache, before an offline
            // room lookup could fail. No network or arbitrary sleep is needed.
            let timeline_cache = loaded.timelines.lock().await;
            for kind in 0..3 {
                {
                    let mut state = manager.state.lock().await;
                    state.active_account_id = Some(account.id.clone());
                    manager.publish_active_account(Some(account.clone()));
                }
                let selection = manager.account_selection(&account.id).unwrap();
                assert_eq!(Arc::strong_count(&loaded), 2);
                let mut opening = Box::pin(async {
                    match kind {
                        0 => manager.messages_snapshot("!waiting:example.org").await.map(drop),
                        1 => manager.message_feed_selected(&selection, "!waiting:example.org").await.map(drop),
                        _ => manager.thread_message_feed_selected(&selection, "!waiting:example.org", "$root").await.map(drop),
                    }
                });
                let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
                assert!(opening.as_mut().poll(&mut context).is_pending());
                assert_eq!(Arc::strong_count(&loaded), 3);
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(2), manager.accounts()).await
                        .expect("a stalled timeline open must release the manager mutex"),
                    [account.clone()]
                );
                {
                    let mut state = manager.state.lock().await;
                    state.active_account_id = None;
                    manager.publish_active_account(None);
                }
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(2), opening).await.unwrap(),
                    Err(MatrixCoreError::SelectedAccountChanged)
                ));
                assert_eq!(Arc::strong_count(&loaded), 2, "revocation releases the loaded client while its timeline cache is still blocked");
            }
            drop(timeline_cache);
            drop(loaded);
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn feed_open_rejects_foreign_and_revoked_account_selection_before_room_lookup() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let (foreign_directory, foreign, _) = fixture(Some(account.clone())).await;
            let stale = manager.account_selection(&account.id).unwrap();
            manager.publish_active_account(None);
            manager.publish_active_account(Some(account));
            let current = manager.current_timeline_selection().unwrap();
            for (target, selection) in [(&*manager, &stale), (&*foreign, &current)] {
                assert!(matches!(
                    target.message_feed_selected(selection, "not-a-room").await,
                    Err(MatrixCoreError::SelectedAccountChanged)
                ));
                assert!(matches!(
                    target
                        .thread_message_feed_selected(selection, "not-a-room", "not-an-event")
                        .await,
                    Err(MatrixCoreError::SelectedAccountChanged)
                ));
            }
            drop(foreign);
            drop(manager);
            fs::remove_dir_all(foreign_directory).unwrap();
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn revoked_timeline_completion_drops_its_owned_result() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let result = Arc::new(());
            let lifetime = Arc::downgrade(&result);
            let revoked = manager.clone();
            assert!(matches!(
                manager
                    .read_selected_timeline(&selection, move |_| async move {
                        revoked.publish_active_account(None);
                        Ok(result)
                    })
                    .await,
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            assert!(lifetime.upgrade().is_none());
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    #[test]
    fn revoked_timeline_read_stops_while_the_manager_mutex_is_still_held() {
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let state = manager.state.lock().await;
            let mut opening =
                Box::pin(manager.message_feed_selected(&selection, "!unused:example.org"));
            let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(opening.as_mut().poll(&mut context).is_pending());
            manager.publish_active_account(None);
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(2), opening)
                    .await
                    .unwrap(),
                Err(MatrixCoreError::SelectedAccountChanged)
            ));
            drop(state);
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }

    /// Size of the future a call would return; the call is never made.
    fn future_size<Fut: Future>(_call: impl FnOnce() -> Fut) -> usize {
        std::mem::size_of::<Fut>()
    }

    #[test]
    fn public_timeline_reads_keep_unpolled_futures_within_budget() {
        // Unoptimized poll frames reserve stack for each future they build, so
        // an SDK timeline open held inline by these wrappers overflowed callers
        // awaiting several reads. Staying below the SDK open itself also fails
        // a wrapper that embeds it while that open happens to fit the budget.
        const BUDGET: usize = 16 * 1024;
        runtime().block_on(async {
            let (directory, manager, account) = fixture(None).await;
            let selection = manager.account_selection(&account.id).unwrap();
            let loaded = {
                let state = manager.state.lock().await;
                state.loaded.get(&account.id).unwrap().clone()
            };
            let (id, room, root) = (account.id.as_str(), "!unused:example.org", "$root");
            let sdk_open = future_size(|| loaded.timeline(room))
                .min(future_size(|| loaded.thread_timeline(room, root)));
            let reads = [
                (
                    "messages_snapshot",
                    future_size(|| manager.messages_snapshot(room)),
                ),
                ("message_feed", future_size(|| manager.message_feed(room))),
                (
                    "message_feed_selected",
                    future_size(|| manager.message_feed_selected(&selection, room)),
                ),
                (
                    "thread_message_feed_for",
                    future_size(|| manager.thread_message_feed_for(id, room, root)),
                ),
                (
                    "thread_message_feed_selected",
                    future_size(|| manager.thread_message_feed_selected(&selection, room, root)),
                ),
                (
                    "paginate_backwards",
                    future_size(|| manager.paginate_backwards(room, 50)),
                ),
                (
                    "paginate_backwards_for",
                    future_size(|| manager.paginate_backwards_for(id, room, 50)),
                ),
                (
                    "paginate_backwards_selected",
                    future_size(|| manager.paginate_backwards_selected(&selection, room, 50)),
                ),
            ];
            for (method, bytes) in reads {
                assert!(
                    bytes <= BUDGET && bytes < sdk_open,
                    "{method} future is {bytes} bytes; budget {BUDGET}, SDK open {sdk_open}"
                );
            }
            drop(loaded);
            drop(manager);
            fs::remove_dir_all(directory).unwrap();
        });
    }
}

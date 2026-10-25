//! UI-thread timeline storage with lazy rich-text projection and stable models.
//!
//! SDK snapshots share immutable rows. Comparing them here avoids
//! rebuilding unchanged Slint rows; only row_data formats a requested row.
//! Account, room and feed-generation guards remain at the publication callers.

use std::{
    any::Any,
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::{Rc, Weak},
    sync::Arc,
};

use morpheus_matrix::MessageInfo;
use slint::{Model, ModelNotify, ModelRc, ModelTracker};

use crate::MessageItem;

// Bound synchronous per-row notifications for bulk replacements. Inserts and
// removals each use one range notification regardless of the history length.
const MAX_ROW_NOTIFICATIONS: usize = 128;

struct TimelineModel {
    weak: Weak<Self>,
    copy_scope: RefCell<Option<Rc<crate::message_copy::CopyScope>>>,
    copy_projected: RefCell<VecDeque<usize>>,
    rows: RefCell<Vec<Arc<MessageInfo>>>,
    // Built only by event lookup, owned by this exact model, and never larger
    // than the loaded row set. Duplicate event IDs keep their first position.
    event_index: RefCell<Option<HashMap<String, usize>>>,
    notify: ModelNotify,
    #[cfg(test)]
    indexed_rows: std::cell::Cell<usize>,
    #[cfg(test)]
    projected: std::cell::Cell<usize>,
}

impl TimelineModel {
    fn event_position(&self, event_id: &str) -> Option<usize> {
        let missing = self.event_index.borrow().is_none();
        if missing {
            let rows = self.rows.borrow();
            let mut index = HashMap::new();
            for (position, row) in rows.iter().enumerate() {
                #[cfg(test)]
                self.indexed_rows.set(self.indexed_rows.get() + 1);
                if let Some(event) = row.event_id.as_ref() {
                    index.entry(event.clone()).or_insert(position);
                }
            }
            *self.event_index.borrow_mut() = Some(index);
        }
        self.event_index
            .borrow()
            .as_ref()
            .and_then(|index| index.get(event_id).copied())
    }

    fn invalidate_event_index(&self) {
        self.event_index.borrow_mut().take();
    }

    // Re-arm only the bounded recent projections. This never resets history
    // geometry, scroll or drafts just to refresh opaque Copy metadata.
    fn notify_copy_rows(&self) {
        let rows: Vec<_> = self
            .copy_projected
            .borrow()
            .iter()
            .copied()
            .filter(|row| *row < self.rows.borrow().len())
            .collect();
        for row in rows {
            self.notify.row_changed(row);
        }
    }
    fn replace(&self, next: Cow<'_, [Arc<MessageInfo>]>) {
        let (prefix, removed, added) = {
            let old = self.rows.borrow();
            let prefix = old
                .iter()
                .zip(next.iter())
                .take_while(|(a, b)| same_row(a, b))
                .count();
            if prefix == old.len() && prefix == next.len() {
                return;
            }
            let suffix = old[prefix..]
                .iter()
                .rev()
                .zip(next[prefix..].iter().rev())
                .take_while(|(a, b)| same_row(a, b))
                .count();
            (
                prefix,
                old.len() - prefix - suffix,
                next.len() - prefix - suffix,
            )
        };

        if removed == added {
            // A wide changed span can contain only a few actual edits. Keep
            // unchanged delegates and payloads intact, with bounded tracking
            // even when every row in a long history changes.
            let changed: Vec<_> = {
                let rows = self.rows.borrow();
                rows[prefix..prefix + removed]
                    .iter()
                    .zip(&next[prefix..prefix + added])
                    .enumerate()
                    .filter_map(|(offset, (old, new))| {
                        (!same_row(old, new)).then_some(prefix + offset)
                    })
                    .take(MAX_ROW_NOTIFICATIONS + 1)
                    .collect()
            };
            if changed.len() > MAX_ROW_NOTIFICATIONS {
                self.invalidate_event_index();
                *self.rows.borrow_mut() = next.into_owned();
                self.notify.reset();
                return;
            }
            let identifiers_changed = {
                let rows = self.rows.borrow();
                changed
                    .iter()
                    .any(|&row| rows[row].event_id != next[row].event_id)
            };
            if identifiers_changed {
                self.invalidate_event_index();
            }
            {
                let mut rows = self.rows.borrow_mut();
                for &row in &changed {
                    rows[row] = next[row].clone();
                }
            }
            for row in changed {
                self.notify.row_changed(row);
            }
        } else {
            // Release each borrow before notifying. A listener may immediately
            // read row_count/data and must see a valid intermediate model.
            if removed != 0 {
                self.invalidate_event_index();
                drop(self.rows.borrow_mut().drain(prefix..prefix + removed));
                self.notify.row_removed(prefix, removed);
            }
            if added != 0 {
                // Take before changing rows: a panic must leave no stale cache.
                // A pure append preserves every old position; any other insert
                // invalidates it. Do not build an index nobody has requested.
                let mut index = self.event_index.borrow_mut().take();
                if prefix != self.rows.borrow().len() {
                    index = None;
                }
                match next {
                    Cow::Borrowed(next) => drop(
                        self.rows
                            .borrow_mut()
                            .splice(prefix..prefix, next[prefix..prefix + added].iter().cloned()),
                    ),
                    Cow::Owned(next) => drop(
                        self.rows
                            .borrow_mut()
                            .splice(prefix..prefix, next.into_iter().skip(prefix).take(added)),
                    ),
                }
                if let Some(index) = index.as_mut() {
                    let rows = self.rows.borrow();
                    for (offset, row) in rows[prefix..prefix + added].iter().enumerate() {
                        let position = prefix + offset;
                        #[cfg(test)]
                        self.indexed_rows.set(self.indexed_rows.get() + 1);
                        if let Some(event) = row.event_id.as_ref() {
                            index.entry(event.clone()).or_insert(position);
                        }
                    }
                }
                *self.event_index.borrow_mut() = index;
                self.notify.row_added(prefix, added);
            }
        }
    }
}

fn same_row(a: &Arc<MessageInfo>, b: &Arc<MessageInfo>) -> bool {
    Arc::ptr_eq(a, b) || a.as_ref() == b.as_ref()
}

impl Model for TimelineModel {
    type Data = MessageItem;

    fn row_count(&self) -> usize {
        self.rows.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<MessageItem> {
        let rows = self.rows.borrow();
        let info = rows.get(row)?;
        {
            let mut recent = self.copy_projected.borrow_mut();
            if let Some(old) = recent.iter().position(|index| *index == row) {
                recent.remove(old);
            }
            if recent.len() == MAX_ROW_NOTIFICATIONS {
                recent.pop_front();
            }
            recent.push_back(row);
        }
        #[cfg(test)]
        self.projected.set(self.projected.get() + 1);
        let mut item = crate::live::message_item(info);
        item.copy_token = crate::message_copy::row_token(
            self.weak.clone(),
            self.copy_scope.borrow().as_ref(),
            row,
            info,
        )
        .into();
        Some(item)
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Apply a snapshot to an existing timeline model or replace a non-timeline
/// model (including an account-detach/room-change blank) with a fresh one.
#[cfg(test)]
pub(crate) fn update_shared(
    current: ModelRc<MessageItem>,
    rows: Vec<Arc<MessageInfo>>,
) -> ModelRc<MessageItem> {
    // Unscoped/test snapshots deliberately carry no Copy capability.
    update_scoped(current, rows, None)
}

/// The actual feed publication's pre-guard captured original capability.
pub(crate) fn update_scoped(
    current: ModelRc<MessageItem>,
    rows: Vec<Arc<MessageInfo>>,
    scope: Option<Rc<crate::message_copy::CopyScope>>,
) -> ModelRc<MessageItem> {
    update_rows_scoped(current, Cow::Owned(rows), scope)
}

/// Borrow the coalesced feed's immutable snapshot on the UI thread. Unchanged
/// rows already belong to the model; only inserted or edited rows need new
/// handles, unless a new model or a bulk reset replaces the whole history.
pub(crate) fn update_snapshot_scoped(
    current: ModelRc<MessageItem>,
    rows: &[Arc<MessageInfo>],
    scope: Option<Rc<crate::message_copy::CopyScope>>,
) -> ModelRc<MessageItem> {
    update_rows_scoped(current, Cow::Borrowed(rows), scope)
}

fn update_rows_scoped(
    current: ModelRc<MessageItem>,
    rows: Cow<'_, [Arc<MessageInfo>]>,
    scope: Option<Rc<crate::message_copy::CopyScope>>,
) -> ModelRc<MessageItem> {
    if let Some(model) = current.as_any().downcast_ref::<TimelineModel>() {
        // Source replacement precedes a reset that can synchronously project.
        model.replace(rows);
        let reuse = {
            let old = model.copy_scope.borrow();
            old.as_ref()
                .zip(scope.as_ref())
                .filter(|(a, b)| crate::message_copy::same_scope(a, b))
                .map(|(old, _)| old.clone())
        };
        let next = reuse.or(scope);
        let same = match (model.copy_scope.borrow().as_ref(), next.as_ref()) {
            (Some(a), Some(b)) => Rc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            *model.copy_scope.borrow_mut() = next;
            model.notify_copy_rows();
        }
        current
    } else {
        let model = Rc::new_cyclic(|weak| TimelineModel {
            weak: weak.clone(),
            copy_scope: RefCell::new(scope),
            copy_projected: RefCell::new(VecDeque::new()),
            rows: RefCell::new(rows.into_owned()),
            event_index: RefCell::new(None),
            notify: ModelNotify::default(),
            #[cfg(test)]
            indexed_rows: std::cell::Cell::new(0),
            #[cfg(test)]
            projected: std::cell::Cell::new(0),
        });
        ModelRc::from(model)
    }
}

/// A modal/context callback can only renew an already-bound exact target;
/// it cannot turn old-account rows into a fresh publication.
pub(crate) fn refresh_copy_context(current: &ModelRc<MessageItem>) {
    let Some(model) = current.as_any().downcast_ref::<TimelineModel>() else {
        return;
    };
    let next = crate::message_copy::context_scope(model.copy_scope.borrow().as_ref());
    let same = match (model.copy_scope.borrow().as_ref(), next.as_ref()) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    };
    if !same {
        *model.copy_scope.borrow_mut() = next;
        model.notify_copy_rows();
    }
}

/// Position of the row showing `event_id`. A timeline model is read from its
/// stored rows without projecting any row.
pub(crate) fn index_of_event(model: &ModelRc<MessageItem>, event_id: &str) -> Option<usize> {
    if let Some(model) = model.as_any().downcast_ref::<TimelineModel>() {
        return model.event_position(event_id);
    }
    model
        .iter()
        .position(|row| row.event_id.as_str() == event_id)
}

/// Whether the row at `index` shows `event_id`: a timeline model reads its
/// stored row without projecting it, any other model projects that row only.
/// A seek checks its row this way before it searches the whole history.
pub(crate) fn has_event_at(model: &ModelRc<MessageItem>, index: usize, event_id: &str) -> bool {
    if let Some(model) = model.as_any().downcast_ref::<TimelineModel>() {
        return model
            .rows
            .borrow()
            .get(index)
            .is_some_and(|row| row.event_id.as_deref() == Some(event_id));
    }
    model
        .row_data(index)
        .is_some_and(|row| row.event_id.as_str() == event_id)
}

/// Exact row identity without projection or an event-ID search.
pub(crate) fn has_exact_row(
    model: &ModelRc<MessageItem>,
    index: usize,
    row: &Arc<MessageInfo>,
) -> bool {
    model
        .as_any()
        .downcast_ref::<TimelineModel>()
        .is_some_and(|model| {
            model
                .rows
                .borrow()
                .get(index)
                .is_some_and(|shown| Arc::ptr_eq(shown, row))
        })
}

/// The row showing `event_id`, projecting only that row.
pub(crate) fn item_of_event(model: &ModelRc<MessageItem>, event_id: &str) -> Option<MessageItem> {
    model.row_data(index_of_event(model, event_id)?)
}

/// The stored row for a sent event, without projecting any row. Controllers
/// check a click against the snapshot the person actually saw.
pub(crate) fn info_of_event(
    model: &ModelRc<MessageItem>,
    event_id: &str,
) -> Option<Arc<MessageInfo>> {
    let model = model.as_any().downcast_ref::<TimelineModel>()?;
    let position = model.event_position(event_id)?;
    model.rows.borrow().get(position).cloned()
}

/// Re-project the rows whose source matches, for presentation state kept
/// outside `MessageInfo` (pinned flags). Bounded like a snapshot replacement.
pub(crate) fn refresh_rows(current: &ModelRc<MessageItem>, matches: impl Fn(&MessageInfo) -> bool) {
    let Some(model) = current.as_any().downcast_ref::<TimelineModel>() else {
        return;
    };
    let rows: Vec<usize> = model
        .rows
        .borrow()
        .iter()
        .enumerate()
        .filter(|(_, row)| matches(row))
        .map(|(index, _)| index)
        .take(MAX_ROW_NOTIFICATIONS + 1)
        .collect();
    if rows.len() > MAX_ROW_NOTIFICATIONS {
        model.notify.reset();
        return;
    }
    for row in rows {
        model.notify.row_changed(row);
    }
}

// Owned fixtures also exercise the value-equality fallback: a rebuilt row can
// be equal even when it has a different allocation from the previous snapshot.
#[cfg(test)]
pub(crate) fn update(
    current: ModelRc<MessageItem>,
    rows: Vec<MessageInfo>,
) -> ModelRc<MessageItem> {
    update_shared(current, rows.into_iter().map(Arc::new).collect())
}

#[cfg(test)]
pub(crate) fn projection_count(model: &ModelRc<MessageItem>) -> usize {
    model
        .as_any()
        .downcast_ref::<TimelineModel>()
        .unwrap()
        .projected
        .get()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use morpheus_matrix::MessageSendState;
    use slint::private_unstable_api::re_exports::{
        ModelChangeListener, ModelChangeListenerContainer,
    };
    use std::pin::Pin;

    pub(crate) fn fixture(index: usize) -> MessageInfo {
        let body = format!(
            "Timeline fixture {index} {}",
            "message".repeat(index % 4 + 1)
        );
        MessageInfo {
            key: format!("fixture-{index}"),
            event_id: Some(format!("$fixture-{index}:example.org")),
            sender: "@fixture:example.org".into(),
            rich_markdown: Some(body.clone()),
            rich_accessible: Some(body.clone()),
            body,
            attachment_name: None,
            attachment_is_image: false,
            attachment_is_audio: false,
            poll: None,
            reply_preview: None,
            reaction_summary: String::new(),
            timestamp_ms: 0,
            is_own: false,
            is_local_echo: false,
            send_state: MessageSendState::Sent,
        }
    }

    /// Mixed conversation shapes for the real generated ListView. Keep the
    /// small equality/notification fixtures separate so their contracts remain
    /// easy to inspect; this workload exercises width-dependent text layout.
    pub(crate) fn rich_fixture(index: usize) -> MessageInfo {
        let mut row = fixture(index);
        row.body = match index % 6 {
            0 => format!("Short timeline message {index}."),
            1 => format!("Timeline plan {index}\nReview the room settings.\nConfirm the next check."),
            2 => format!(
                "Workshop notes {index}: {}",
                "A longer conversation should wrap naturally as the reading width changes, while nearby messages retain their own heights. ".repeat(4)
            ),
            3 => format!("Build output {index}\nfn ready() {{\n    prepare();\n    verify();\n}}"),
            4 => format!("Sharing the revised room layout {index}."),
            _ => format!("Checklist {index}\n- Review contrast\n- Check keyboard focus\n- Keep the selected alias visible"),
        };
        row.rich_markdown = Some(if index % 6 == 3 {
            format!(
                "Build output {index}\n\n```rust\nfn ready() {{\n    prepare();\n    verify();\n}}\n```"
            )
        } else {
            // Match Matrix hard line breaks rather than Markdown soft wraps.
            row.body.replace('\n', "  \n")
        });
        row.rich_accessible = Some(row.body.clone());
        row.sender = [
            "@casey:example.org",
            "@morgan:example.org",
            "@jules:example.org",
            "@sam:example.org",
        ][index % 4]
            .into();
        row.timestamp_ms = 1_700_000_000_000 + index as u64 * 60_000;
        if index % 5 == 4 {
            row.attachment_name = Some(format!("layout-notes-{index}.pdf"));
        }
        if index % 7 == 4 {
            row.reply_preview = Some("Morgan: Please review the revised reading width.".into());
        }
        if index % 4 == 0 {
            row.reaction_summary = "✓  3".into();
        }
        row
    }

    #[test]
    fn copy_capability_requires_the_exact_model_row_and_index_after_edits_or_moves() {
        let original = Arc::new(fixture(1));
        let second = Arc::new(fixture(2));
        let model = update_shared(ModelRc::default(), vec![original.clone(), second.clone()]);
        assert!(has_exact_row(&model, 0, &original));
        let mut edited = original.as_ref().clone();
        edited.body = "Edited accepted text".into();
        edited.rich_markdown = None;
        edited.rich_accessible = None;
        let edited = Arc::new(edited);
        let model = update_shared(model, vec![edited.clone(), second.clone()]);
        assert!(
            !has_exact_row(&model, 0, &original),
            "same event ID must not revive the old row"
        );
        assert!(has_exact_row(&model, 0, &edited));
        let other_model = update_shared(ModelRc::default(), vec![original.clone()]);
        assert!(has_exact_row(&other_model, 0, &original));
        assert!(!has_exact_row(&model, 0, &original));
        let model = update_shared(model, vec![second, edited.clone()]);
        assert!(
            !has_exact_row(&model, 0, &edited),
            "an old index is not the moved row"
        );
        assert!(has_exact_row(&model, 1, &edited));
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Change {
        Changed(usize, usize),
        Added(usize, usize, usize),
        Removed(usize, usize, usize),
        Reset(usize),
    }

    struct Observer {
        model: ModelRc<MessageItem>,
        changes: RefCell<Vec<Change>>,
    }

    impl Observer {
        fn rows(&self) -> usize {
            let count = self.model.row_count();
            if count > 0 {
                // Notification callbacks can reenter the model immediately.
                assert!(self.model.row_data(0).is_some());
            }
            assert!(self.model.row_data(count).is_none());
            count
        }
    }

    impl ModelChangeListener for Observer {
        fn row_changed(self: Pin<&Self>, row: usize) {
            assert!(row < self.model.row_count());
            let count = self.rows();
            self.changes.borrow_mut().push(Change::Changed(row, count));
        }
        fn row_added(self: Pin<&Self>, index: usize, count: usize) {
            assert!(index + count <= self.model.row_count());
            let rows = self.rows();
            self.changes
                .borrow_mut()
                .push(Change::Added(index, count, rows));
        }
        fn row_removed(self: Pin<&Self>, index: usize, count: usize) {
            assert!(index <= self.model.row_count());
            let rows = self.rows();
            self.changes
                .borrow_mut()
                .push(Change::Removed(index, count, rows));
        }
        fn reset(self: Pin<&Self>) {
            let rows = self.rows();
            self.changes.borrow_mut().push(Change::Reset(rows));
        }
    }

    fn assert_rows(model: &ModelRc<MessageItem>, expected: &[MessageInfo]) {
        let actual = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        assert!(
            actual
                .rows
                .borrow()
                .iter()
                .map(|row| row.as_ref())
                .eq(expected.iter())
        );
        assert_eq!(model.row_count(), expected.len());
    }

    #[test]
    fn ten_thousand_rich_rows_project_only_requested_data_and_keep_model_identity() {
        let rows: Vec<_> = (0..10_000).map(fixture).collect();
        let model = update(ModelRc::default(), rows.clone());
        assert_eq!(model.row_count(), 10_000);
        assert_eq!(projection_count(&model), 0);
        let last = model.row_data(9_999).unwrap();
        assert_eq!(last.body.as_str(), rows[9_999].body);
        assert!(last.edit_markdown);
        assert_eq!(projection_count(&model), 1);
        assert!(model.row_data(10_000).is_none());
        let same = update(model.clone(), rows);
        assert_eq!(same, model);
        assert_eq!(projection_count(&same), 1);
    }

    /// A seek's row check reads stored rows only: no row of a 10,000-row
    /// mixed history is formatted, and another row, an index past the end or
    /// a row without an event (a local echo) matches nothing.
    #[test]
    fn a_row_check_reads_stored_rows_without_projecting_them() {
        let mut rows: Vec<_> = (0..10_000).map(rich_fixture).collect();
        rows[9_999].event_id = None;
        let model = update(ModelRc::default(), rows);
        assert!(has_event_at(&model, 7_654, "$fixture-7654:example.org"));
        assert!(!has_event_at(&model, 7_655, "$fixture-7654:example.org"));
        assert!(!has_event_at(&model, 10_000, "$fixture-7654:example.org"));
        assert!(!has_event_at(&model, 9_999, "$fixture-9999:example.org"));
        assert_eq!(projection_count(&model), 0);
    }

    #[test]
    fn copy_rearm_notifies_only_bounded_recent_rows_without_resetting_the_history() {
        let original: Vec<_> = (0..10_000).map(|i| Arc::new(fixture(i))).collect();
        let model = update_shared(ModelRc::default(), original.clone());
        for index in 9_700..10_000 {
            assert!(model.row_data(index).is_some());
        }
        let observer = Box::pin(ModelChangeListenerContainer::new(Observer {
            model: model.clone(),
            changes: RefCell::new(Vec::new()),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());
        let before = projection_count(&model);
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        assert_eq!(stored.copy_projected.borrow().len(), MAX_ROW_NOTIFICATIONS);
        stored.notify_copy_rows();
        let observed = observer.as_ref().get();
        let changes = observed.changes.borrow();
        assert_eq!(changes.len(), MAX_ROW_NOTIFICATIONS);
        assert!(
            changes
                .iter()
                .all(|change| matches!(change, Change::Changed(_, 10_000))),
            "Copy rearm must not reset or resize the actual history"
        );
        assert_eq!(
            projection_count(&model) - before,
            MAX_ROW_NOTIFICATIONS,
            "only the actual observer's bounded reentrant reads project rows"
        );
        assert!(
            stored
                .rows
                .borrow()
                .iter()
                .zip(&original)
                .all(|(a, b)| Arc::ptr_eq(a, b)),
            "Copy metadata refresh preserves all immutable payload allocations"
        );
    }

    #[test]
    fn shared_history_retains_payload_allocations_across_append_and_edit() {
        let original: Vec<_> = (0..10_000).map(|i| Arc::new(fixture(i))).collect();
        let model = update_shared(ModelRc::default(), original.clone());
        let observer = Box::pin(ModelChangeListenerContainer::new(Observer {
            model: model.clone(),
            changes: RefCell::new(Vec::new()),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());

        let mut next = original.clone();
        next.push(Arc::new(fixture(10_000)));
        assert_eq!(update_shared(model.clone(), next.clone()), model);
        let mut edited = next[0].as_ref().clone();
        edited.body = "Edited shared row".into();
        edited.rich_markdown = Some(edited.body.clone());
        edited.rich_accessible = Some(edited.body.clone());
        next[0] = Arc::new(edited);
        update_shared(model.clone(), next.clone());

        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        {
            let rows = stored.rows.borrow();
            assert_eq!(rows.len(), 10_001);
            assert!(
                rows.iter().zip(&next).all(|(a, b)| Arc::ptr_eq(a, b)),
                "the UI must retain published row allocations, not copy payloads"
            );
            assert!(
                rows[1..10_000]
                    .iter()
                    .zip(&original[1..])
                    .all(|(a, b)| Arc::ptr_eq(a, b))
            );
        }
        assert_ne!(original[0].body, "Edited shared row");
        assert_eq!(
            model.row_data(0).unwrap().body.as_str(),
            "Edited shared row"
        );
        assert_eq!(
            *observer.as_ref().get().changes.borrow(),
            [Change::Added(10_000, 1, 10_001), Change::Changed(0, 10_001)]
        );
        assert_eq!(
            projection_count(&model),
            3,
            "only two reentrant observer reads and one explicit read project text"
        );
    }

    #[test]
    fn borrowed_feed_snapshots_keep_notifications_and_do_not_clone_unchanged_handles() {
        struct HandleObserver {
            row: Arc<MessageInfo>,
            counts: RefCell<Vec<usize>>,
        }
        impl ModelChangeListener for HandleObserver {
            fn row_changed(self: Pin<&Self>, _row: usize) {
                self.counts.borrow_mut().push(Arc::strong_count(&self.row));
            }
            fn row_added(self: Pin<&Self>, _index: usize, _count: usize) {
                self.counts.borrow_mut().push(Arc::strong_count(&self.row));
            }
            fn row_removed(self: Pin<&Self>, _index: usize, _count: usize) {
                self.counts.borrow_mut().push(Arc::strong_count(&self.row));
            }
            fn reset(self: Pin<&Self>) {
                self.counts.borrow_mut().push(Arc::strong_count(&self.row));
            }
        }

        let original: Arc<Vec<_>> = Arc::new((0..10_000).map(|i| Arc::new(fixture(i))).collect());
        let model = update_snapshot_scoped(ModelRc::default(), original.as_slice(), None);
        let observer = Box::pin(ModelChangeListenerContainer::new(Observer {
            model: model.clone(),
            changes: RefCell::new(Vec::new()),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());
        let handles = Box::pin(ModelChangeListenerContainer::new(HandleObserver {
            row: original[5_000].clone(),
            counts: RefCell::new(Vec::new()),
        }));
        model
            .model_tracker()
            .attach_peer(handles.as_ref().model_peer());

        // The retained feed snapshot is borrowed, including during reentrant
        // notifications. No temporary full Vec may retain every old handle.
        assert_eq!(
            update_snapshot_scoped(model.clone(), original.as_slice(), None),
            model
        );
        assert!(observer.as_ref().get().changes.borrow().is_empty());
        let mut appended = original.as_ref().clone();
        appended.push(Arc::new(fixture(10_000)));
        let appended = Arc::new(appended);
        let append_count = Arc::strong_count(&original[5_000]);
        assert_eq!(
            update_snapshot_scoped(model.clone(), appended.as_slice(), None),
            model
        );

        let mut edited = appended.as_ref().clone();
        Arc::make_mut(&mut edited[0]).body = "Edited borrowed row".into();
        let edited = Arc::new(edited);
        let edit_count = Arc::strong_count(&original[5_000]);
        assert_eq!(
            update_snapshot_scoped(model.clone(), edited.as_slice(), None),
            model
        );
        assert_eq!(
            model.row_data(0).unwrap().body.as_str(),
            "Edited borrowed row"
        );
        assert_ne!(original[0].body, "Edited borrowed row");

        let mut reset = edited.as_ref().clone();
        for row in &mut reset[..MAX_ROW_NOTIFICATIONS + 1] {
            Arc::make_mut(row).body = "Bulk borrowed replacement".into();
        }
        let reset = Arc::new(reset);
        let reset_count = Arc::strong_count(&original[5_000]);
        assert_eq!(
            update_snapshot_scoped(model.clone(), reset.as_slice(), None),
            model
        );
        assert_eq!(
            update_snapshot_scoped(model.clone(), reset.as_slice(), None),
            model
        );
        assert_eq!(
            *observer.as_ref().get().changes.borrow(),
            [
                Change::Added(10_000, 1, 10_001),
                Change::Changed(0, 10_001),
                Change::Reset(10_001),
            ]
        );
        assert_eq!(
            *handles.as_ref().get().counts.borrow(),
            [append_count, edit_count, reset_count],
            "notifications must not retain an eager clone of the whole feed"
        );
        assert_eq!(model.row_count(), reset.len());
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        assert!(
            stored
                .rows
                .borrow()
                .iter()
                .zip(reset.iter())
                .all(|(shown, source)| shown.as_ref() == source.as_ref())
        );
        assert!(has_exact_row(&model, 0, &reset[0]));
        assert!(has_exact_row(&model, 5_000, &original[5_000]));
    }

    #[test]
    fn sparse_edits_across_ten_thousand_shared_rows_notify_only_changed_rows() {
        let original: Vec<_> = (0..10_000).map(|i| Arc::new(fixture(i))).collect();
        let model = update_shared(ModelRc::default(), original.clone());
        let observer = Box::pin(ModelChangeListenerContainer::new(Observer {
            model: model.clone(),
            changes: RefCell::new(Vec::new()),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());

        let mut next = original.clone();
        let edited_rows = [37, 9_876];
        for &row in &edited_rows {
            Arc::make_mut(&mut next[row]).send_state = MessageSendState::Failed;
        }
        // Value-equal rebuilt payloads between the edits must also retain the
        // existing allocation rather than replace an unchanged delegate's data.
        next[5_000] = Arc::new(original[5_000].as_ref().clone());
        assert!(!Arc::ptr_eq(&next[5_000], &original[5_000]));

        assert_eq!(update_shared(model.clone(), next.clone()), model);
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        {
            let rows = stored.rows.borrow();
            assert_eq!(rows.len(), 10_000);
            for (index, row) in rows.iter().enumerate() {
                assert_eq!(row.as_ref(), next[index].as_ref());
                if edited_rows.contains(&index) {
                    assert!(Arc::ptr_eq(row, &next[index]));
                } else {
                    assert!(Arc::ptr_eq(row, &original[index]));
                }
            }
        }
        assert_eq!(
            *observer.as_ref().get().changes.borrow(),
            [Change::Changed(37, 10_000), Change::Changed(9_876, 10_000)]
        );
        assert_eq!(
            projection_count(&model),
            2,
            "only the two reentrant observer reads project data"
        );
    }

    #[test]
    fn range_notifications_are_consistent_during_reentrant_reads() {
        let mut rows: Vec<_> = (0..3).map(fixture).collect();
        let model = update(ModelRc::default(), rows.clone());
        let observer = Box::pin(ModelChangeListenerContainer::new(Observer {
            model: model.clone(),
            changes: RefCell::new(Vec::new()),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());
        rows.push(fixture(3));
        assert_eq!(update(model.clone(), rows.clone()), model);
        rows.splice(0..0, [fixture(4), fixture(5)]);
        update(model.clone(), rows.clone());
        rows[3].reaction_summary = "heart 2".into();
        update(model.clone(), rows.clone());
        rows.remove(2);
        update(model.clone(), rows.clone());
        rows.splice(1..3, [fixture(6), fixture(7), fixture(8)]);
        update(model.clone(), rows.clone());
        assert_rows(&model, &rows);
        assert_eq!(
            *observer.as_ref().get().changes.borrow(),
            [
                Change::Added(3, 1, 4),
                Change::Added(0, 2, 6),
                Change::Changed(3, 6),
                Change::Removed(2, 1, 5),
                Change::Removed(1, 2, 3),
                Change::Added(1, 3, 6),
            ]
        );
        rows.clear();
        update(model.clone(), rows.clone());
        assert_rows(&model, &rows);
        assert_eq!(
            observer.as_ref().get().changes.borrow().last(),
            Some(&Change::Removed(0, 6, 0))
        );
    }

    #[test]
    fn bulk_metadata_changes_use_one_reset_without_eager_projection() {
        let mut rows: Vec<_> = (0..MAX_ROW_NOTIFICATIONS + 1).map(fixture).collect();
        let model = update(ModelRc::default(), rows.clone());
        let observer = Box::pin(ModelChangeListenerContainer::new(Observer {
            model: model.clone(),
            changes: RefCell::new(Vec::new()),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());
        for row in &mut rows {
            row.send_state = MessageSendState::Failed;
        }
        update(model.clone(), rows.clone());
        assert_rows(&model, &rows);
        assert_eq!(
            *observer.as_ref().get().changes.borrow(),
            [Change::Reset(MAX_ROW_NOTIFICATIONS + 1)]
        );
        assert_eq!(
            projection_count(&model),
            1,
            "only the observer's own read projects data"
        );
    }

    #[test]
    fn every_message_field_participates_in_snapshot_equality() {
        let base = fixture(0);
        let mut variants = Vec::new();
        macro_rules! changed {
            ($field:ident, $value:expr) => {{
                let mut row = base.clone();
                row.$field = $value;
                variants.push(row);
            }};
        }
        changed!(key, "new-local-key".into());
        changed!(event_id, None);
        changed!(sender, "@other:example.org".into());
        changed!(body, "changed body".into());
        changed!(rich_markdown, Some("**changed rich**".into()));
        changed!(rich_accessible, Some("changed accessible".into()));
        changed!(attachment_name, Some("fixture.png".into()));
        changed!(attachment_is_image, true);
        changed!(reply_preview, Some("reply context".into()));
        changed!(reaction_summary, "heart 3".into());
        changed!(timestamp_ms, 1_000);
        changed!(is_own, true);
        changed!(is_local_echo, true);
        changed!(send_state, MessageSendState::Pending);
        for row in variants {
            let model = update(ModelRc::default(), vec![base.clone()]);
            let observer = Box::pin(ModelChangeListenerContainer::new(Observer {
                model: model.clone(),
                changes: RefCell::new(Vec::new()),
            }));
            model
                .model_tracker()
                .attach_peer(observer.as_ref().model_peer());
            update(model.clone(), vec![row.clone()]);
            assert_rows(&model, &[row]);
            assert_eq!(
                *observer.as_ref().get().changes.borrow(),
                [Change::Changed(0, 1)]
            );
        }
    }

    #[test]
    fn small_snapshots_cover_empty_duplicates_reordering_and_replacement() {
        let shapes: &[&[usize]] = &[
            &[],
            &[0],
            &[1],
            &[0, 0],
            &[0, 1],
            &[1, 0],
            &[0, 1, 2],
            &[2, 0, 1],
            &[0, 2, 1, 2],
        ];
        for before in shapes {
            for after in shapes {
                let model = update(
                    ModelRc::default(),
                    before.iter().map(|i| fixture(*i)).collect(),
                );
                let expected: Vec<_> = after.iter().map(|i| fixture(*i)).collect();
                assert_eq!(update(model.clone(), expected.clone()), model);
                assert_rows(&model, &expected);
            }
        }
    }

    #[test]
    fn room_thread_and_detached_models_do_not_share_rows() {
        let room = update(ModelRc::default(), vec![fixture(0)]);
        let thread = update(ModelRc::default(), vec![fixture(1)]);
        assert_ne!(room, thread);
        update(room.clone(), vec![fixture(2)]);
        assert_rows(&thread, &[fixture(1)]);
        let blank: ModelRc<MessageItem> = ModelRc::default();
        let returned = update(blank, vec![fixture(3)]);
        assert_ne!(returned, room);
        assert_rows(&returned, &[fixture(3)]);
        assert_rows(&room, &[fixture(2)]);
    }

    #[test]
    fn ten_thousand_event_lookups_reuse_index_for_noops_edits_and_appends() {
        let original: Vec<_> = (0..10_000).map(|i| Arc::new(fixture(i))).collect();
        let model = update_snapshot_scoped(ModelRc::default(), &original, None);
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        assert!(stored.event_index.borrow().is_none());
        assert_eq!(stored.indexed_rows.get(), 0, "unused index stays lazy");
        for _ in 0..3 {
            assert_eq!(
                index_of_event(&model, "$fixture-9999:example.org"),
                Some(9_999)
            );
            assert!(info_of_event(&model, "$fixture-5000:example.org").is_some());
            assert_eq!(index_of_event(&model, "$missing:example.org"), None);
        }
        assert_eq!(stored.indexed_rows.get(), 10_000, "one initial build");
        assert_eq!(
            update_snapshot_scoped(model.clone(), &original, None),
            model
        );
        let mut next = original.clone();
        let mut edited = next[5_000].as_ref().clone();
        edited.body = "Edited and redacted presentation".into();
        edited.rich_markdown = None;
        edited.rich_accessible = None;
        edited.reaction_summary = "Check 2".into();
        next[5_000] = Arc::new(edited);
        assert_eq!(update_snapshot_scoped(model.clone(), &next, None), model);
        let shown = info_of_event(&model, "$fixture-5000:example.org").unwrap();
        assert!(
            Arc::ptr_eq(&shown, &next[5_000]),
            "lookup reads the new row, not a cached payload"
        );
        assert_eq!(stored.indexed_rows.get(), 10_000);
        next.push(Arc::new(fixture(10_000)));
        next.push(Arc::new(fixture(42)));
        assert_eq!(update_snapshot_scoped(model.clone(), &next, None), model);
        assert_eq!(
            index_of_event(&model, "$fixture-10000:example.org"),
            Some(10_000)
        );
        assert_eq!(index_of_event(&model, "$fixture-42:example.org"), Some(42));
        assert_eq!(
            stored.indexed_rows.get(),
            10_002,
            "append indexes only its new rows"
        );
        assert_eq!(stored.event_index.borrow().as_ref().unwrap().len(), 10_001);
        assert_eq!(
            projection_count(&model),
            0,
            "event lookup never projects rich text"
        );

        let lazy = update_snapshot_scoped(ModelRc::default(), &original, None);
        update_snapshot_scoped(lazy.clone(), &next, None);
        let lazy = lazy.as_any().downcast_ref::<TimelineModel>().unwrap();
        assert!(lazy.event_index.borrow().is_none());
        assert_eq!(
            lazy.indexed_rows.get(),
            0,
            "edits/appends do not build an unused cache"
        );
    }

    struct IndexObserver {
        model: ModelRc<MessageItem>,
        queries: Vec<String>,
        notifications: std::cell::Cell<usize>,
    }

    impl IndexObserver {
        fn check(&self) {
            let stored = self.model.as_any().downcast_ref::<TimelineModel>().unwrap();
            for event in &self.queries {
                let expected = stored
                    .rows
                    .borrow()
                    .iter()
                    .position(|row| row.event_id.as_deref() == Some(event.as_str()));
                assert_eq!(index_of_event(&self.model, event), expected);
                let found = info_of_event(&self.model, event);
                match expected {
                    Some(position) => assert!(Arc::ptr_eq(
                        &found.unwrap(),
                        &stored.rows.borrow()[position]
                    )),
                    None => assert!(found.is_none()),
                }
            }
            assert!(stored.event_index.borrow().as_ref().unwrap().len() <= self.model.row_count());
            self.notifications.set(self.notifications.get() + 1);
        }
    }

    impl ModelChangeListener for IndexObserver {
        fn row_changed(self: Pin<&Self>, _: usize) {
            self.check();
        }
        fn row_added(self: Pin<&Self>, _: usize, _: usize) {
            self.check();
        }
        fn row_removed(self: Pin<&Self>, _: usize, _: usize) {
            self.check();
        }
        fn reset(self: Pin<&Self>) {
            self.check();
        }
    }

    #[test]
    fn event_index_matches_current_rows_during_edits_paging_moves_removal_and_reset() {
        let mut rows: Vec<_> = [0, 1, 1, 2]
            .into_iter()
            .map(|i| Arc::new(fixture(i)))
            .collect();
        Arc::make_mut(&mut rows[3]).event_id = None;
        let model = update_shared(ModelRc::default(), rows.clone());
        assert_eq!(index_of_event(&model, "$fixture-1:example.org"), Some(1));
        let observer = Box::pin(ModelChangeListenerContainer::new(IndexObserver {
            model: model.clone(),
            queries: (0..MAX_ROW_NOTIFICATIONS + 5)
                .map(|i| format!("$fixture-{i}:example.org"))
                .chain([
                    String::new(),
                    "$sent:example.org".into(),
                    "$missing:example.org".into(),
                ])
                .collect(),
            notifications: std::cell::Cell::new(0),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());
        // Same event ID, new value: the cached position must return the current Arc.
        Arc::make_mut(&mut rows[0]).reaction_summary = "Updated 2".into();
        update_shared(model.clone(), rows.clone());
        // Local echo acknowledgement and changing the first duplicate's ID.
        Arc::make_mut(&mut rows[3]).event_id = Some("$sent:example.org".into());
        Arc::make_mut(&mut rows[1]).event_id = Some(String::new());
        update_shared(model.clone(), rows.clone());
        assert_eq!(index_of_event(&model, "$fixture-1:example.org"), Some(2));
        assert_eq!(index_of_event(&model, ""), Some(1));
        // Pagination, reordering and removals invalidate shifted positions.
        rows.insert(0, Arc::new(fixture(4)));
        update_shared(model.clone(), rows.clone());
        rows.rotate_left(2);
        update_shared(model.clone(), rows.clone());
        rows.remove(1);
        update_shared(model.clone(), rows.clone());
        rows = (0..MAX_ROW_NOTIFICATIONS + 5)
            .map(|i| Arc::new(fixture(i)))
            .collect();
        update_shared(model.clone(), rows.clone());
        for row in &mut rows {
            Arc::make_mut(row).body.push_str(" changed");
        }
        update_shared(model.clone(), rows);
        update_shared(model.clone(), Vec::new());
        assert!(observer.as_ref().get().notifications.get() >= 8);
        assert_eq!(model.row_count(), 0);
        assert!(
            model
                .as_any()
                .downcast_ref::<TimelineModel>()
                .unwrap()
                .event_index
                .borrow()
                .as_ref()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn event_index_is_model_local_and_never_retains_old_row_payloads() {
        let first = Arc::new(fixture(7));
        let old_row = Arc::downgrade(&first);
        let model = update_shared(ModelRc::default(), vec![first]);
        assert_eq!(index_of_event(&model, "$fixture-7:example.org"), Some(0));
        let mut replacement = fixture(7);
        replacement.body = "Current account's edited row".into();
        update_shared(model.clone(), vec![Arc::new(replacement)]);
        assert!(
            old_row.upgrade().is_none(),
            "index keeps IDs/positions, never an old row Arc"
        );
        let mut other = fixture(7);
        other.body = "Other room/thread's own row".into();
        let other = update_shared(ModelRc::default(), vec![Arc::new(other)]);
        assert_ne!(model, other);
        assert_eq!(
            info_of_event(&model, "$fixture-7:example.org")
                .unwrap()
                .body,
            "Current account's edited row"
        );
        assert_eq!(
            info_of_event(&other, "$fixture-7:example.org")
                .unwrap()
                .body,
            "Other room/thread's own row"
        );
        let old_model = model
            .as_any()
            .downcast_ref::<TimelineModel>()
            .unwrap()
            .weak
            .clone();
        drop(model);
        assert!(
            old_model.upgrade().is_none(),
            "detaching releases the entire indexed model"
        );
        let returned = update_shared(ModelRc::default(), vec![Arc::new(fixture(9))]);
        assert_eq!(index_of_event(&returned, "$fixture-7:example.org"), None);
        assert_eq!(index_of_event(&returned, "$fixture-9:example.org"), Some(0));
        assert_eq!(index_of_event(&other, "$fixture-7:example.org"), Some(0));
    }
}

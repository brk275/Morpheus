//! UI-thread timeline storage with lazy rich-text projection and stable models.
//!
//! SDK snapshots share immutable rows. Comparing them here avoids
//! rebuilding unchanged Slint rows; only row_data formats a requested row.
//! Account, room and feed-generation guards remain at the publication callers.

use std::{any::Any, cell::RefCell, sync::Arc};

use morpheus_matrix::MessageInfo;
use slint::{Model, ModelNotify, ModelRc, ModelTracker};

use crate::MessageItem;

// Bound synchronous per-row notifications for bulk replacements. Inserts and
// removals each use one range notification regardless of the history length.
const MAX_ROW_NOTIFICATIONS: usize = 128;

struct TimelineModel {
    rows: RefCell<Vec<Arc<MessageInfo>>>,
    notify: ModelNotify,
    #[cfg(test)]
    projected: std::cell::Cell<usize>,
}

impl TimelineModel {
    fn replace(&self, next: Vec<Arc<MessageInfo>>) {
        let (prefix, removed, added) = {
            let old = self.rows.borrow();
            let prefix = old
                .iter()
                .zip(&next)
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
                *self.rows.borrow_mut() = next;
                self.notify.reset();
                return;
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
                drop(self.rows.borrow_mut().drain(prefix..prefix + removed));
                self.notify.row_removed(prefix, removed);
            }
            if added != 0 {
                drop(
                    self.rows
                        .borrow_mut()
                        .splice(prefix..prefix, next.into_iter().skip(prefix).take(added)),
                );
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
        #[cfg(test)]
        self.projected.set(self.projected.get() + 1);
        Some(crate::live::message_item(info))
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
pub(crate) fn update_shared(
    current: ModelRc<MessageItem>,
    rows: Vec<Arc<MessageInfo>>,
) -> ModelRc<MessageItem> {
    if let Some(model) = current.as_any().downcast_ref::<TimelineModel>() {
        model.replace(rows);
        current
    } else {
        ModelRc::new(TimelineModel {
            rows: RefCell::new(rows),
            notify: ModelNotify::default(),
            #[cfg(test)]
            projected: std::cell::Cell::new(0),
        })
    }
}

/// Position of the row showing `event_id`. A timeline model is read from its
/// stored rows without projecting any row.
pub(crate) fn index_of_event(model: &ModelRc<MessageItem>, event_id: &str) -> Option<usize> {
    if let Some(model) = model.as_any().downcast_ref::<TimelineModel>() {
        return model
            .rows
            .borrow()
            .iter()
            .position(|row| row.event_id.as_deref() == Some(event_id));
    }
    model
        .iter()
        .position(|row| row.event_id.as_str() == event_id)
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
    model
        .rows
        .borrow()
        .iter()
        .find(|row| row.event_id.as_deref() == Some(event_id))
        .cloned()
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
}

//! UI-thread timeline storage with lazy rich-text projection and stable models.
//!
//! SDK snapshots share immutable rows. Comparing them here avoids
//! rebuilding unchanged Slint rows; only row_data formats a requested row.
//! Account, room and feed-generation guards remain at the publication callers.

use std::{
    any::Any,
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::{Rc, Weak},
    sync::Arc,
};

use morpheus_matrix::{MessageInfo, MessageSnapshot};
use slint::{Model, ModelNotify, ModelRc, ModelTracker};

use crate::MessageItem;

// Bound synchronous per-row notifications for bulk replacements. Inserts and
// removals each use one range notification regardless of the history length.
const MAX_ROW_NOTIFICATIONS: usize = 128;

// These are projection-cache limits, never timeline/history or render limits.
const MAX_CACHED_TEXT_ROWS: usize = 128;
const MAX_CACHED_TEXT_CHARGE: usize = 4 * 1024 * 1024;
const MAX_CACHED_MARKDOWN_BYTES: usize = 4096;
const MAX_CACHED_MARKDOWN_MARKERS: usize = 128;

/// Conservative owned-text/span admission charge, not measured RSS. Complex
/// rich text (including links/reference expansion) and oversized rows still
/// render fully; only their optional parsed-text cache entry is refused.
fn text_cache_charge(info: &MessageInfo) -> Option<usize> {
    let bytes = info
        .body
        .len()
        .checked_add(info.rich_markdown.as_ref().map_or(0, String::len))?
        .checked_add(info.rich_accessible.as_ref().map_or(0, String::len))?;
    let mut charge = bytes.checked_mul(16)?.checked_add(2048)?;
    if let Some(markdown) = info.rich_markdown.as_ref() {
        if markdown.len() > MAX_CACHED_MARKDOWN_BYTES
            || markdown.contains('[')
            || markdown.contains('<')
        {
            return None;
        }
        let markers = markdown
            .bytes()
            .filter(|byte| byte.is_ascii_punctuation() || matches!(*byte, b'\n' | b'\r'))
            .count();
        if markers > MAX_CACHED_MARKDOWN_MARKERS {
            return None;
        }
        // Slint's pinned parser owns paragraph strings and flat span vectors.
        // Charge both possible paragraph/style cross-products conservatively;
        // no URLs/interpolated paragraphs are admitted through this path.
        let span_charge = markers.checked_add(1)?.checked_pow(2)?.checked_mul(128)?;
        charge = charge.checked_add(span_charge)?;
    }
    (charge <= MAX_CACHED_TEXT_CHARGE).then_some(charge)
}

struct CachedText {
    source: std::sync::Weak<MessageInfo>,
    text: Rc<crate::live::MessageText>,
    charge: usize,
}

struct TextCache {
    entries: HashMap<usize, CachedText>,
    order: VecDeque<usize>,
    charge: usize,
    epoch: u64,
    enabled: bool,
    publication_required: bool,
}

impl TextCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            charge: 0,
            epoch: 0,
            enabled: true,
            publication_required: false,
        }
    }

    fn invalidate(&mut self, enabled: bool) {
        self.entries.clear();
        self.order.clear();
        self.charge = 0;
        self.epoch = self.epoch.checked_add(1).unwrap_or(u64::MAX);
        self.enabled = enabled && self.epoch != u64::MAX;
    }

    fn suspend(&mut self, publication_required: bool) {
        self.invalidate(false);
        self.publication_required |= publication_required;
    }

    fn enable_after_publication(&mut self, epoch: u64) {
        if self.epoch == epoch && self.epoch != u64::MAX {
            self.publication_required = false;
            self.enabled = true;
        }
    }

    fn resume_after_context(&mut self, epoch: u64) {
        if self.epoch == epoch && self.epoch != u64::MAX && !self.publication_required {
            self.enabled = true;
        }
    }

    fn remove(&mut self, row: usize) {
        if let Some(entry) = self.entries.remove(&row) {
            self.charge -= entry.charge;
        }
        if let Some(position) = self.order.iter().position(|index| *index == row) {
            self.order.remove(position);
        }
    }

    fn get(&mut self, row: usize, info: &Arc<MessageInfo>) -> Option<Rc<crate::live::MessageText>> {
        if !self.enabled {
            return None;
        }
        let text = self
            .entries
            .get(&row)
            .filter(|entry| {
                entry
                    .source
                    .upgrade()
                    .is_some_and(|source| Arc::ptr_eq(&source, info))
            })
            .map(|entry| entry.text.clone());
        if text.is_some() {
            if let Some(position) = self.order.iter().position(|index| *index == row) {
                self.order.remove(position);
            }
            self.order.push_back(row);
        } else {
            self.remove(row);
        }
        text
    }

    fn insert(
        &mut self,
        row: usize,
        info: &Arc<MessageInfo>,
        epoch: u64,
        text: crate::live::MessageText,
        charge: usize,
    ) {
        if !self.enabled || self.epoch != epoch || charge > MAX_CACHED_TEXT_CHARGE {
            return;
        }
        self.remove(row);
        while self.entries.len() >= MAX_CACHED_TEXT_ROWS
            || self.charge.saturating_add(charge) > MAX_CACHED_TEXT_CHARGE
        {
            let Some(oldest) = self.order.pop_front() else {
                self.invalidate(false);
                return;
            };
            if let Some(entry) = self.entries.remove(&oldest) {
                self.charge -= entry.charge;
            }
        }
        self.order.push_back(row);
        self.charge += charge;
        self.entries.insert(
            row,
            CachedText {
                source: Arc::downgrade(info),
                text: Rc::new(text),
                charge,
            },
        );
    }
}

struct TimelineModel {
    weak: Weak<Self>,
    copy_scope: RefCell<Option<Rc<crate::message_copy::CopyScope>>>,
    copy_projected: RefCell<VecDeque<usize>>,
    rows: RefCell<Vec<Arc<MessageInfo>>>,
    // Weak identity only: never keep an old feed snapshot or plaintext alive.
    // Every replacement clears it before changing rows; notification reentry
    // cannot install the outer publication's identity after a newer write.
    snapshot: RefCell<Option<std::sync::Weak<Vec<Arc<MessageInfo>>>>>,
    replacement_serial: Cell<u64>,
    #[cfg(test)]
    snapshot_scans: Cell<usize>,
    text_cache: RefCell<TextCache>,
    // Built only by event lookup, owned by this exact model, and never larger
    // than the loaded row set. Duplicate event IDs keep their first position.
    event_index: RefCell<Option<HashMap<String, usize>>>,
    notify: ModelNotify,
    #[cfg(test)]
    indexed_rows: std::cell::Cell<usize>,
    #[cfg(test)]
    projected: std::cell::Cell<usize>,
    #[cfg(test)]
    parsed_text: std::cell::Cell<usize>,
}

impl TimelineModel {
    fn project_item(
        &self,
        row: usize,
        info: &Arc<MessageInfo>,
        mut allowed: impl FnMut() -> bool,
    ) -> MessageItem {
        let permitted = allowed();
        let (epoch, cached) = {
            let mut cache = self.text_cache.borrow_mut();
            if !permitted && cache.enabled {
                cache.invalidate(false);
            }
            (cache.epoch, cache.get(row, info))
        };
        if let Some(text) = cached {
            crate::live::message_item_with_text(info, &text)
        } else {
            #[cfg(test)]
            self.parsed_text.set(self.parsed_text.get() + 1);
            let text = crate::live::message_text(info);
            let item = crate::live::message_item_with_text(info, &text);
            let permitted = allowed();
            let current = self
                .rows
                .borrow()
                .get(row)
                .is_some_and(|source| Arc::ptr_eq(source, info));
            if current && permitted {
                if let Some(charge) = text_cache_charge(info) {
                    self.text_cache
                        .borrow_mut()
                        .insert(row, info, epoch, text, charge);
                }
            }
            item
        }
    }

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
    fn remember_snapshot(&self, snapshot: Option<&MessageSnapshot>, serial: u64) {
        if serial != u64::MAX && self.replacement_serial.get() == serial {
            *self.snapshot.borrow_mut() = snapshot.map(Arc::downgrade);
        }
    }

    fn replace(
        &self,
        next: Cow<'_, [Arc<MessageInfo>]>,
        snapshot: Option<&MessageSnapshot>,
    ) -> u64 {
        let serial = self.replacement_serial.get().saturating_add(1);
        self.replacement_serial.set(serial);
        let unchanged = serial != u64::MAX
            && snapshot.is_some_and(|next| {
                self.snapshot
                    .borrow()
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .is_some_and(|previous| Arc::ptr_eq(&previous, next))
            });
        if unchanged {
            return self.text_cache.borrow().epoch;
        }
        self.snapshot.borrow_mut().take();
        #[cfg(test)]
        self.snapshot_scans.set(self.snapshot_scans.get() + 1);
        let (prefix, removed, added) = {
            let old = self.rows.borrow();
            let prefix = old
                .iter()
                .zip(next.iter())
                .take_while(|(a, b)| same_row(a, b))
                .count();
            if prefix == old.len() && prefix == next.len() {
                self.remember_snapshot(snapshot, serial);
                return self.text_cache.borrow().epoch;
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

        // Drop every old parsed plaintext object BEFORE any edit, splice or
        // reset can notify/reenter the model. Only a no-op keeps the cache.
        let publication_epoch = {
            let mut cache = self.text_cache.borrow_mut();
            let enabled = cache.enabled;
            cache.invalidate(enabled);
            cache.epoch
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
                self.snapshot.borrow_mut().take();
                *self.rows.borrow_mut() = next.into_owned();
                self.notify.reset();
                self.remember_snapshot(snapshot, serial);
                return publication_epoch;
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
                self.snapshot.borrow_mut().take();
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
                self.snapshot.borrow_mut().take();
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
                self.snapshot.borrow_mut().take();
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
        self.remember_snapshot(snapshot, serial);
        publication_epoch
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
        // Hold neither the source-model nor the cache borrow while formatting
        // or dynamic projection runs. A reentrant edit invalidates the epoch.
        let info = self.rows.borrow().get(row)?.clone();
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
        let mut item = self.project_item(row, &info, || {
            let scope = self.copy_scope.borrow().clone();
            crate::message_copy::text_projection_cache_allowed(
                scope.as_ref(),
                self.weak.clone(),
                row,
                &info,
            )
        });
        item.copy_token = crate::message_copy::row_token(
            self.weak.clone(),
            self.copy_scope.borrow().as_ref(),
            row,
            &info,
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
    update_rows_scoped(current, Cow::Owned(rows), scope, None)
}

/// Borrow the coalesced feed's immutable snapshot on the UI thread. Unchanged
/// rows already belong to the model; only inserted or edited rows need new
/// handles, unless a new model or a bulk reset replaces the whole history.
#[cfg(test)]
pub(crate) fn update_snapshot_scoped(
    current: ModelRc<MessageItem>,
    rows: &[Arc<MessageInfo>],
    scope: Option<Rc<crate::message_copy::CopyScope>>,
) -> ModelRc<MessageItem> {
    update_rows_scoped(current, Cow::Borrowed(rows), scope, None)
}

/// Preserve the coalesced feed's immutable allocation identity. A repeated
/// snapshot skips only row comparison; scope renewal and dynamic projection
/// still run normally. Arc::make_mut dissociates remembered weak pointers.
pub(crate) fn update_feed_snapshot_scoped(
    current: ModelRc<MessageItem>,
    rows: &MessageSnapshot,
    scope: Option<Rc<crate::message_copy::CopyScope>>,
) -> ModelRc<MessageItem> {
    update_rows_scoped(current, Cow::Borrowed(rows.as_slice()), scope, Some(rows))
}

fn update_rows_scoped(
    current: ModelRc<MessageItem>,
    rows: Cow<'_, [Arc<MessageInfo>]>,
    scope: Option<Rc<crate::message_copy::CopyScope>>,
    snapshot: Option<&MessageSnapshot>,
) -> ModelRc<MessageItem> {
    if let Some(model) = current.as_any().downcast_ref::<TimelineModel>() {
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
            model.text_cache.borrow_mut().invalidate(false);
        }
        // Source replacement still precedes a reset that can synchronously
        // project; a changed scope cannot cache under the old authority.
        let publication_epoch = model.replace(rows, snapshot);
        if !same {
            *model.copy_scope.borrow_mut() = next;
            model.notify_copy_rows();
        }
        model
            .text_cache
            .borrow_mut()
            .enable_after_publication(publication_epoch);
        current
    } else {
        let model = Rc::new_cyclic(|weak| TimelineModel {
            weak: weak.clone(),
            copy_scope: RefCell::new(scope),
            copy_projected: RefCell::new(VecDeque::new()),
            rows: RefCell::new(rows.into_owned()),
            snapshot: RefCell::new(snapshot.map(Arc::downgrade)),
            replacement_serial: Cell::new(0),
            #[cfg(test)]
            snapshot_scans: Cell::new(0),
            text_cache: RefCell::new(TextCache::new()),
            event_index: RefCell::new(None),
            notify: ModelNotify::default(),
            #[cfg(test)]
            indexed_rows: std::cell::Cell::new(0),
            #[cfg(test)]
            projected: std::cell::Cell::new(0),
            #[cfg(test)]
            parsed_text: std::cell::Cell::new(0),
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
    let old_scope = model.copy_scope.borrow().clone();
    let next = crate::message_copy::context_scope(old_scope.as_ref());
    let same = match (model.copy_scope.borrow().as_ref(), next.as_ref()) {
        (Some(a), Some(b)) => Rc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    };
    if !same {
        model.text_cache.borrow_mut().invalidate(false);
    }
    let epoch = model.text_cache.borrow().epoch;
    if !same {
        *model.copy_scope.borrow_mut() = next;
        model.notify_copy_rows();
    }
    let scope = model.copy_scope.borrow().clone();
    // No RefCell guard survives this UI/capability check. A cache whose room
    // retired still requires a real publication, not a context notification.
    let first = model.rows.borrow().first().cloned();
    if let Some(first) = first {
        if crate::message_copy::text_projection_cache_allowed(
            scope.as_ref(),
            model.weak.clone(),
            0,
            &first,
        ) {
            model.text_cache.borrow_mut().resume_after_context(epoch);
        }
    }
}

/// Synchronous account/room/protection withdrawal clears parsed plaintext even
/// if another internal callback still holds this old model. No notification or
/// formatting can refill it until a fresh guarded publication enables it.
pub(crate) fn clear_text_projection_cache(current: &ModelRc<MessageItem>) {
    if let Some(model) = current.as_any().downcast_ref::<TimelineModel>() {
        model.text_cache.borrow_mut().suspend(true);
    }
}

/// A protected overlay suspends caching until the SAME original displayed
/// scope becomes current again; it never binds an old model to a new target.
pub(crate) fn suspend_text_projection_cache(current: &ModelRc<MessageItem>) {
    if let Some(model) = current.as_any().downcast_ref::<TimelineModel>() {
        model.text_cache.borrow_mut().suspend(false);
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
pub(crate) fn cached_text_count(model: &ModelRc<MessageItem>) -> usize {
    model
        .as_any()
        .downcast_ref::<TimelineModel>()
        .map_or(0, |model| model.text_cache.borrow().entries.len())
}

#[cfg(test)]
pub(crate) fn parsed_text_count(model: &ModelRc<MessageItem>) -> usize {
    model
        .as_any()
        .downcast_ref::<TimelineModel>()
        .map_or(0, |model| model.parsed_text.get())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use morpheus_matrix::MessageSendState;
    use slint::private_unstable_api::re_exports::{
        ModelChangeListener, ModelChangeListenerContainer,
    };
    use std::pin::Pin;

    #[test]
    fn text_projection_shares_plain_strings_and_reuses_parse_without_freezing_pins() {
        let mut plain = fixture(1);
        plain.rich_markdown = None;
        plain.rich_accessible = None;
        let text = crate::live::message_text(&plain);
        assert_eq!(
            text.body.as_str().as_ptr(),
            text.rich_accessible_body.as_str().as_ptr()
        );
        assert_eq!(
            text.body.as_str().as_ptr(),
            text.edit_source.as_str().as_ptr()
        );
        let row = Arc::new(plain);
        let model = update_shared(ModelRc::default(), vec![row.clone()]);
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        crate::pinned_messages::seed_row_pins_for_test(&[]);
        let before = stored.project_item(0, &row, || true);
        assert!(!before.pinned);
        crate::pinned_messages::seed_row_pins_for_test(&[row.event_id.as_deref().unwrap()]);
        let after = stored.project_item(0, &row, || true);
        assert!(after.pinned);
        assert_eq!(before.rich_body, after.rich_body);
        assert_eq!(before.rich_accessible_body, after.rich_accessible_body);
        assert_eq!(stored.parsed_text.get(), 1);
        assert_eq!(after, crate::live::message_item(&row));
        crate::pinned_messages::seed_row_pins_for_test(&[]);
    }

    #[test]
    fn text_cache_never_matches_event_ids_or_raw_addresses_and_keeps_no_source_alive() {
        let mut cache = TextCache::new();
        let row = Arc::new(fixture(7));
        let weak = Arc::downgrade(&row);
        let epoch = cache.epoch;
        cache.insert(
            0,
            &row,
            epoch,
            crate::live::message_text(&row),
            text_cache_charge(&row).unwrap(),
        );
        let cached = cache.get(0, &row).unwrap();
        let parsed = Rc::downgrade(&cached);
        drop(cached);
        drop(row);
        assert!(weak.upgrade().is_none());
        let mut other = fixture(7);
        other.body = "Different account or edit with the same event ID".into();
        let other = Arc::new(other);
        assert!(cache.get(0, &other).is_none());
        assert!(parsed.upgrade().is_none());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.charge, 0);
    }

    #[test]
    fn text_cache_bounds_entry_and_payload_charge_without_limiting_history() {
        let rows: Vec<_> = (0..10_000).map(|index| Arc::new(fixture(index))).collect();
        let model = update_shared(ModelRc::default(), rows.clone());
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        for (index, row) in rows.iter().take(MAX_CACHED_TEXT_ROWS + 2).enumerate() {
            stored.project_item(index, row, || true);
        }
        assert_eq!(model.row_count(), 10_000);
        assert_eq!(
            stored.text_cache.borrow().entries.len(),
            MAX_CACHED_TEXT_ROWS
        );
        assert!(stored.text_cache.borrow_mut().get(0, &rows[0]).is_none());
        assert!(stored.text_cache.borrow().charge <= MAX_CACHED_TEXT_CHARGE);

        let mut cache = TextCache::new();
        let mut big = fixture(0);
        big.body = "x".repeat(8192);
        big.rich_markdown = None;
        big.rich_accessible = None;
        let big = Arc::new(big);
        let charge = text_cache_charge(&big).unwrap();
        let total = MAX_CACHED_TEXT_CHARGE / charge + 1;
        assert!(total < MAX_CACHED_TEXT_ROWS);
        for row in 0..total {
            cache.insert(
                row,
                &big,
                cache.epoch,
                crate::live::message_text(&big),
                charge,
            );
        }
        assert_eq!(cache.entries.len(), total - 1);
        assert_eq!(cache.charge, (total - 1) * charge);
        assert!(cache.get(0, &big).is_none());
        assert!(cache.charge <= MAX_CACHED_TEXT_CHARGE);
    }

    #[test]
    fn oversized_and_linked_rich_rows_render_fully_without_optional_cache_retention() {
        let mut oversized = fixture(3);
        oversized.body = "x".repeat(MAX_CACHED_TEXT_CHARGE / 16 + 1);
        oversized.rich_markdown = None;
        oversized.rich_accessible = None;
        let mut linked = fixture(4);
        linked.body = "Unrelated fallback".into();
        linked.rich_markdown = Some("[Full visible link](https://example.org/path)".into());
        linked.rich_accessible = Some("Full visible link".into());
        for row in [oversized, linked] {
            assert!(text_cache_charge(&row).is_none());
            let row = Arc::new(row);
            let model = update_shared(ModelRc::default(), vec![row.clone()]);
            let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
            let first = stored.project_item(0, &row, || true);
            let second = stored.project_item(0, &row, || true);
            assert_eq!(first, crate::live::message_item(&row));
            assert_eq!(second, first);
            assert_eq!(second.body.as_str(), row.body);
            assert_eq!(stored.parsed_text.get(), 2);
            assert!(stored.text_cache.borrow().entries.is_empty());
            assert_eq!(stored.text_cache.borrow().charge, 0);
        }
    }

    #[test]
    fn text_cache_withdrawal_drops_payloads_and_rejects_stale_insertion_until_publication() {
        let row = Arc::new(fixture(9));
        let model = update_shared(ModelRc::default(), vec![row.clone()]);
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        stored.project_item(0, &row, || true);
        let cached = stored.text_cache.borrow_mut().get(0, &row).unwrap();
        let weak = Rc::downgrade(&cached);
        drop(cached);
        let previous = stored.text_cache.borrow().epoch;
        clear_text_projection_cache(&model);
        assert!(weak.upgrade().is_none());
        assert!(!stored.text_cache.borrow().enabled);
        stored.text_cache.borrow_mut().insert(
            0,
            &row,
            previous,
            crate::live::message_text(&row),
            text_cache_charge(&row).unwrap(),
        );
        assert!(stored.text_cache.borrow().entries.is_empty());
        stored.project_item(0, &row, || true);
        assert!(stored.text_cache.borrow().entries.is_empty());
        assert_eq!(model.row_count(), 1);
        update_shared(model.clone(), vec![row.clone()]);
        assert!(stored.text_cache.borrow().enabled);
        stored.text_cache.borrow_mut().insert(
            0,
            &row,
            previous,
            crate::live::message_text(&row),
            text_cache_charge(&row).unwrap(),
        );
        assert!(stored.text_cache.borrow().entries.is_empty());
        stored.project_item(0, &row, || true);
        assert_eq!(stored.text_cache.borrow().entries.len(), 1);
        assert_eq!(stored.parsed_text.get(), 3);
    }

    #[test]
    fn edited_rows_clear_cached_plaintext_before_reentrant_model_notifications() {
        struct Cleared {
            model: ModelRc<MessageItem>,
            previous: Weak<crate::live::MessageText>,
            notifications: std::cell::Cell<usize>,
        }
        impl ModelChangeListener for Cleared {
            fn row_changed(self: Pin<&Self>, _row: usize) {
                self.reset();
            }
            fn row_added(self: Pin<&Self>, _row: usize, _count: usize) {
                self.reset();
            }
            fn row_removed(self: Pin<&Self>, _row: usize, _count: usize) {
                self.reset();
            }
            fn reset(self: Pin<&Self>) {
                assert!(self.previous.upgrade().is_none());
                let stored = self.model.as_any().downcast_ref::<TimelineModel>().unwrap();
                assert!(stored.text_cache.borrow().entries.is_empty());
                // A synchronous listener can retire this very model during
                // publication. Its later enable must not undo retirement.
                clear_text_projection_cache(&self.model);
                let _ = self.model.row_data(0);
                self.notifications.set(self.notifications.get() + 1);
            }
        }
        let row = Arc::new(fixture(11));
        let model = update_shared(ModelRc::default(), vec![row.clone()]);
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        stored.project_item(0, &row, || true);
        let cached = stored.text_cache.borrow_mut().get(0, &row).unwrap();
        let previous = Rc::downgrade(&cached);
        drop(cached);
        let observer = Box::pin(ModelChangeListenerContainer::new(Cleared {
            model: model.clone(),
            previous,
            notifications: std::cell::Cell::new(0),
        }));
        model
            .model_tracker()
            .attach_peer(observer.as_ref().model_peer());
        let mut edited = fixture(11);
        edited.body = "Edited and redacted text".into();
        edited.rich_markdown = None;
        edited.rich_accessible = None;
        let edited = Arc::new(edited);
        update_shared(model.clone(), vec![edited.clone()]);
        assert_eq!(observer.as_ref().get().notifications.get(), 1);
        assert!(!stored.text_cache.borrow().enabled);
        let shown = stored.project_item(0, &edited, || true);
        assert_eq!(shown.body.as_str(), "Edited and redacted text");
        assert!(stored.text_cache.borrow().entries.is_empty());
        // A subsequent actual publication may renew the exact retained row.
        update_shared(model.clone(), vec![edited.clone()]);
        stored.project_item(0, &edited, || true);
        assert_eq!(stored.text_cache.borrow().entries.len(), 1);
        assert_eq!(model.row_count(), 1);
    }

    #[test]
    fn text_cache_context_resume_requires_the_exact_unretired_epoch() {
        let mut cache = TextCache::new();
        cache.suspend(false);
        let modal_epoch = cache.epoch;
        cache.resume_after_context(modal_epoch);
        assert!(cache.enabled);
        cache.suspend(true);
        let retired_epoch = cache.epoch;
        cache.resume_after_context(retired_epoch);
        cache.enable_after_publication(modal_epoch);
        assert!(!cache.enabled);
        assert!(cache.publication_required);
        cache.enable_after_publication(retired_epoch);
        assert!(cache.enabled);
        assert!(!cache.publication_required);
    }

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
    fn repeated_feed_identity_skips_scans_without_retaining_snapshots_or_stale_local_edits() {
        let mut snapshot = Arc::new(vec![Arc::new(fixture(1)), Arc::new(fixture(2))]);
        let previous = Arc::downgrade(&snapshot);
        let model = update_feed_snapshot_scoped(ModelRc::default(), &snapshot, None);
        let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
        assert_eq!(Arc::strong_count(&snapshot), 1);
        assert_eq!(stored.snapshot_scans.get(), 0);
        update_feed_snapshot_scoped(model.clone(), &snapshot, None);
        assert_eq!(stored.snapshot_scans.get(), 0);
        assert_eq!(Arc::strong_count(&snapshot), 1);

        // A local/slice replacement invalidates the proof even when the next
        // feed repeats an older allocation. Never skip restoring its rows.
        update_shared(model.clone(), vec![Arc::new(fixture(3))]);
        assert!(stored.snapshot.borrow().is_none());
        let scans = stored.snapshot_scans.get();
        update_feed_snapshot_scoped(model.clone(), &snapshot, None);
        assert_eq!(stored.snapshot_scans.get(), scans + 1);
        assert!(has_exact_row(&model, 0, &snapshot[0]));
        assert_eq!(model.row_count(), 2);

        // With only a weak observer, make_mut dissociates the old identity;
        // modifying the feed cannot turn a changed snapshot into a fast hit.
        Arc::make_mut(&mut snapshot)[0] = Arc::new(fixture(4));
        assert!(previous.upgrade().is_none());
        let scans = stored.snapshot_scans.get();
        update_feed_snapshot_scoped(model.clone(), &snapshot, None);
        assert_eq!(stored.snapshot_scans.get(), scans + 1);
        assert!(has_exact_row(&model, 0, &snapshot[0]));
        let retired = Arc::downgrade(&snapshot);
        drop(snapshot);
        assert!(retired.upgrade().is_none());

        // Saturation permanently refuses identity shortcuts instead of
        // wrapping into an old publication's serial.
        let snapshot = Arc::new(vec![Arc::new(fixture(5))]);
        stored.replacement_serial.set(u64::MAX - 1);
        update_feed_snapshot_scoped(model.clone(), &snapshot, None);
        let scans = stored.snapshot_scans.get();
        update_feed_snapshot_scoped(model.clone(), &snapshot, None);
        assert_eq!(stored.snapshot_scans.get(), scans + 1);
        assert!(stored.snapshot.borrow().is_none());
    }

    #[test]
    fn reentrant_feed_replacement_cannot_restore_an_outer_snapshot_identity() {
        struct Replaced {
            model: ModelRc<MessageItem>,
            replacement: MessageSnapshot,
            fired: Cell<bool>,
        }
        impl ModelChangeListener for Replaced {
            fn row_changed(self: Pin<&Self>, _row: usize) {
                self.reset();
            }
            fn row_added(self: Pin<&Self>, _row: usize, _count: usize) {
                self.reset();
            }
            fn row_removed(self: Pin<&Self>, _row: usize, _count: usize) {
                self.reset();
            }
            fn reset(self: Pin<&Self>) {
                if !self.fired.replace(true) {
                    update_feed_snapshot_scoped(self.model.clone(), &self.replacement, None);
                }
            }
        }
        for same_length in [true, false] {
            let initial = Arc::new(vec![Arc::new(fixture(1)), Arc::new(fixture(2))]);
            let outer = Arc::new(if same_length {
                vec![Arc::new(fixture(3)), Arc::new(fixture(4))]
            } else {
                vec![
                    Arc::new(fixture(3)),
                    Arc::new(fixture(4)),
                    Arc::new(fixture(5)),
                ]
            });
            let replacement = Arc::new(vec![Arc::new(fixture(8)), Arc::new(fixture(9))]);
            let model = update_feed_snapshot_scoped(ModelRc::default(), &initial, None);
            let observer = Box::pin(ModelChangeListenerContainer::new(Replaced {
                model: model.clone(),
                replacement: replacement.clone(),
                fired: Cell::new(false),
            }));
            model
                .model_tracker()
                .attach_peer(observer.as_ref().model_peer());
            update_feed_snapshot_scoped(model.clone(), &outer, None);
            assert!(observer.as_ref().get().fired.get());
            let stored = model.as_any().downcast_ref::<TimelineModel>().unwrap();
            let remembered = stored
                .snapshot
                .borrow()
                .as_ref()
                .and_then(std::sync::Weak::upgrade);
            if same_length {
                assert!(Arc::ptr_eq(remembered.as_ref().unwrap(), &replacement));
            } else {
                // The outer insertion resumed after the inner publication.
                // Its further writes must discard the inner proof as well.
                assert!(remembered.is_none());
            }
            let scans = stored.snapshot_scans.get();
            update_feed_snapshot_scoped(model.clone(), &outer, None);
            assert_eq!(stored.snapshot_scans.get(), scans + 1);
            assert_eq!(model.row_count(), outer.len());
            for (index, row) in outer.iter().enumerate() {
                assert!(has_exact_row(&model, index, row));
            }
        }
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

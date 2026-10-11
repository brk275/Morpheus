//! Native composer binding. Only owned immutable snapshots cross Tokio/UI
//! boundaries; account revocation and scope/revision checks precede publication.

use super::{Candidate, Insertion, Scope, Session, Target, Ticket, emoji};
use crate::{AppWindow, EmojiItem, MentionItem, live::LiveUi};
use morpheus_matrix::{
    AccountSelection, ComposeError, ComposeSendTarget, ComposeText, EditBaseline, TextFormat,
};
use slint::{ComponentHandle, ModelRc, VecModel, platform::Key};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;

#[derive(Default)]
struct BoundSession {
    session: Session,
    selection: Option<AccountSelection>,
    baseline: Option<EditBaseline>,
    latest: Option<Ticket>,
    running: bool,
}

struct ParkedDraft {
    scope: Scope,
    text: ComposeText,
    selection: AccountSelection,
    draft_revision: Option<u64>,
}

struct EmojiPicker {
    threaded: bool,
    ticket: Insertion,
    selection: AccountSelection,
    rows: Vec<&'static emoji::Emoji>,
    highlighted: usize,
}

#[derive(Default)]
struct State {
    main: BoundSession,
    thread: BoundSession,
    parked: Option<ParkedDraft>,
    sends: super::send::SendLedger,
    emoji: Option<EmojiPicker>,
}
impl State {
    fn bound(&self, threaded: bool) -> &BoundSession {
        if threaded { &self.thread } else { &self.main }
    }
    fn bound_mut(&mut self, threaded: bool) -> &mut BoundSession {
        if threaded {
            &mut self.thread
        } else {
            &mut self.main
        }
    }
}

#[derive(Clone, Default)]
pub struct ComposerUi {
    state: Arc<Mutex<State>>,
}

#[derive(Clone)]
pub struct Submission {
    pub selection: AccountSelection,
    pub scope: Scope,
    pub text: ComposeText,
    pub baseline: Option<EditBaseline>,
    revision: u64,
}
impl Submission {
    pub fn target(&self) -> ComposeSendTarget {
        match &self.scope.target {
            Target::Room => ComposeSendTarget::Room,
            Target::Reply(id) => ComposeSendTarget::Reply(id.clone()),
            Target::Edit(id) => ComposeSendTarget::Edit(id.clone()),
            Target::Thread(id) => ComposeSendTarget::Thread(id.clone()),
        }
    }
    pub fn threaded(&self) -> bool {
        matches!(self.scope.target, Target::Thread(_))
    }
}

impl ComposerUi {
    pub fn install(window: &AppWindow, live: Arc<LiveUi>, runtime: Handle) -> Self {
        let this = Self::default();
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_open_emoji(move |threaded| {
            if let Some(window) = weak.upgrade() {
                owner.open_emoji(&window, &binding, threaded);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        window.on_emoji_query(move |query| {
            if let Some(window) = weak.upgrade() {
                {
                    let mut state = owner.state.lock().expect("composer mutex poisoned");
                    if let Some(picker) = state.emoji.as_mut() {
                        picker.rows = emoji::search(query.as_str());
                        picker.highlighted = 0;
                    }
                }
                owner.publish_emoji(&window);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        window.on_emoji_navigate(move |offset| {
            if let Some(window) = weak.upgrade() {
                {
                    let mut state = owner.state.lock().expect("composer mutex poisoned");
                    if let Some(picker) = state
                        .emoji
                        .as_mut()
                        .filter(|picker| !picker.rows.is_empty())
                    {
                        picker.highlighted = (picker.highlighted as i32 + offset.signum())
                            .rem_euclid(picker.rows.len() as i32)
                            as usize;
                    }
                }
                owner.publish_emoji(&window);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_emoji_choose(move |key| {
            if let Some(window) = weak.upgrade() {
                owner.choose_emoji(&window, &binding, key.as_str());
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        window.on_emoji_dismiss(move || {
            if let Some(window) = weak.upgrade() {
                owner.close_emoji(&window, true);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_composer_edited(move |body| {
            if let Some(window) = weak.upgrade() {
                owner.edited(&window, &binding, false, body.to_string());
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_thread_composer_edited(move |body| {
            if let Some(window) = weak.upgrade() {
                owner.edited(&window, &binding, true, body.to_string());
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_composer_formatting_changed(move |markdown| {
            if let Some(window) = weak.upgrade() {
                owner.format(&window, &binding, false, markdown);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_thread_formatting_changed(move |markdown| {
            if let Some(window) = weak.upgrade() {
                owner.format(&window, &binding, true, markdown);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        let handle = runtime.clone();
        window.on_mention_query(move |threaded, caret, anchor, preedit| {
            if let Some(window) = weak.upgrade() {
                owner.query(&window, &binding, &handle, threaded, caret, anchor, preedit);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_mention_choose(move |threaded, key| {
            if let Some(window) = weak.upgrade() {
                owner.choose(&window, &binding, threaded, key.as_str());
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        window.on_mention_key(move |threaded, key| {
            let Some(window) = weak.upgrade() else {
                return false;
            };
            if !allowed(&window, threaded) {
                return false;
            }
            let mut chars = key.chars();
            let Some(key) = chars.next() else {
                return false;
            };
            if chars.next().is_some() {
                return false;
            }
            let mut state = owner.state.lock().expect("composer mutex poisoned");
            if !ensure_scope(&mut state, &window, &binding, threaded) {
                return false;
            }
            let session = &mut state.bound_mut(threaded).session;
            if !session.visible() {
                return false;
            }
            if key == char::from(Key::Escape) {
                session.dismiss();
                drop(state);
                owner.publish(&window, threaded);
                return true;
            }
            if key == char::from(Key::UpArrow) || key == char::from(Key::DownArrow) {
                session.move_highlight(if key == char::from(Key::UpArrow) {
                    -1
                } else {
                    1
                });
                drop(state);
                owner.publish(&window, threaded);
                return true;
            }
            if key == char::from(Key::Return) || key == char::from(Key::Tab) {
                let key = session.highlighted_key();
                drop(state);
                if let Some(key) = key {
                    owner.choose(&window, &binding, threaded, &key);
                    return true;
                }
            }
            false
        });
        let weak = window.as_weak();
        let owner = this.clone();
        window.on_mention_context_changed(move || {
            if let Some(window) = weak.upgrade() {
                owner.context_changed(&window);
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live.clone();
        let handle = runtime;
        window.on_begin_compose(move |mode, event, sender, source, markdown| {
            if let Some(window) = weak.upgrade() {
                owner.begin(
                    &window,
                    &binding,
                    &handle,
                    mode,
                    event.to_string(),
                    sender.to_string(),
                    source.to_string(),
                    markdown,
                );
            }
        });
        let weak = window.as_weak();
        let owner = this.clone();
        let binding = live;
        window.on_cancel_compose(move || {
            if let Some(window) = weak.upgrade() {
                if !window.get_composer_busy() {
                    owner.restore_normal(&window, &binding);
                }
            }
        });
        this
    }

    fn open_emoji(&self, window: &AppWindow, live: &Arc<LiveUi>, threaded: bool) {
        if !allowed(window, threaded) {
            return;
        }
        let (caret, anchor, preedit) = cursor(window, threaded);
        {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            if !ensure_scope(&mut state, window, live, threaded) {
                return;
            }
            let bound = state.bound_mut(threaded);
            if bound.session.text().body != body(window, threaded) {
                return;
            }
            let Some(ticket) = bound.session.insertion(caret, anchor, preedit) else {
                return;
            };
            let Some(selection) = bound.selection.clone().filter(AccountSelection::is_current)
            else {
                return;
            };
            bound.session.dismiss();
            bound.latest = None;
            state.emoji = Some(EmojiPicker {
                threaded,
                ticket,
                selection,
                rows: emoji::search(""),
                highlighted: 0,
            });
        }
        window.set_emoji_query_text("".into());
        self.publish(window, threaded);
        self.publish_emoji(window);
    }

    fn emoji_valid(state: &State, window: &AppWindow, picker: &EmojiPicker) -> bool {
        let (caret, anchor, preedit) = cursor(window, picker.threaded);
        let bound = state.bound(picker.threaded);
        picker.selection.is_current()
            && allowed(window, picker.threaded)
            && scope(window, picker.threaded).as_ref() == bound.session.scope()
            && bound.session.text().body == body(window, picker.threaded)
            && bound
                .session
                .insertion_matches(&picker.ticket, caret, anchor, preedit)
    }

    fn publish_emoji(&self, window: &AppWindow) {
        let (open, threaded, selected, rows) = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            if state
                .emoji
                .as_ref()
                .is_some_and(|picker| !Self::emoji_valid(&state, window, picker))
            {
                state.emoji = None;
            }
            match &state.emoji {
                Some(picker) => (
                    true,
                    picker.threaded,
                    picker.highlighted as i32,
                    picker
                        .rows
                        .iter()
                        .map(|entry| EmojiItem {
                            key: entry.key.into(),
                            symbol: entry.text.into(),
                            name: entry.name.into(),
                        })
                        .collect::<Vec<_>>(),
                ),
                None => (false, false, 0, Vec::new()),
            }
        };
        window.set_emoji_items(ModelRc::new(VecModel::from(rows)));
        window.set_emoji_index(selected);
        window.set_emoji_threaded(threaded);
        window.set_emoji_open(open);
    }

    fn close_emoji(&self, window: &AppWindow, focus: bool) {
        let target = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            let target = state
                .emoji
                .as_ref()
                .filter(|picker| Self::emoji_valid(&state, window, picker))
                .map(|picker| picker.threaded);
            state.emoji = None;
            target
        };
        self.publish_emoji(window);
        if focus {
            if let Some(threaded) = target {
                window.invoke_focus_composer(threaded);
            }
        }
    }

    fn choose_emoji(&self, window: &AppWindow, live: &Arc<LiveUi>, key: &str) {
        let chosen = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            let Some(picker) = state.emoji.take() else {
                return;
            };
            if !Self::emoji_valid(&state, window, &picker) {
                None
            } else {
                let literal = picker
                    .rows
                    .iter()
                    .find(|entry| entry.key == key)
                    .map(|entry| entry.text);
                let (caret, anchor, preedit) = cursor(window, picker.threaded);
                literal.and_then(|literal| {
                    state
                        .bound_mut(picker.threaded)
                        .session
                        .insert_literal(&picker.ticket, literal, caret, anchor, preedit)
                        .ok()
                        .map(|caret| {
                            (
                                picker.threaded,
                                state.bound(picker.threaded).session.text().clone(),
                                caret,
                            )
                        })
                })
            }
        };
        self.publish_emoji(window);
        let Some((threaded, text, caret)) = chosen else {
            return;
        };
        self.remember(window, live, threaded, text.clone());
        if threaded {
            window.set_thread_composer_text(text.body.into());
        } else {
            window.set_composer_text(text.body.into());
        }
        self.publish(window, threaded);
        if let Ok(caret) = i32::try_from(caret) {
            window.invoke_place_mention_caret(threaded, caret);
        }
    }

    fn edited(&self, window: &AppWindow, live: &Arc<LiveUi>, threaded: bool, body: String) {
        self.close_emoji(window, false);
        let text = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            if !ensure_scope(&mut state, window, live, threaded) {
                return;
            }
            let bound = state.bound_mut(threaded);
            bound.session.edited(body);
            bound.session.text().clone()
        };
        self.remember(window, live, threaded, text);
        self.publish(window, threaded);
    }

    fn format(&self, window: &AppWindow, live: &Arc<LiveUi>, threaded: bool, markdown: bool) {
        self.close_emoji(window, false);
        let text = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            if !ensure_scope(&mut state, window, live, threaded) {
                return;
            }
            let session = &mut state.bound_mut(threaded).session;
            session.format(markdown);
            session.text().clone()
        };
        self.remember(window, live, threaded, text);
        self.publish(window, threaded);
    }

    fn remember(&self, window: &AppWindow, live: &Arc<LiveUi>, threaded: bool, text: ComposeText) {
        let Some(scope) = scope(window, threaded) else {
            return;
        };
        let thread = match &scope.target {
            Target::Room => None,
            Target::Thread(root) => Some(root.as_str()),
            _ => return,
        };
        if let Err(error) = live.remember_composed_draft(&scope.account, &scope.room, thread, text)
        {
            window.set_session_error(error.to_string().into());
        }
    }

    fn query(
        &self,
        window: &AppWindow,
        live: &Arc<LiveUi>,
        runtime: &Handle,
        threaded: bool,
        caret: i32,
        anchor: i32,
        preedit: bool,
    ) {
        if window.get_emoji_open() {
            self.publish_emoji(window);
            return;
        }
        let spawn = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            if !allowed(window, threaded) || !ensure_scope(&mut state, window, live, threaded) {
                state.bound_mut(threaded).session.dismiss();
                drop(state);
                self.publish(window, threaded);
                return;
            }
            let bound = state.bound_mut(threaded);
            // A cursor callback can precede the edited callback for the same
            // native operation. Do not interpret that source twice.
            if bound.session.text().body != body(window, threaded) {
                bound.session.dismiss();
                drop(state);
                self.publish(window, threaded);
                return;
            }
            let ticket = if selected_intent_at_caret(bound.session.text(), caret) {
                // Programmatic cursor placement after a choice must not reopen
                // completion over the full ID we just deliberately inserted.
                bound.session.dismiss();
                None
            } else {
                bound
                    .session
                    .request(caret, anchor, if preedit { "active" } else { "" })
            };
            bound.latest = ticket;
            if bound.running || bound.latest.is_none() {
                false
            } else {
                bound.running = true;
                true
            }
        };
        self.publish(window, threaded);
        if !spawn {
            return;
        }
        let owner = self.clone();
        let live = live.clone();
        let weak = window.as_weak();
        runtime.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let next={
                    let mut state=owner.state.lock().expect("composer mutex poisoned");
                    let bound=state.bound_mut(threaded);
                    match (bound.latest.take(),bound.selection.clone()) {
                        (Some(ticket),Some(selection)) if selection.is_current()=>Some((ticket,selection)),
                        _=>{bound.running=false;None}
                    }
                };
                let Some((ticket,selection))=next else { break; };
                let result=tokio::select! {
                    _=selection.revoked()=>None,
                    result=tokio::time::timeout(Duration::from_secs(5),live.manager.mention_candidates_for(&selection,ticket.room(),ticket.query()))=>result.ok().and_then(Result::ok),
                };
                let published=owner.clone();
                let _=weak.upgrade_in_event_loop(move |window| {
                    if !selection.is_current() || !allowed(&window,threaded) { return; }
                    {
                        let mut state=published.state.lock().expect("composer mutex poisoned");
                        if scope(&window,threaded).as_ref()!=state.bound(threaded).session.scope() { return; }
                        let rows=result.unwrap_or_default().into_iter().map(|row|Candidate{target:row.target,label:row.label}).collect();
                        state.bound_mut(threaded).session.publish(&ticket,rows);
                    }
                    published.publish(&window,threaded);
                });
            }
        });
    }

    fn choose(&self, window: &AppWindow, live: &Arc<LiveUi>, threaded: bool, key: &str) {
        if !allowed(window, threaded) {
            return;
        }
        let (caret, anchor, preedit) = cursor(window, threaded);
        let chosen = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            if !ensure_scope(&mut state, window, live, threaded) {
                return;
            }
            let bound = state.bound_mut(threaded);
            if bound.session.text().body != body(window, threaded) {
                return;
            }
            bound
                .session
                .choose(key, caret, anchor, if preedit { "active" } else { "" })
                .ok()
                .map(|caret| (bound.session.text().clone(), caret))
        };
        let Some((text, caret)) = chosen else {
            return;
        };
        self.remember(window, live, threaded, text.clone());
        if threaded {
            window.set_thread_composer_text(text.body.into());
        } else {
            window.set_composer_text(text.body.into());
        }
        self.publish(window, threaded);
        if let Ok(caret) = i32::try_from(caret) {
            window.invoke_place_mention_caret(threaded, caret);
        }
    }

    fn publish(&self, window: &AppWindow, threaded: bool) {
        let (rows, index) = {
            let state = self.state.lock().expect("composer mutex poisoned");
            let bound = state.bound(threaded);
            let valid = bound
                .selection
                .as_ref()
                .is_some_and(AccountSelection::is_current)
                && scope(window, threaded).as_ref() == bound.session.scope()
                && allowed(window, threaded);
            let rows = if valid {
                bound
                    .session
                    .candidates()
                    .iter()
                    .map(|row| MentionItem {
                        key: row.key().into(),
                        name: row.label.clone().into(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (rows, bound.session.highlighted() as i32)
        };
        if threaded {
            window.set_thread_mentions(ModelRc::new(VecModel::from(rows)));
            window.set_thread_mention_index(index);
        } else {
            window.set_composer_mentions(ModelRc::new(VecModel::from(rows)));
            window.set_composer_mention_index(index);
        }
    }

    fn context_changed(&self, window: &AppWindow) {
        self.close_emoji(window, false);
        {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            for threaded in [false, true] {
                let bound = state.bound_mut(threaded);
                if scope(window, threaded).as_ref() != bound.session.scope()
                    || !bound
                        .selection
                        .as_ref()
                        .is_some_and(AccountSelection::is_current)
                {
                    bound.session.clear();
                    bound.selection = None;
                    bound.baseline = None;
                } else {
                    bound.session.dismiss();
                }
                bound.latest = None;
            }
            if state.parked.as_ref().is_some_and(|saved| {
                !saved.selection.is_current()
                    || saved.scope.account != window.get_active_account_local_id().as_str()
                    || saved.scope.room != window.get_active_room_id().as_str()
            }) {
                state.parked = None;
            }
        }
        self.publish(window, false);
        self.publish(window, true);
    }

    pub(super) fn reserve_send(&self, scope: &Scope) -> Option<super::send::SendLease> {
        self.state
            .lock()
            .expect("composer mutex poisoned")
            .sends
            .reserve(scope.clone())
    }

    pub(super) fn release_send(&self, lease: &super::send::SendLease) -> bool {
        self.state
            .lock()
            .expect("composer mutex poisoned")
            .sends
            .release(lease)
    }

    pub fn snapshot(
        &self,
        window: &AppWindow,
        live: &Arc<LiveUi>,
        threaded: bool,
    ) -> Result<Submission, ComposeError> {
        self.close_emoji(window, false);
        if cursor(window, threaded).2 {
            return Err(ComposeError::InvalidMention);
        }
        let mut state = self.state.lock().expect("composer mutex poisoned");
        if !ensure_scope(&mut state, window, live, threaded) {
            return Err(ComposeError::InvalidMention);
        }
        let bound = state.bound_mut(threaded);
        if bound.session.text().body != body(window, threaded) {
            return Err(ComposeError::InvalidMention);
        }
        let text = bound.session.text().clone();
        text.validate()?;
        let selection = bound
            .selection
            .clone()
            .filter(AccountSelection::is_current)
            .ok_or(ComposeError::InvalidMention)?;
        let scope = bound
            .session
            .scope()
            .cloned()
            .ok_or(ComposeError::InvalidMention)?;
        if matches!(scope.target, Target::Edit(_)) && bound.baseline.is_none() {
            return Err(ComposeError::InvalidMention);
        }
        Ok(Submission {
            selection,
            scope,
            text,
            baseline: bound.baseline.clone(),
            revision: bound.session.revision(),
        })
    }

    pub fn matches(&self, window: &AppWindow, submitted: &Submission) -> bool {
        let state = self.state.lock().expect("composer mutex poisoned");
        let bound = state.bound(submitted.threaded());
        submitted.selection.is_current()
            && scope(window, submitted.threaded()).as_ref() == Some(&submitted.scope)
            && bound.session.revision() == submitted.revision
            && bound.session.text() == &submitted.text
            && body(window, submitted.threaded()) == submitted.text.body
    }

    pub fn complete(&self, window: &AppWindow, live: &Arc<LiveUi>, submitted: &Submission) -> bool {
        if !self.matches(window, submitted) {
            return false;
        }
        if matches!(submitted.scope.target, Target::Reply(_) | Target::Edit(_)) {
            self.restore_normal(window, live);
        } else {
            let threaded = submitted.threaded();
            {
                let mut state = self.state.lock().expect("composer mutex poisoned");
                state
                    .bound_mut(threaded)
                    .session
                    .replace(ComposeText::new(String::new(), submitted.text.format));
            }
            if threaded {
                window.set_thread_composer_text("".into());
            } else {
                window.set_composer_text("".into());
            }
            self.publish(window, threaded);
        }
        true
    }

    pub fn restore_normal(&self, window: &AppWindow, live: &Arc<LiveUi>) {
        self.close_emoji(window, false);
        if !window.get_live_session() {
            window.set_composer_mode(0);
            window.set_composer_target_id("".into());
            window.set_composer_target_label("".into());
            window.set_composer_text(window.get_composer_saved_draft());
            window.set_composer_markdown(window.get_composer_saved_markdown());
            window.set_composer_saved_draft("".into());
            window.set_composer_saved_markdown(false);
            return;
        }
        let account = window.get_active_account_local_id().to_string();
        let room = window.get_active_room_id().to_string();
        let saved = {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            state
                .parked
                .take()
                .filter(|saved| {
                    saved.selection.is_current()
                        && saved.scope.account == account
                        && saved.scope.room == room
                })
                .filter(|saved| saved.draft_revision == live.draft_revision(&account, &room))
                .map(|saved| saved.text)
                .unwrap_or_else(|| live.composed_draft(&account, &room, None))
        };
        window.set_composer_mode(0);
        window.set_composer_target_id("".into());
        window.set_composer_target_label("".into());
        window.set_composer_busy(false);
        window.set_composer_saved_draft("".into());
        window.set_composer_saved_markdown(false);
        {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            state.main.session.reset(
                Scope {
                    account: account.clone(),
                    room: room.clone(),
                    target: Target::Room,
                },
                saved.clone(),
            );
            state.main.selection = live.manager.account_selection(&account);
            state.main.baseline = None;
            state.main.latest = None;
        }
        window.set_composer_text(saved.body.into());
        window.set_composer_markdown(saved.format == TextFormat::Markdown);
        self.publish(window, false);
    }

    fn begin(
        &self,
        window: &AppWindow,
        live: &Arc<LiveUi>,
        runtime: &Handle,
        mode: i32,
        event: String,
        sender: String,
        source: String,
        markdown: bool,
    ) {
        if window.get_composer_busy() || event.is_empty() || !matches!(mode, 1 | 2) {
            return;
        }
        self.close_emoji(window, false);
        if !window.get_live_session() {
            if window.get_composer_mode() == 0 {
                window.set_composer_saved_draft(window.get_composer_text());
                window.set_composer_saved_markdown(window.get_composer_markdown());
            }
            window.set_composer_mode(mode);
            window.set_composer_target_id(event.into());
            window.set_composer_target_label(sender.into());
            window.set_composer_text(source.into());
            window.set_composer_markdown(markdown);
            return;
        }
        let account = window.get_active_account_local_id().to_string();
        let room = window.get_active_room_id().to_string();
        let Some(selection) = live.manager.account_selection(&account) else {
            return;
        };
        {
            let mut state = self.state.lock().expect("composer mutex poisoned");
            if !ensure_scope(&mut state, window, live, false) {
                return;
            }
            if window.get_composer_mode() == 0 {
                let saved = state.main.session.text().clone();
                state.parked = Some(ParkedDraft {
                    scope: Scope {
                        account: account.clone(),
                        room: room.clone(),
                        target: Target::Room,
                    },
                    text: saved,
                    selection: selection.clone(),
                    draft_revision: live.draft_revision(&account, &room),
                });
            }
            state.main.session.reset(
                Scope {
                    account: account.clone(),
                    room: room.clone(),
                    target: if mode == 1 {
                        Target::Reply(event.clone())
                    } else {
                        Target::Edit(event.clone())
                    },
                },
                ComposeText::new(
                    source.clone(),
                    if markdown {
                        TextFormat::Markdown
                    } else {
                        TextFormat::Plain
                    },
                ),
            );
            state.main.selection = Some(selection.clone());
            state.main.baseline = None;
            state.main.latest = None;
        }
        let load_revision = self
            .state
            .lock()
            .expect("composer mutex poisoned")
            .main
            .session
            .revision();
        let operation = window.get_composer_operation().wrapping_add(1);
        window.set_composer_operation(operation);
        window.set_composer_mode(mode);
        window.set_composer_target_id(event.clone().into());
        window.set_composer_target_label(sender.into());
        window.set_composer_text(source.into());
        window.set_composer_markdown(markdown);
        window.set_composer_busy(mode == 2);
        self.publish(window, false);
        if mode != 2 {
            return;
        }
        let owner = self.clone();
        let live = live.clone();
        let weak = window.as_weak();
        runtime.spawn(async move {
            let result=tokio::select! {
                _=selection.revoked()=>None,
                result=tokio::time::timeout(Duration::from_secs(15),live.manager.edit_composition_for(&selection,&room,&event))=>result.ok().and_then(Result::ok),
            };
            let _=weak.upgrade_in_event_loop(move |window| {
                if !selection.is_current() || window.get_composer_operation()!=operation || window.get_active_account_local_id().as_str()!=account || window.get_active_room_id().as_str()!=room || window.get_composer_mode()!=2 || window.get_composer_target_id().as_str()!=event { return; }
                window.set_composer_busy(false);
                let Some(edit)=result else { window.set_session_error("This edit could not be restored safely. Cancel or reopen the message.".into());return; };
                {
                    let mut state=owner.state.lock().expect("composer mutex poisoned");
                    if state.main.session.revision()!=load_revision {
                        drop(state);
                        window.set_session_error("Your text changed while the edit was loading. Cancel or reopen the message; your text is retained.".into());
                        return;
                    }
                    state.main.session.replace(edit.text.clone());state.main.baseline=Some(edit.baseline);
                }
                window.set_composer_text(edit.text.body.into());window.set_composer_markdown(edit.text.format==TextFormat::Markdown);
            });
        });
    }
}

fn scope(window: &AppWindow, threaded: bool) -> Option<Scope> {
    let account = window.get_active_account_local_id().to_string();
    let room = window.get_active_room_id().to_string();
    if account.is_empty() || room.is_empty() {
        return None;
    }
    let target = if threaded {
        let root = window.get_thread_root_id().to_string();
        if root.is_empty() {
            return None;
        }
        Target::Thread(root)
    } else {
        match window.get_composer_mode() {
            0 => Target::Room,
            1 => Target::Reply(window.get_composer_target_id().to_string()),
            2 => Target::Edit(window.get_composer_target_id().to_string()),
            _ => return None,
        }
    };
    Some(Scope {
        account,
        room,
        target,
    })
}
fn body(window: &AppWindow, threaded: bool) -> String {
    if threaded {
        window.get_thread_composer_text()
    } else {
        window.get_composer_text()
    }
    .to_string()
}
fn cursor(window: &AppWindow, threaded: bool) -> (i32, i32, bool) {
    if threaded {
        (
            window.get_thread_caret(),
            window.get_thread_anchor(),
            window.get_thread_preedit(),
        )
    } else {
        (
            window.get_composer_caret(),
            window.get_composer_anchor(),
            window.get_composer_preedit(),
        )
    }
}
fn allowed(window: &AppWindow, threaded: bool) -> bool {
    window.get_mention_ui_allowed()
        && if threaded {
            !window.get_thread_send_pending() && window.get_side_panel_tab() == 1
        } else {
            !window.get_composer_busy()
        }
}

fn ensure_scope(state: &mut State, window: &AppWindow, live: &Arc<LiveUi>, threaded: bool) -> bool {
    let Some(scope) = scope(window, threaded) else {
        return false;
    };
    let Some(selection) = live.manager.account_selection(&scope.account) else {
        return false;
    };
    let bound = state.bound_mut(threaded);
    if bound.session.scope() != Some(&scope)
        || !bound
            .selection
            .as_ref()
            .is_some_and(AccountSelection::is_current)
    {
        let text = match &scope.target {
            Target::Room => live.composed_draft(&scope.account, &scope.room, None),
            Target::Thread(root) => live.composed_draft(&scope.account, &scope.room, Some(root)),
            _ => ComposeText::new(
                body(window, threaded),
                if window.get_composer_markdown() {
                    TextFormat::Markdown
                } else {
                    TextFormat::Plain
                },
            ),
        };
        bound.session.reset(scope.clone(), text);
        bound.selection = Some(selection);
        bound.baseline = None;
        bound.latest = None;
    } else if bound.session.text().body != body(window, threaded) {
        let restored = match &scope.target {
            Target::Room => Some(live.composed_draft(&scope.account, &scope.room, None)),
            Target::Thread(root) => {
                Some(live.composed_draft(&scope.account, &scope.room, Some(root)))
            }
            _ => None,
        };
        if let Some(restored) = restored.filter(|text| text.body == body(window, threaded)) {
            bound.session.replace(restored);
        }
    }
    true
}

fn selected_intent_at_caret(text: &ComposeText, caret: i32) -> bool {
    usize::try_from(caret).ok().is_some_and(|caret| {
        text.mentions
            .iter()
            .any(|span| span.range.start <= caret && caret <= span.range.end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chosen_full_id_does_not_immediately_reopen_completion_but_pasted_text_can() {
        let source = ComposeText::plain("👋 @ca");
        let (selected, caret) = source
            .insert_mention(
                5..8,
                morpheus_matrix::MentionTarget::User("@casey:example.org".into()),
            )
            .unwrap();
        assert!(selected_intent_at_caret(&selected, caret as i32));
        assert!(!selected_intent_at_caret(&selected, 4));
        let pasted = ComposeText::plain(selected.body.clone());
        assert!(!selected_intent_at_caret(&pasted, caret as i32));
        let changed = selected.rebase_body(format!("{}x", selected.body));
        assert!(!selected_intent_at_caret(&changed, (caret + 1) as i32));
    }
}

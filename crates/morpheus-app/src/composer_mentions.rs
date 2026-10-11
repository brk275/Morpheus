//! Pure completion state. The UI adapter additionally holds a revocable
//! AccountSelection across every asynchronous query and final send.

use morpheus_matrix::{ComposeError, ComposeText, MentionTarget, TextFormat};
mod send;
pub mod ui;

use std::{fmt, ops::Range};

pub const MAX_RESULTS: usize = 8;
const MAX_QUERY_BYTES: usize = 128;

#[derive(Clone, Eq, PartialEq)]
pub enum Target {
    Room,
    Reply(String),
    Edit(String),
    Thread(String),
}

#[derive(Clone, Eq, PartialEq)]
pub struct Scope {
    pub account: String,
    pub room: String,
    pub target: Target,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Query {
    pub range: Range<usize>,
    pub text: String,
}

#[derive(Clone)]
pub struct Ticket {
    scope: Scope,
    generation: u64,
    revision: u64,
    request: u64,
    query: Query,
}

impl Ticket {
    pub fn query(&self) -> &str {
        &self.query.text
    }
    pub fn room(&self) -> &str {
        &self.scope.room
    }
    pub fn account(&self) -> &str {
        &self.scope.account
    }
}

#[derive(Clone)]
pub struct Candidate {
    pub target: MentionTarget,
    pub label: String,
}

impl Candidate {
    pub fn key(&self) -> &str {
        self.target.text()
    }
}

/// All source-bearing structures deliberately omit source-bearing Debug output.
impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompletionTicket")
            .field("generation", &self.generation)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
pub struct Session {
    scope: Option<Scope>,
    generation: u64,
    revision: u64,
    request: u64,
    text: ComposeText,
    query: Option<Query>,
    candidates: Vec<Candidate>,
    highlighted: usize,
}

impl Session {
    /// Called on a real scope/selection change, including returning to the same
    /// account after an intermediate selection. Old tickets never become live.
    pub fn reset(&mut self, scope: Scope, text: ComposeText) {
        self.scope = Some(scope);
        self.generation = self.generation.wrapping_add(1);
        self.revision = self.revision.wrapping_add(1);
        self.text = text;
        self.dismiss();
    }

    pub fn clear(&mut self) {
        self.scope = None;
        self.generation = self.generation.wrapping_add(1);
        self.text = ComposeText::default();
        self.dismiss();
    }

    pub fn scope(&self) -> Option<&Scope> {
        self.scope.as_ref()
    }
    pub fn text(&self) -> &ComposeText {
        &self.text
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }
    pub fn highlighted(&self) -> usize {
        self.highlighted
    }
    pub fn visible(&self) -> bool {
        self.query.is_some() && !self.candidates.is_empty()
    }

    pub fn edited(&mut self, body: String) {
        self.text = self.text.rebase_body(body);
        self.revision = self.revision.wrapping_add(1);
        self.dismiss();
    }

    /// Programmatic restore/format changes are separate from edited(): they
    /// preserve deliberate selection intent and still invalidate old callbacks.
    pub fn replace(&mut self, text: ComposeText) {
        self.text = text;
        self.revision = self.revision.wrapping_add(1);
        self.dismiss();
    }

    pub fn format(&mut self, markdown: bool) {
        self.replace(self.text.with_format(if markdown {
            TextFormat::Markdown
        } else {
            TextFormat::Plain
        }));
    }

    pub fn request(&mut self, caret: i32, anchor: i32, preedit: &str) -> Option<Ticket> {
        let Some(query) = query_at(&self.text, caret, anchor, preedit) else {
            self.dismiss();
            return None;
        };
        let scope = self.scope.clone()?;
        self.request = self.request.wrapping_add(1);
        self.query = Some(query.clone());
        self.candidates.clear();
        self.highlighted = 0;
        Some(Ticket {
            scope,
            generation: self.generation,
            revision: self.revision,
            request: self.request,
            query,
        })
    }

    pub fn matches(&self, ticket: &Ticket) -> bool {
        self.scope.as_ref() == Some(&ticket.scope)
            && self.generation == ticket.generation
            && self.revision == ticket.revision
            && self.request == ticket.request
            && self.query.as_ref() == Some(&ticket.query)
    }

    pub fn publish(&mut self, ticket: &Ticket, candidates: Vec<Candidate>) -> bool {
        if !self.matches(ticket) {
            return false;
        }
        self.candidates.clear();
        for candidate in candidates {
            if self.candidates.len() == MAX_RESULTS {
                break;
            }
            if candidate.target.validate().is_err()
                || self
                    .candidates
                    .iter()
                    .any(|previous| previous.target == candidate.target)
            {
                continue;
            }
            self.candidates.push(Candidate {
                target: candidate.target,
                label: safe_label(&candidate.label),
            });
        }
        self.highlighted = 0;
        true
    }

    pub fn move_highlight(&mut self, offset: i32) -> bool {
        if !self.visible() {
            return false;
        }
        self.highlighted =
            (self.highlighted as i32 + offset).rem_euclid(self.candidates.len() as i32) as usize;
        true
    }

    pub fn highlighted_key(&self) -> Option<String> {
        self.candidates
            .get(self.highlighted)
            .map(|candidate| candidate.key().to_owned())
    }

    /// The caller passes current cursor/anchor/preedit values again, so a mouse
    /// move or IME composition between result display and click cannot splice a
    /// stale range. Selection uses the target key, never a stale row index.
    pub fn choose(
        &mut self,
        key: &str,
        caret: i32,
        anchor: i32,
        preedit: &str,
    ) -> Result<usize, ComposeError> {
        let query = query_at(&self.text, caret, anchor, preedit)
            .filter(|query| Some(query) == self.query.as_ref())
            .ok_or(ComposeError::InvalidMention)?;
        let target = self
            .candidates
            .iter()
            .find(|candidate| candidate.key() == key)
            .map(|candidate| candidate.target.clone())
            .ok_or(ComposeError::InvalidMention)?;
        let (text, caret) = self.text.insert_mention(query.range, target)?;
        self.replace(text);
        Ok(caret)
    }

    pub fn dismiss(&mut self) {
        self.request = self.request.wrapping_add(1);
        self.query = None;
        self.candidates.clear();
        self.highlighted = 0;
    }
}

pub fn query_at(text: &ComposeText, caret: i32, anchor: i32, preedit: &str) -> Option<Query> {
    if text.body.len() > 64 * 1024 || caret < 0 || caret != anchor || !preedit.is_empty() {
        return None;
    }
    let caret = usize::try_from(caret).ok()?;
    if !text.body.is_char_boundary(caret) {
        return None;
    }
    let before = text.body.get(..caret)?;
    let mut start = caret;
    for (index, ch) in before.char_indices().rev() {
        if caret - index > MAX_QUERY_BYTES {
            return None;
        }
        if ch == '@' {
            start = index;
            break;
        }
        if ch.is_whitespace()
            || matches!(
                ch,
                '(' | ')' | ',' | ';' | '!' | '?' | '"' | '\'' | '`' | '*' | '<' | '>'
            )
        {
            return None;
        }
    }
    if before.get(start..start + 1) != Some("@") {
        return None;
    }
    // Avoid email addresses and a query in the middle of an identifier.
    if before[..start]
        .chars()
        .next_back()
        .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '@' | '_' | ':' | '/' | '.' | '-'))
    {
        return None;
    }
    if text.body[caret..]
        .chars()
        .next()
        .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, ':' | '_' | '/' | '.' | '-'))
    {
        return None;
    }
    let range = start..caret;
    if !text.allows_mention_range(range.clone()) {
        return None;
    }
    Some(Query {
        range,
        text: before[start + 1..].to_owned(),
    })
}

pub fn safe_label(source: &str) -> String {
    source.chars().filter(|ch| !ch.is_control() && !matches!(*ch,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')).take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope(account: &str, target: Target) -> Scope {
        Scope {
            account: account.into(),
            room: "!room:example.org".into(),
            target,
        }
    }
    fn candidate() -> Candidate {
        Candidate {
            target: MentionTarget::User("@casey:example.org".into()),
            label: "Casey".into(),
        }
    }
    #[test]
    fn unicode_caret_and_composition_are_not_guessed() {
        let text = ComposeText::plain("👋 @ca");
        assert_eq!(query_at(&text, 8, 8, "").unwrap().range, 5..8);
        assert!(query_at(&text, 1, 1, "").is_none());
        assert!(query_at(&text, 8, 5, "").is_none());
        assert!(query_at(&text, 8, 8, "composition").is_none());
        assert!(query_at(&ComposeText::plain("email@ca"), 8, 8, "").is_none());
    }
    #[test]
    fn stale_account_return_mode_revision_and_cursor_reject_results_or_choice() {
        let mut session = Session::default();
        session.reset(scope("a", Target::Room), ComposeText::plain("@ca"));
        let first = session.request(3, 3, "").unwrap();
        session.reset(scope("b", Target::Room), ComposeText::plain("@ca"));
        session.reset(scope("a", Target::Room), ComposeText::plain("@ca"));
        assert!(!session.publish(&first, vec![candidate()]));
        let next = session.request(3, 3, "").unwrap();
        session.edited("@ca".into());
        assert!(!session.publish(&next, vec![candidate()]));
        let next = session.request(3, 3, "").unwrap();
        assert!(session.publish(&next, vec![candidate()]));
        assert!(session.choose("@casey:example.org", 2, 2, "").is_err());
        session.reset(
            scope("a", Target::Reply("$reply".into())),
            ComposeText::plain("@ca"),
        );
        assert!(!session.publish(&next, vec![candidate()]));
    }
    #[test]
    fn keyboard_selection_uses_exact_target_and_dismiss_keeps_source() {
        let mut session = Session::default();
        session.reset(scope("a", Target::Room), ComposeText::plain("@"));
        let ticket = session.request(1, 1, "").unwrap();
        let room = Candidate {
            target: MentionTarget::Room,
            label: "Notify room".into(),
        };
        assert!(session.publish(&ticket, vec![candidate(), room]));
        assert!(session.move_highlight(-1));
        assert_eq!(session.highlighted_key().as_deref(), Some("@room"));
        assert_eq!(session.choose("@room", 1, 1, "").unwrap(), 5);
        assert_eq!(session.text().mentions.len(), 1);
        assert!(!session.visible());
        session.dismiss();
        assert_eq!(session.text().body, "@room");
        assert!(session.choose("@room", 5, 5, "").is_err());
    }
    #[test]
    fn bounded_rows_and_identity_label_never_replace_target_data() {
        let mut session = Session::default();
        session.reset(scope("a", Target::Room), ComposeText::plain("@"));
        let ticket = session.request(1, 1, "").unwrap();
        let rows = (0..20)
            .map(|i| Candidate {
                target: MentionTarget::User(format!("@casey{i}:example.org")),
                label: "\u{061c}Casey\n".repeat(20),
            })
            .collect();
        assert!(session.publish(&ticket, rows));
        assert_eq!(session.candidates().len(), MAX_RESULTS);
        for row in session.candidates() {
            assert!(row.label.chars().count() <= 80);
            assert!(!row.label.contains(['\n', '\u{061c}']));
            assert!(row.key().starts_with("@casey"));
        }
    }
    #[test]
    fn source_scope_clear_invalidates_tickets_and_does_not_expose_debug() {
        let mut session = Session::default();
        session.reset(
            scope("private-account", Target::Thread("$thread".into())),
            ComposeText::plain("@private"),
        );
        let ticket = session.request(8, 8, "").unwrap();
        session.clear();
        assert!(!session.publish(&ticket, vec![candidate()]));
        assert!(!format!("{ticket:?}").contains("private"));
    }
}

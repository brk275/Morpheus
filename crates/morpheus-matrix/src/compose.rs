//! Explicit mention intent, independent of display names and clipboard text.
//! Draft HTML is a local canonical encoding in the encrypted SDK draft slot;
//! ordinary Matrix HTML or an arbitrary link never confers selection intent.

use std::{fmt, ops::Range};

use matrix_sdk::ruma::{MatrixToUri, UserId, events::Mentions, matrix_uri::MatrixId};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ruma::html::{Html, NodeData, NodeRef};

use crate::TextFormat;
use crate::draft_recovery::{
    DraftEnvelope, MAX_ENVELOPE_HEX_BYTES, QuarantineReason, QuarantinedRow, ReadOnlyRow,
    RowRevision,
};

pub const MAX_COMPOSE_BYTES: usize = 64 * 1024;
pub const MAX_MENTION_SPANS: usize = 32;
/// Largest draft row this build parses: the escaped live text, its mention
/// anchors, the bounded recovery attribute and the span markup.
const MAX_DRAFT_HTML_BYTES: usize =
    MAX_COMPOSE_BYTES * 8 + MAX_MENTION_SPANS * 4096 + MAX_ENVELOPE_HEX_BYTES + 128;
const DRAFT_VERSION_PLAIN: &str = "1";
const DRAFT_VERSION_RECOVERY: &str = "2";
/// Every Morpheus-written draft row carries an attribute with this prefix.
const MORPHEUS_MARKER: &str = "data-morpheus-";

#[derive(Clone, Eq, PartialEq)]
pub enum MentionTarget {
    User(String),
    Room,
}

impl MentionTarget {
    pub fn text(&self) -> &str {
        match self {
            Self::User(id) => id,
            Self::Room => "@room",
        }
    }

    pub fn validate(&self) -> Result<(), ComposeError> {
        match self {
            Self::User(id) if id.len() <= 255 && !id.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) => {
                UserId::parse(id)
                    .map(|_| ())
                    .map_err(|_| ComposeError::InvalidMention)
            }
            Self::Room => Ok(()),
            _ => Err(ComposeError::InvalidMention),
        }
    }

    pub(crate) fn anchor(&self) -> Result<String, ComposeError> {
        self.validate()?;
        Ok(match self {
            Self::User(id) => format!(
                "<a href=\"{}\">{}</a>",
                escape_html(
                    &UserId::parse(id)
                        .map_err(|_| ComposeError::InvalidMention)?
                        .matrix_to_uri()
                        .to_string()
                ),
                literal_anchor_text(id),
            ),
            Self::Room => "<span data-morpheus-room=\"1\">@room</span>".to_owned(),
        })
    }
}

impl fmt::Debug for MentionTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User(_) => "User([redacted])",
            Self::Room => "Room",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MentionSpan {
    pub range: Range<usize>,
    pub target: MentionTarget,
}

#[derive(Clone, Default, Eq, PartialEq)]
pub struct ComposeText {
    pub body: String,
    pub format: TextFormat,
    pub mentions: Vec<MentionSpan>,
}

impl From<String> for ComposeText {
    fn from(body: String) -> Self {
        Self::plain(body)
    }
}
impl From<&str> for ComposeText {
    fn from(body: &str) -> Self {
        Self::plain(body)
    }
}

impl fmt::Debug for ComposeText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ComposeText")
            .field("bytes", &self.body.len())
            .field("format", &self.format)
            .field("mention_count", &self.mentions.len())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ComposeError {
    #[error("The message is too large.")]
    TooLarge,
    #[error("The selected mention is no longer valid. Select it again.")]
    InvalidMention,
    #[error("The saved send status for this draft could not be encoded.")]
    InvalidRecovery,
}

pub struct RestoredCompose {
    pub text: ComposeText,
    /// Non-Morpheus HTML was present and ignored; only the plain body is used.
    pub discarded_rich_intent: bool,
    /// Recovery metadata saved with the text, or a fresh envelope for legacy
    /// rows. For a quarantined row that fits the carry bound the envelope
    /// holds the complete original row in `quarantine()`.
    pub envelope: DraftEnvelope,
    /// The row cannot be carried completely or rewritten faithfully (too
    /// large, it holds attachments, or its draft kind is one this build does
    /// not write): it stays untouched in the store and nothing may be written
    /// to this scope until the user explicitly discards exactly that row.
    pub read_only: Option<ReadOnlyRow>,
    /// Opaque identity of the unreadable original (carried or read-only),
    /// minted for the complete row this value was restored from. `None` when
    /// nothing is blocked. The store layer reconciles it with the identity it
    /// already holds when a reload returns the identical complete row.
    pub revision: Option<RowRevision>,
}

impl RestoredCompose {
    /// Why this scope refuses new sends and, when read-only, saves.
    pub fn blocked(&self) -> Option<QuarantineReason> {
        self.read_only
            .map(|row| row.reason)
            .or_else(|| self.envelope.quarantine().map(QuarantinedRow::reason))
    }

    /// Identity of the unreadable original, carried or read-only.
    pub fn blocked_revision(&self) -> Option<&RowRevision> {
        self.revision.as_ref()
    }
}

/// Outcome of reading a stored draft row.
enum Decoded {
    /// A canonical Morpheus row: typed text plus its envelope.
    Text(ComposeText, DraftEnvelope),
    /// HTML not written by Morpheus; the plain body stands alone.
    Foreign,
    /// Morpheus-written but not readable by this build. `text` is the typed
    /// live text when the span itself was canonical.
    Quarantine {
        reason: QuarantineReason,
        text: Option<ComposeText>,
    },
}

impl ComposeText {
    pub fn plain(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            ..Self::default()
        }
    }

    pub fn new(body: impl Into<String>, format: TextFormat) -> Self {
        Self {
            body: body.into(),
            format,
            mentions: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), ComposeError> {
        if self.body.len() > MAX_COMPOSE_BYTES || self.mentions.len() > MAX_MENTION_SPANS {
            return Err(ComposeError::TooLarge);
        }
        let mut previous_end = 0;
        for span in &self.mentions {
            span.target.validate()?;
            if span.range.start < previous_end
                || span.range.start >= span.range.end
                || self.body.get(span.range.clone()) != Some(span.target.text())
                || !self.allows_mention_range(span.range.clone())
                || !token_boundaries(&self.body, span.range.clone())
            {
                return Err(ComposeError::InvalidMention);
            }
            previous_end = span.range.end;
        }
        Ok(())
    }

    /// Pinned parser source ranges are byte offsets, like Slint selection offsets.
    /// A query/mention must lie in one ordinary text node, outside links/images/code.
    pub fn allows_mention_range(&self, range: Range<usize>) -> bool {
        if range.start >= range.end || self.body.get(range.clone()).is_none() {
            return false;
        }
        if self.format == TextFormat::Plain {
            return true;
        }
        // An escaped at-sign is deliberately literal even if pulldown emits text.
        if self.body[..range.start]
            .bytes()
            .rev()
            .take_while(|b| *b == b'\\')
            .count()
            % 2
            != 0
        {
            return false;
        }
        // Classify surrounding Markdown, not punctuation inside the chosen ID.
        // Equal-byte masking preserves parser offsets and keeps generated user
        // labels literal even for historical underscore/asterisk localparts.
        let mut masked = self.body.clone();
        masked.replace_range(range.clone(), &"m".repeat(range.len()));
        let mut excluded = 0usize;
        for (event, source) in Parser::new_ext(&masked, markdown_options()).into_offset_iter() {
            match event {
                Event::Start(Tag::CodeBlock(_) | Tag::Link { .. } | Tag::Image { .. }) => {
                    excluded += 1
                }
                Event::End(TagEnd::CodeBlock | TagEnd::Link | TagEnd::Image) => {
                    excluded = excluded.saturating_sub(1)
                }
                Event::Text(_)
                    if excluded == 0 && source.start <= range.start && source.end >= range.end =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// Preserve intent only for unchanged source spans. Same-text edited events
    /// cannot prove that a selection survived undo/IME replacement, so remove it.
    pub fn rebase_body(&self, body: String) -> Self {
        if self.body == body {
            return Self::new(body, self.format);
        }
        let mut prefix = self
            .body
            .bytes()
            .zip(body.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        while !self.body.is_char_boundary(prefix) || !body.is_char_boundary(prefix) {
            prefix -= 1;
        }
        let mut suffix = self
            .body
            .bytes()
            .rev()
            .zip(body.bytes().rev())
            .take_while(|(a, b)| a == b)
            .count();
        while !self.body.is_char_boundary(self.body.len() - suffix)
            || !body.is_char_boundary(body.len() - suffix)
        {
            suffix -= 1;
        }
        // Independent full prefix/suffix matches can overlap after deleting or
        // inserting identical occurrences. A body-only edited callback cannot
        // establish which occurrence survived; never transfer its notification.
        if prefix.saturating_add(suffix) > self.body.len().min(body.len()) {
            return Self::new(body, self.format);
        }
        let old_end = self.body.len() - suffix;
        let new_end = body.len() - suffix;
        let mut next = Self::new(body, self.format);
        for span in &self.mentions {
            let range = if span.range.end <= prefix {
                Some(span.range.clone())
            } else if span.range.start >= old_end {
                let start = new_end.checked_add(span.range.start - old_end);
                let end = new_end.checked_add(span.range.end - old_end);
                start.zip(end).map(|(start, end)| start..end)
            } else {
                None
            };
            if let Some(range) = range {
                if next.body.get(range.clone()) == Some(span.target.text())
                    && next.allows_mention_range(range.clone())
                    && token_boundaries(&next.body, range.clone())
                {
                    next.mentions.push(MentionSpan {
                        range,
                        target: span.target.clone(),
                    });
                }
            }
        }
        next
    }

    pub fn with_format(&self, format: TextFormat) -> Self {
        let mut next = self.clone();
        next.format = format;
        next.mentions.retain(|span| {
            Self::new(next.body.clone(), format).allows_mention_range(span.range.clone())
        });
        next
    }

    pub fn insert_mention(
        &self,
        range: Range<usize>,
        target: MentionTarget,
    ) -> Result<(Self, usize), ComposeError> {
        self.validate()?;
        target.validate()?;
        if !self.allows_mention_range(range.clone()) {
            return Err(ComposeError::InvalidMention);
        }
        let mut body = self.body.clone();
        body.replace_range(range.clone(), target.text());
        let end = range.start + target.text().len();
        // Here the exact replaced range is known; do not apply an ambiguous
        // body-only diff or discard unrelated selections on identical text.
        let mut next = Self::new(body, self.format);
        for span in &self.mentions {
            let shifted = if span.range.end <= range.start {
                Some(span.range.clone())
            } else if span.range.start >= range.end {
                Some(end + (span.range.start - range.end)..end + (span.range.end - range.end))
            } else {
                None
            };
            if let Some(shifted) = shifted {
                if token_boundaries(&next.body, shifted.clone())
                    && next.allows_mention_range(shifted.clone())
                {
                    next.mentions.push(MentionSpan {
                        range: shifted,
                        target: span.target.clone(),
                    });
                }
            }
        }
        next.mentions.push(MentionSpan {
            range: range.start..end,
            target,
        });
        next.mentions.sort_by_key(|span| span.range.start);
        next.validate()?;
        Ok((next, end))
    }

    /// Replace one known native UTF-8 selection with literal text. Unlike a
    /// body-only diff, the exact edit can preserve every unaffected selection.
    /// Literal IDs, links and emoji never add Matrix mention intent.
    pub fn insert_literal(
        &self,
        range: Range<usize>,
        literal: &str,
    ) -> Result<(Self, usize), ComposeError> {
        self.validate()?;
        if range.start > range.end || self.body.get(range.clone()).is_none() {
            return Err(ComposeError::InvalidMention);
        }
        let length = self
            .body
            .len()
            .checked_sub(range.len())
            .and_then(|length| length.checked_add(literal.len()))
            .filter(|length| *length <= MAX_COMPOSE_BYTES)
            .ok_or(ComposeError::TooLarge)?;
        let mut body = String::with_capacity(length);
        body.push_str(&self.body[..range.start]);
        body.push_str(literal);
        body.push_str(&self.body[range.end..]);
        let end = range.start + literal.len();
        let mut next = Self::new(body, self.format);
        for span in &self.mentions {
            let shifted = if span.range.end <= range.start {
                Some(span.range.clone())
            } else if span.range.start >= range.end {
                Some(end + (span.range.start - range.end)..end + (span.range.end - range.end))
            } else {
                None
            };
            if let Some(range) = shifted {
                if next.body.get(range.clone()) == Some(span.target.text())
                    && token_boundaries(&next.body, range.clone())
                    && next.allows_mention_range(range.clone())
                {
                    next.mentions.push(MentionSpan {
                        range,
                        target: span.target.clone(),
                    });
                }
            }
        }
        next.validate()?;
        Ok((next, end))
    }

    pub fn trimmed_for_send(&self) -> Result<Self, ComposeError> {
        self.validate()?;
        let start = self.body.len() - self.body.trim_start().len();
        let end = self.body.trim_end().len().max(start);
        let mut next = Self::new(self.body[start..end].to_owned(), self.format);
        for span in &self.mentions {
            if span.range.start < start || span.range.end > end {
                return Err(ComposeError::InvalidMention);
            }
            next.mentions.push(MentionSpan {
                range: span.range.start - start..span.range.end - start,
                target: span.target.clone(),
            });
        }
        next.validate()?;
        Ok(next)
    }

    /// Always returns explicit metadata, including an empty set for ordinary text.
    pub fn matrix_mentions(&self, own_user: &UserId) -> Result<Mentions, ComposeError> {
        self.validate()?;
        let mut mentions = Mentions::new();
        for span in &self.mentions {
            match &span.target {
                MentionTarget::Room => mentions.room = true,
                MentionTarget::User(id) => {
                    let id = UserId::parse(id).map_err(|_| ComposeError::InvalidMention)?;
                    if id != own_user {
                        mentions.user_ids.insert(id);
                    }
                }
            }
        }
        Ok(mentions)
    }

    /// Legacy canonical draft encoding without recovery metadata. Kept so
    /// rows written by earlier builds stay decodable and testable; new rows
    /// are written with `encode_draft`. Never used for outbound events.
    pub fn draft_html(&self) -> Result<String, ComposeError> {
        self.render_draft(DRAFT_VERSION_PLAIN, None)
    }

    /// Canonical draft encoding carrying the recovery envelope in one
    /// attribute, so text and metadata share a single encrypted row write.
    pub fn encode_draft(&self, envelope: &DraftEnvelope) -> Result<String, ComposeError> {
        let meta = envelope
            .encode()
            .map_err(|_| ComposeError::InvalidRecovery)?;
        self.render_draft(DRAFT_VERSION_RECOVERY, Some(&meta))
    }

    fn render_draft(&self, version: &str, meta: Option<&str>) -> Result<String, ComposeError> {
        self.validate()?;
        let format = match self.format {
            TextFormat::Plain => "plain",
            TextFormat::Markdown => "markdown",
        };
        let mut html =
            format!("<span data-morpheus-compose=\"{version}\" data-format=\"{format}\"");
        if let Some(meta) = meta {
            // The envelope is lowercase hex, so no attribute escaping applies.
            html.push_str(" data-morpheus-meta=\"");
            html.push_str(meta);
            html.push('"');
        }
        html.push('>');
        let mut cursor = 0;
        for span in &self.mentions {
            html.push_str(&escape_html(&self.body[cursor..span.range.start]));
            html.push_str(&span.target.anchor()?);
            cursor = span.range.end;
        }
        html.push_str(&escape_html(&self.body[cursor..]));
        html.push_str("</span>");
        Ok(html)
    }

    /// Restore a stored row whose draft kind is a new message without
    /// attachments; `restore_stored_draft` takes the complete stored row.
    pub fn restore_draft(body: String, html: Option<&str>) -> RestoredCompose {
        Self::restore_stored_draft(body, html, "new", false)
    }

    /// Restore a complete stored row. `kind` names the stored draft kind
    /// (`new`, `reply:<event id>` or `edit:<event id>`) and
    /// `has_attachments` whether the row carries attachments. Neither changes
    /// how the text is shown, but a row whose kind this build does not write,
    /// or that holds attachments, is preserved read-only even when its text is
    /// readable: a later save would otherwise rewrite it as a plain new
    /// message and silently drop the reply or edit target or the attachments.
    /// An unreadable row is carried verbatim or kept read-only in place.
    pub fn restore_stored_draft(
        body: String,
        html: Option<&str>,
        kind: &str,
        has_attachments: bool,
    ) -> RestoredCompose {
        let unsupported_fields = kind != "new" || has_attachments;
        // Readable text with stored fields this build cannot write back: shown,
        // never rewritten until the user discards exactly this row.
        let preserve = |restored: RestoredCompose| {
            if restored.blocked().is_none() && unsupported_fields {
                RestoredCompose {
                    read_only: Some(ReadOnlyRow {
                        reason: QuarantineReason::UnsupportedFields,
                    }),
                    revision: Some(RowRevision::fresh()),
                    ..restored
                }
            } else {
                restored
            }
        };
        let plain = |body: String| RestoredCompose {
            text: Self::plain(body),
            discarded_rich_intent: false,
            envelope: DraftEnvelope::fresh(),
            read_only: None,
            revision: None,
        };
        let Some(html) = html else {
            return preserve(plain(body));
        };
        match Self::decode_draft(&body, html) {
            Decoded::Text(text, envelope) => {
                // A readable row may still carry an earlier quarantined
                // original; that carry keeps its own identity.
                let revision = envelope.quarantine().is_some().then(RowRevision::fresh);
                preserve(RestoredCompose {
                    text,
                    discarded_rich_intent: false,
                    envelope,
                    read_only: None,
                    revision,
                })
            }
            Decoded::Foreign => preserve(RestoredCompose {
                discarded_rich_intent: true,
                ..plain(body)
            }),
            Decoded::Quarantine { reason, text } => {
                // The complete original row (both columns and its kind)
                // travels with the scope until the user discards exactly it;
                // a row that cannot be carried completely stays read-only.
                // Either way the identity is minted for the complete row.
                let carried = QuarantinedRow::new(reason, &body, Some(html), kind, has_attachments);
                let text = text.unwrap_or_else(|| Self::plain(body));
                match carried {
                    Some(row) => {
                        let mut envelope = DraftEnvelope::fresh();
                        envelope.set_quarantine(Some(row));
                        RestoredCompose {
                            text,
                            discarded_rich_intent: false,
                            envelope,
                            read_only: None,
                            revision: Some(RowRevision::fresh()),
                        }
                    }
                    None => RestoredCompose {
                        text,
                        discarded_rich_intent: false,
                        envelope: DraftEnvelope::fresh(),
                        read_only: Some(ReadOnlyRow { reason }),
                        revision: Some(RowRevision::fresh()),
                    },
                }
            }
        }
    }

    fn decode_draft(body: &str, html: &str) -> Decoded {
        let marked = html.contains(MORPHEUS_MARKER);
        let malformed = || {
            if marked {
                Decoded::Quarantine {
                    reason: QuarantineReason::Malformed,
                    text: None,
                }
            } else {
                Decoded::Foreign
            }
        };
        if body.len() > MAX_COMPOSE_BYTES || html.len() > MAX_DRAFT_HTML_BYTES {
            return if marked {
                Decoded::Quarantine {
                    reason: QuarantineReason::TooLarge,
                    text: None,
                }
            } else {
                Decoded::Foreign
            };
        }
        let parsed = Html::parse(html);
        let nodes: Vec<_> = parsed.children().collect();
        if nodes.len() != 1 {
            return malformed();
        }
        let Some(root) = nodes.first() else {
            return malformed();
        };
        let Some(element) = root.as_element() else {
            return malformed();
        };
        if element.name.local.as_ref() != "span" {
            return malformed();
        }
        let attrs = element.attrs.borrow();
        let Some(version) = attribute(&attrs, "data-morpheus-compose") else {
            return malformed();
        };
        let meta = match version.as_str() {
            DRAFT_VERSION_PLAIN if attrs.len() == 2 => None,
            DRAFT_VERSION_RECOVERY if attrs.len() == 3 => {
                match attribute(&attrs, "data-morpheus-meta") {
                    Some(meta) => Some(meta),
                    None => return malformed(),
                }
            }
            DRAFT_VERSION_PLAIN | DRAFT_VERSION_RECOVERY => return malformed(),
            _ => {
                return Decoded::Quarantine {
                    reason: QuarantineReason::UnsupportedVersion,
                    text: None,
                };
            }
        };
        let format = match attribute(&attrs, "data-format").as_deref() {
            Some("plain") => TextFormat::Plain,
            Some("markdown") => TextFormat::Markdown,
            _ => return malformed(),
        };
        drop(attrs);
        let Some(decoded) = Self::decode_children(root, format) else {
            return malformed();
        };
        // Byte-for-byte canonical equality also rejects parser repairs, comments,
        // duplicate attributes, unknown metadata, and alternative URI spellings.
        // The metadata attribute is compared verbatim here and interpreted
        // separately, so an unreadable envelope never discards the text's
        // formatting or mention intent.
        if decoded.body != body
            || decoded
                .render_draft(&version, meta.as_deref())
                .ok()
                .as_deref()
                != Some(html)
        {
            return malformed();
        }
        match meta {
            None => Decoded::Text(decoded, DraftEnvelope::fresh()),
            Some(meta) => match DraftEnvelope::decode(&meta) {
                Ok(envelope) => Decoded::Text(decoded, envelope),
                Err(error) => Decoded::Quarantine {
                    reason: error.into(),
                    text: Some(decoded),
                },
            },
        }
    }

    fn decode_children(root: &NodeRef, format: TextFormat) -> Option<Self> {
        let mut decoded = Self::new(String::new(), format);
        for node in root.children() {
            match node.data() {
                NodeData::Text(text) => decoded.body.push_str(&text.borrow()),
                NodeData::Element(element) => {
                    let attrs = element.attrs.borrow();
                    if attrs.len() != 1 {
                        return None;
                    }
                    let target = match element.name.local.as_ref() {
                        "a" => {
                            let uri = MatrixToUri::parse(&attribute(&attrs, "href")?).ok()?;
                            let MatrixId::User(id) = uri.id() else {
                                return None;
                            };
                            MentionTarget::User(id.to_string())
                        }
                        "span" if attribute(&attrs, "data-morpheus-room")? == "1" => {
                            MentionTarget::Room
                        }
                        _ => return None,
                    };
                    drop(attrs);
                    let text = text_only(&node)?;
                    if text != target.text() || decoded.mentions.len() == MAX_MENTION_SPANS {
                        return None;
                    }
                    let start = decoded.body.len();
                    decoded.body.push_str(&text);
                    decoded.mentions.push(MentionSpan {
                        range: start..decoded.body.len(),
                        target,
                    });
                }
                _ => return None,
            }
        }
        Some(decoded)
    }
}

/// Adjacent identifier characters can change the apparent target even when
/// the selected byte range itself is unchanged. Commas/parentheses remain
/// normal punctuation; a dot is conservative because it can extend a server.
fn token_boundaries(body: &str, range: Range<usize>) -> bool {
    let continues = |ch: char| {
        ch.is_alphanumeric()
            || matches!(
                ch,
                '.' | ':' | '/' | '_' | '-' | '=' | '+' | '@' | '[' | ']' | '%'
            )
    };
    !body[..range.start]
        .chars()
        .next_back()
        .is_some_and(continues)
        && !body[range.end..].chars().next().is_some_and(continues)
}

fn attribute(
    attrs: &std::collections::BTreeSet<ruma::html::Attribute>,
    name: &str,
) -> Option<String> {
    let attr = attrs.iter().find(|attr| attr.name.local.as_ref() == name)?;
    if !attr.name.ns.as_ref().is_empty() {
        return None;
    }
    Some(attr.value.to_string())
}

fn text_only(node: &NodeRef) -> Option<String> {
    let mut text = String::new();
    for child in node.children() {
        text.push_str(&child.as_text()?.borrow());
    }
    Some(text)
}

pub(crate) fn markdown_options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

// CommonMark parses text between inline HTML anchor tags. Entity-encoding
// every target character prevents valid MXID underscores/asterisks from
// becoming emphasis; decoding happens only after Markdown structure parsing.
fn literal_anchor_text(source: &str) -> String {
    source
        .chars()
        .map(|ch| format!("&#{};", u32::from(ch)))
        .collect()
}

pub(crate) fn escape_html(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    for ch in source.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            '\r' => output.push_str("&#13;"),
            '\n' => output.push_str("&#10;"),
            _ => output.push(ch),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    fn selected(prefix: &str, suffix: &str) -> ComposeText {
        let mut value = ComposeText::plain(format!("{prefix}@casey:example.org{suffix}"));
        value.mentions.push(MentionSpan {
            range: prefix.len()..prefix.len() + "@casey:example.org".len(),
            target: MentionTarget::User("@casey:example.org".into()),
        });
        value
    }
    #[test]
    fn unicode_trim_shifts_intent_and_retains_untrimmed_original() {
        let value = selected("\u{2003}\n", "\r\n ");
        let sent = value.trimmed_for_send().unwrap();
        assert_eq!(sent.mentions[0].range, 0..sent.body.len());
        assert_eq!(value.mentions[0].range.start, 4);
        assert_eq!(
            ComposeText::plain(" \u{2003}")
                .trimmed_for_send()
                .unwrap()
                .body,
            ""
        );
    }
    #[test]
    fn unicode_edit_before_shifts_but_intersection_and_same_text_edit_remove_intent() {
        let value = selected("hi ", " 👋");
        let shifted = value.rebase_body(format!("🦋 {}", value.body));
        assert_eq!(
            shifted.mentions[0].range.start,
            value.mentions[0].range.start + 5
        );
        assert!(
            value
                .rebase_body(value.body.replace("casey", "riley"))
                .mentions
                .is_empty()
        );
        assert!(value.rebase_body(value.body.clone()).mentions.is_empty());
    }
    #[test]
    fn literal_ids_are_not_mentions_and_explicit_selection_survives_same_text() {
        let value = ComposeText::plain("@casey:example.org");
        let (selected, caret) = value
            .insert_mention(0..value.body.len(), MentionTarget::User(value.body.clone()))
            .unwrap();
        assert_eq!(caret, value.body.len());
        assert!(value.mentions.is_empty());
        assert_eq!(selected.mentions.len(), 1);
    }
    #[test]
    fn canonical_encrypted_draft_round_trip_and_arbitrary_html_rejected() {
        let value = selected("\n<&\"'\r ", "\n");
        let html = value.draft_html().unwrap();
        let restored = ComposeText::restore_draft(value.body.clone(), Some(&html));
        assert_eq!(restored.text, value);
        assert!(!restored.discarded_rich_intent);
        for bad in [
            html.replace("data-morpheus-compose=\"1\"", "data-morpheus-compose=\"2\""),
            format!("{html}<!-- x -->"),
            html.replace("@casey", "@riley"),
        ] {
            let restored = ComposeText::restore_draft(value.body.clone(), Some(&bad));
            assert_eq!(restored.text.body, value.body);
            assert!(restored.text.mentions.is_empty());
            assert!(restored.discarded_rich_intent);
        }
        assert!(
            ComposeText::restore_draft(value.body.clone(), None)
                .text
                .mentions
                .is_empty()
        );
    }
    #[test]
    fn recovery_envelope_rides_the_canonical_draft_and_records_keep_their_text() {
        use crate::draft_recovery::{DraftEnvelope, Generation};
        let value = selected("send ", " now");
        let mut envelope = DraftEnvelope::fresh();
        let nonce = Generation::fresh();
        envelope
            .begin_pending(nonce.clone(), envelope.generation().clone(), &value, false)
            .unwrap();
        assert!(envelope.mark_queued(&nonce, "abcdef0123456789abcdef0123456789"));
        envelope.index_thread("$root:example.org").unwrap();
        // The live text moves on; the record keeps the submitted text.
        let live = ComposeText::new("something newer".into(), TextFormat::Markdown);
        envelope.set_generation(Generation::fresh());
        let html = live.encode_draft(&envelope).unwrap();
        assert!(html.contains("data-morpheus-compose=\"2\""));
        let restored = ComposeText::restore_draft(live.body.clone(), Some(&html));
        assert_eq!(restored.text, live);
        assert_eq!(restored.envelope, envelope);
        assert_eq!(restored.envelope.pending()[0].text().to_compose().unwrap(), value);
        assert!(!restored.discarded_rich_intent && restored.blocked().is_none());
        // A metadata-only row keeps its envelope with an empty body.
        let empty = ComposeText::new(String::new(), TextFormat::Markdown);
        let html = empty.encode_draft(&envelope).unwrap();
        let restored = ComposeText::restore_draft(String::new(), Some(&html));
        assert_eq!(restored.text, empty);
        assert_eq!(restored.envelope, envelope);
        // Legacy canonical rows migrate to a fresh, empty envelope.
        let legacy = ComposeText::restore_draft(value.body.clone(), Some(&value.draft_html().unwrap()));
        assert_eq!(legacy.text, value);
        assert!(legacy.envelope.is_empty() && legacy.blocked().is_none() && !legacy.discarded_rich_intent);
        // Foreign HTML is not ours: the plain body stands alone.
        let foreign = ComposeText::restore_draft("x".into(), Some("<b>x</b>"));
        assert!(foreign.discarded_rich_intent && foreign.blocked().is_none());
    }

    #[test]
    fn unreadable_morpheus_rows_are_quarantined_with_the_complete_original_until_discarded() {
        use crate::draft_recovery::{DraftEnvelope, Generation, QuarantineReason};
        let value = selected("send ", " now");
        let mut envelope = DraftEnvelope::fresh();
        let nonce = Generation::fresh();
        envelope
            .begin_pending(nonce.clone(), envelope.generation().clone(), &value, false)
            .unwrap();
        let html = value.encode_draft(&envelope).unwrap();
        let meta = envelope.encode().unwrap();
        // Malformed metadata with canonical text: the typed text and intent
        // restore, no pending send is inferred, the full row is carried.
        let broken = html.replace("data-morpheus-meta=\"", "data-morpheus-meta=\"ff");
        // The stored plain column may differ from the HTML's text; both
        // columns and the stored kind are carried, so a later edit of the live
        // draft destroys neither.
        let plain_column = format!("{} (plain column differs)", value.body);
        let restored = ComposeText::restore_stored_draft(
            plain_column.clone(),
            Some(&broken),
            "reply:$target:example.org",
            false,
        );
        assert_eq!(restored.text.body, plain_column, "the plain column is what the user sees");
        assert!(!restored.discarded_rich_intent && restored.read_only.is_none());
        assert_eq!(restored.blocked(), Some(QuarantineReason::Malformed));
        assert!(restored.envelope.pending().is_empty());
        let carried_row = restored.envelope.quarantine().unwrap();
        assert_eq!(carried_row.original_html().as_deref(), Some(broken.as_str()));
        assert_eq!(carried_row.original_plain().as_deref(), Some(plain_column.as_str()));
        assert_eq!(carried_row.original_kind(), "reply:$target:example.org");
        assert!(restored.blocked_revision().is_some(), "an unreadable original has an identity");
        // Identity is opaque and per restored row: the same bytes restored
        // again are a new review, never an equal identity derived from text.
        let twin = ComposeText::restore_stored_draft(
            plain_column.clone(),
            Some(&broken),
            "reply:$target:example.org",
            false,
        );
        assert_ne!(twin.blocked_revision(), restored.blocked_revision());
        let restored = ComposeText::restore_draft(value.body.clone(), Some(&broken));
        assert_eq!(restored.text, value, "a canonical span still yields the typed text");
        assert_eq!(
            restored.envelope.quarantine().unwrap().original_html().as_deref(),
            Some(broken.as_str())
        );
        // An edit saved afterwards carries the quarantine; a restart still
        // sees it, and it is never normalized into an ordinary envelope.
        let edited = ComposeText::plain("edited later");
        let mut carried_envelope = restored.envelope.clone();
        carried_envelope.set_generation(Generation::fresh());
        let carried = edited.encode_draft(&carried_envelope).unwrap();
        let again = ComposeText::restore_draft(edited.body.clone(), Some(&carried));
        assert_eq!(again.text, edited);
        assert_eq!(again.blocked(), Some(QuarantineReason::Malformed));
        assert_eq!(
            again.envelope.quarantine().unwrap().original_html().as_deref(),
            Some(broken.as_str())
        );
        let mut discarded = again.envelope.clone();
        assert!(discarded.clear_quarantine());
        assert!(discarded.is_empty());
        // Other recognizable but unreadable shapes: future version, version 2
        // without its attribute, version 1 with an extra attribute, non-hex,
        // non-canonical and oversized metadata, and a foreign element that
        // carries our marker. None falls back to an ordinary draft.
        let oversized_meta = "00".repeat(crate::draft_recovery::MAX_ENVELOPE_HEX_BYTES / 2 + 1);
        let noncanonical = {
            let mut json = meta.as_bytes().chunks(2).map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
            }).collect::<Vec<u8>>();
            json.push(b' ');
            json.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
        };
        let cases: Vec<(String, QuarantineReason)> = vec![
            (
                html.replace("data-morpheus-compose=\"2\"", "data-morpheus-compose=\"3\""),
                QuarantineReason::UnsupportedVersion,
            ),
            (
                html.replace(&format!(" data-morpheus-meta=\"{meta}\""), ""),
                QuarantineReason::Malformed,
            ),
            (
                html.replace("data-morpheus-compose=\"2\"", "data-morpheus-compose=\"1\""),
                QuarantineReason::Malformed,
            ),
            (
                value
                    .draft_html()
                    .unwrap()
                    .replace("data-format=", "data-morpheus-extra=\"1\" data-format="),
                QuarantineReason::Malformed,
            ),
            (html.replace(&meta, "zz"), QuarantineReason::Malformed),
            (html.replace(&meta, &noncanonical), QuarantineReason::Malformed),
            (
                format!("<span data-morpheus-compose=\"2\" data-format=\"plain\" data-morpheus-meta=\"{oversized_meta}\">{}</span>", value.body),
                QuarantineReason::TooLarge,
            ),
            (
                format!("<b data-morpheus-compose=\"2\">{}</b>", value.body),
                QuarantineReason::Malformed,
            ),
        ];
        for (bad, reason) in cases {
            let restored = ComposeText::restore_draft(value.body.clone(), Some(&bad));
            assert_eq!(restored.blocked(), Some(reason), "{}", &bad[..bad.len().min(60)]);
            assert!(!restored.discarded_rich_intent);
            assert_eq!(restored.text.body, value.body, "the saved body is never lost");
            if restored.read_only.is_none() {
                assert_eq!(
                    restored.envelope.quarantine().unwrap().original_html().as_deref(),
                    Some(bad.as_str()),
                    "the complete original row is carried"
                );
            }
        }
        // A row too large to parse or carry stays read-only; its body text is
        // still shown and nothing about it is dropped or rewritten.
        let huge = format!(
            "<span data-morpheus-compose=\"2\" data-format=\"plain\" data-morpheus-meta=\"{}\">x</span>",
            "0".repeat(MAX_DRAFT_HTML_BYTES)
        );
        let restored = ComposeText::restore_draft("x".into(), Some(&huge));
        assert_eq!(restored.read_only.map(|row| row.reason), Some(QuarantineReason::TooLarge));
        assert_eq!(restored.blocked(), Some(QuarantineReason::TooLarge));
        assert!(restored.blocked_revision().is_some(), "a read-only row has an identity");
        assert!(restored.envelope.is_empty());
        assert_eq!(restored.text.body, "x");
        // A row with attachments cannot be carried completely either.
        let with_attachments = ComposeText::restore_stored_draft(
            value.body.clone(),
            Some(&broken),
            "reply:$target:example.org",
            true,
        );
        assert_eq!(with_attachments.read_only.map(|row| row.reason), Some(QuarantineReason::Malformed));
        assert!(with_attachments.envelope.quarantine().is_none());
        assert!(with_attachments.blocked_revision().is_some());
        // The same size without our marker is just foreign HTML.
        let foreign = format!("<b>{}</b>", "0".repeat(MAX_DRAFT_HTML_BYTES));
        let restored = ComposeText::restore_draft("x".into(), Some(&foreign));
        assert!(restored.discarded_rich_intent && restored.blocked().is_none());
    }

    #[test]
    fn readable_rows_with_unsupported_stored_fields_are_preserved_read_only() {
        use crate::draft_recovery::QuarantineReason;
        let value = selected("send ", " now");
        let html = value.draft_html().unwrap();
        // A plain new message with nothing else stored restores writable.
        let plain = ComposeText::restore_stored_draft(value.body.clone(), Some(&html), "new", false);
        assert_eq!(plain.text, value);
        assert!(plain.blocked().is_none() && plain.blocked_revision().is_none());
        // Kind-only difference: a readable reply or edit draft shows its text
        // but is never rewritten as a new message.
        for kind in ["reply:$target:example.org", "edit:$target:example.org"] {
            let restored = ComposeText::restore_stored_draft(value.body.clone(), Some(&html), kind, false);
            assert_eq!(restored.text, value, "the typed text and mention intent still restore");
            assert!(!restored.discarded_rich_intent);
            assert_eq!(restored.blocked(), Some(QuarantineReason::UnsupportedFields));
            assert_eq!(restored.read_only.map(|row| row.reason), Some(QuarantineReason::UnsupportedFields));
            assert!(restored.envelope.quarantine().is_none(), "nothing is carried; the row stays in place");
            assert!(restored.blocked_revision().is_some());
        }
        // Attachment-only difference: identical text and kind, attachments
        // present. Preserved read-only, with its own identity each read.
        let with_attachments = ComposeText::restore_stored_draft(value.body.clone(), Some(&html), "new", true);
        assert_eq!(with_attachments.text, value);
        assert_eq!(with_attachments.blocked(), Some(QuarantineReason::UnsupportedFields));
        let again = ComposeText::restore_stored_draft(value.body.clone(), Some(&html), "new", true);
        assert_ne!(again.blocked_revision(), with_attachments.blocked_revision());
        // No HTML at all (another client's plain draft) with attachments: the
        // plain body shows, the row is preserved.
        let no_html = ComposeText::restore_stored_draft("just text".into(), None, "new", true);
        assert_eq!(no_html.text, ComposeText::plain("just text"));
        assert_eq!(no_html.blocked(), Some(QuarantineReason::UnsupportedFields));
        assert!(!no_html.discarded_rich_intent);
        let no_html_plain = ComposeText::restore_stored_draft("just text".into(), None, "new", false);
        assert!(no_html_plain.blocked().is_none());
        // Foreign HTML with a reply kind: the body falls back as before, and
        // the row is still preserved rather than rewritten.
        let foreign = ComposeText::restore_stored_draft("x".into(), Some("<b>x</b>"), "reply:$t:example.org", false);
        assert!(foreign.discarded_rich_intent);
        assert_eq!(foreign.blocked(), Some(QuarantineReason::UnsupportedFields));
        // An unreadable row keeps its own reason; unsupported fields never
        // mask it.
        let broken = html.replace("data-format=\"", "data-format=\"x");
        let unreadable = ComposeText::restore_stored_draft(value.body.clone(), Some(&broken), "reply:$t:example.org", true);
        assert_eq!(unreadable.blocked(), Some(QuarantineReason::Malformed));
    }

    #[test]
    fn markdown_excludes_code_links_images_escaped_and_raw_html() {
        for body in [
            "`@casey:example.org`",
            "```\n@casey:example.org\n```",
            "[@casey:example.org](https://example.org)",
            "![a](https://example.org/@casey:example.org)",
            "\\@casey:example.org",
            "<div>@casey:example.org</div>",
        ] {
            let start = body.find("@casey").unwrap();
            assert!(
                !ComposeText::new(body, TextFormat::Markdown)
                    .allows_mention_range(start..start + "@casey:example.org".len())
            );
        }
        let body = "**hello** @casey:example.org";
        assert!(ComposeText::new(body, TextFormat::Markdown).allows_mention_range(10..body.len()));
    }
    #[test]
    fn duplicate_self_and_room_metadata_are_explicit() {
        let mut value = selected("", " @casey:example.org @room");
        let second = value.body.rfind("@casey").unwrap();
        value.mentions.push(MentionSpan {
            range: second..second + "@casey:example.org".len(),
            target: MentionTarget::User("@casey:example.org".into()),
        });
        let room = value.body.rfind("@room").unwrap();
        value.mentions.push(MentionSpan {
            range: room..room + 5,
            target: MentionTarget::Room,
        });
        let metadata = value
            .matrix_mentions(&UserId::parse("@casey:example.org").unwrap())
            .unwrap();
        assert!(metadata.user_ids.is_empty());
        assert!(metadata.room);
        assert_eq!(
            ComposeText::restore_draft(value.body.clone(), Some(&value.draft_html().unwrap())).text,
            value
        );
    }
    #[test]
    fn invalid_unicode_ranges_overlaps_and_overflow_counts_rejected() {
        let mut value = selected("👋 ", "");
        value.mentions[0].range.start = 1;
        assert!(value.validate().is_err());
        let mut value = selected("", "");
        value.mentions.push(value.mentions[0].clone());
        assert!(value.validate().is_err());
        assert_eq!(
            ComposeText::plain("x".repeat(MAX_COMPOSE_BYTES + 1)).validate(),
            Err(ComposeError::TooLarge)
        );
    }
    #[test]
    fn debug_does_not_emit_body_or_user_ids() {
        let value = selected("private message ", "");
        let debug = format!("{value:?} {:?}", value.mentions);
        assert!(!debug.contains("casey"));
        assert!(!debug.contains("private message"));
    }
    #[test]
    fn identical_occurrence_deletion_cannot_transfer_selected_intent() {
        let value = selected("", " @casey:example.org");
        let next = value.rebase_body("@casey:example.org".to_owned());
        assert!(next.mentions.is_empty());
        let value = selected("@casey:example.org ", "");
        assert!(
            value
                .rebase_body("@casey:example.org".to_owned())
                .mentions
                .is_empty()
        );
    }

    #[test]
    fn adjacent_identifier_edits_remove_intent_but_delimiters_preserve_it() {
        let value = selected("", "");
        for suffix in ["x", ".", ":42", "_x"] {
            let next = value.rebase_body(format!("{}{suffix}", value.body));
            assert!(next.mentions.is_empty());
        }
        for suffix in [",", ")", "!", " 👋", "🦋x"] {
            assert_eq!(
                value
                    .rebase_body(format!("{}{suffix}", value.body))
                    .mentions
                    .len(),
                1
            );
        }
        assert!(
            value
                .rebase_body(format!("x{}", value.body))
                .mentions
                .is_empty()
        );
    }
    #[test]
    fn literal_splice_preserves_unaffected_intent_and_replaces_selected_span() {
        let (source, _) = ComposeText::plain("Ω @ca tail")
            .insert_mention(3..6, MentionTarget::User("@casey:example.org".into()))
            .unwrap();
        let (shifted, caret) = source.insert_literal(0..2, "👋").unwrap();
        assert_eq!(caret, 4);
        assert_eq!(shifted.body, "👋 @casey:example.org tail");
        assert_eq!(shifted.mentions[0].range, 5..23);
        let (replaced, _) = shifted.insert_literal(5..23, "🎉").unwrap();
        assert!(replaced.mentions.is_empty());
        assert_eq!(replaced.body, "👋 🎉 tail");
    }

    #[test]
    fn literal_splice_never_confers_intent_and_rejects_invalid_offsets_or_size() {
        let source = ComposeText::plain("Ω ");
        assert!(source.insert_literal(1..1, "👋").is_err());
        assert!(source.insert_literal(3..2, "👋").is_err());
        assert!(
            source
                .insert_literal(3..3, &"x".repeat(MAX_COMPOSE_BYTES))
                .is_err()
        );
        let (text, _) = source.insert_literal(3..3, "@room 👋").unwrap();
        assert!(text.mentions.is_empty());
        assert_eq!(source.body, "Ω ");
    }
}

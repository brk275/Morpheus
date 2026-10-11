//! Outbound Matrix text content. Markdown is opt-in and its HTML is reduced to
//! the Matrix specification's safe subset before it can reach the send queue.

use crate::compose::{ComposeError, ComposeText, escape_html, markdown_options};
use matrix_sdk::ruma::events::room::message::{
    MessageType, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::{UserId, events::Mentions};
use pulldown_cmark::{Event, Options, Parser};
use ruma::html::{HtmlSanitizerMode, RemoveReplyFallback};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TextFormat {
    #[default]
    Plain,
    Markdown,
}

pub(crate) fn room_text(body: String, format: TextFormat) -> RoomMessageEventContent {
    let mut content = match format {
        TextFormat::Plain => RoomMessageEventContent::text_plain(body),
        TextFormat::Markdown => {
            let mut content = RoomMessageEventContent::text_markdown(escape_raw_html(&body));
            if let MessageType::Text(text) = &mut content.msgtype {
                text.body = body;
            }
            content
        }
    };
    sanitize(&mut content.msgtype);
    content.add_mentions(Mentions::new())
}

pub(crate) fn related_text(
    body: String,
    format: TextFormat,
) -> RoomMessageEventContentWithoutRelation {
    let mut content = match format {
        TextFormat::Plain => RoomMessageEventContentWithoutRelation::text_plain(body),
        TextFormat::Markdown => {
            let mut content =
                RoomMessageEventContentWithoutRelation::text_markdown(escape_raw_html(&body));
            if let MessageType::Text(text) = &mut content.msgtype {
                text.body = body;
            }
            content
        }
    };
    sanitize(&mut content.msgtype);
    content.add_mentions(Mentions::new())
}

/// Typed entry point: selections are checked before any relation helper runs.
pub(crate) fn compose_room(
    text: &ComposeText,
    own_user: &UserId,
) -> Result<RoomMessageEventContent, ComposeError> {
    text.validate()?;
    let mut content = match text.format {
        TextFormat::Plain if text.mentions.is_empty() => {
            RoomMessageEventContent::text_plain(&text.body)
        }
        TextFormat::Plain => {
            let html = insert_mentions(text, false)?.replace("&#10;", "<br>");
            RoomMessageEventContent::text_html(&text.body, html)
        }
        TextFormat::Markdown => {
            let mut content = RoomMessageEventContent::text_markdown(insert_mentions(text, true)?);
            if let MessageType::Text(message) = &mut content.msgtype {
                message.body.clone_from(&text.body);
            }
            content
        }
    };
    sanitize(&mut content.msgtype);
    Ok(content.add_mentions(text.matrix_mentions(own_user)?))
}

pub(crate) fn compose_related(
    text: &ComposeText,
    own_user: &UserId,
) -> Result<RoomMessageEventContentWithoutRelation, ComposeError> {
    Ok(compose_room(text, own_user)?.into())
}

/// Escape original raw HTML before injecting our own anchors. Source operations
/// are disjoint and retain the original parser context, including fenced code.
fn insert_mentions(text: &ComposeText, markdown: bool) -> Result<String, ComposeError> {
    let mut replacements = Vec::new();
    if markdown {
        for (event, range) in Parser::new_ext(&text.body, markdown_options()).into_offset_iter() {
            if matches!(event, Event::Html(_) | Event::InlineHtml(_)) {
                replacements.push((range.clone(), escape_html(&text.body[range])));
            }
        }
    }
    for span in &text.mentions {
        replacements.push((span.range.clone(), span.target.anchor()?));
    }
    replacements.sort_by_key(|(range, _)| range.start);
    let mut result = String::new();
    let mut cursor = 0;
    for (range, replacement) in replacements {
        if range.start < cursor {
            return Err(ComposeError::InvalidMention);
        }
        if markdown {
            result.push_str(&text.body[cursor..range.start]);
        } else {
            result.push_str(&escape_html(&text.body[cursor..range.start]));
        }
        result.push_str(&replacement);
        cursor = range.end;
    }
    if markdown {
        result.push_str(&text.body[cursor..]);
    } else {
        result.push_str(&escape_html(&text.body[cursor..]));
    }
    Ok(result)
}

/// Escape raw HTML spans before Ruma's Markdown renderer sees them. Using
/// source offsets avoids changing code spans and fenced code, where `<tag>`
/// is already text and should render without an extra level of escaping.
fn escape_raw_html(source: &str) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut escaped = String::with_capacity(source.len());
    let mut cursor = 0;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        if !matches!(event, Event::Html(_) | Event::InlineHtml(_)) || range.start < cursor {
            continue;
        }
        escaped.push_str(&source[cursor..range.start]);
        for ch in source[range.clone()].chars() {
            match ch {
                '&' => escaped.push_str("&amp;"),
                '<' => escaped.push_str("&lt;"),
                '>' => escaped.push_str("&gt;"),
                _ => escaped.push(ch),
            }
        }
        cursor = range.end;
    }
    escaped.push_str(&source[cursor..]);
    escaped
}

fn sanitize(message_type: &mut MessageType) {
    message_type.sanitize(HtmlSanitizerMode::Strict, RemoveReplyFallback::No);
    // Raw HTML with no permitted content can render as an empty rich message.
    // Keep the visible plain-text body as the sole representation in that case.
    if let MessageType::Text(text) = message_type {
        if text
            .formatted
            .as_ref()
            .is_some_and(|formatted| formatted.body.trim().is_empty())
        {
            text.formatted = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{ComposeText, MentionTarget};
    use matrix_sdk::ruma::UserId;
    use serde_json::to_value;

    use super::{TextFormat, related_text, room_text};

    #[test]
    fn markdown_has_readable_plain_body_and_safe_html() {
        let source = "Hello **team** & <script>alert('x')</script>";
        let content = room_text(source.to_owned(), TextFormat::Markdown);
        let event = to_value(content).unwrap();
        assert_eq!(event["body"], source);
        let html = event["formatted_body"].as_str().unwrap();
        assert!(html.contains("<strong>team</strong>"), "{html}");
        assert!(!html.contains("<script"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(html.contains("&lt;/script&gt;"), "{html}");
        assert_eq!(event["format"], "org.matrix.custom.html");
    }

    #[test]
    fn unsafe_attributes_and_urls_do_not_survive() {
        let source =
            "[safe](https://example.org) [click](javascript:alert(1)) <img src=x onerror=alert(2)>";
        let event = to_value(related_text(source.to_owned(), TextFormat::Markdown)).unwrap();
        assert_eq!(event["body"], source);
        let html = event["formatted_body"].as_str().unwrap();
        assert!(html.contains("href=\"https://example.org\""), "{html}");
        assert!(!html.contains("javascript:"), "{html}");
        assert!(!html.contains("<img"), "{html}");
        // The handler is visible only as escaped text, never an HTML attribute.
        assert!(
            html.contains("&lt;img src=x onerror=alert(2)&gt;"),
            "{html}"
        );
    }

    #[test]
    fn html_inside_code_is_escaped_once() {
        let event = to_value(room_text(
            "`<script>` and **bold**".to_owned(),
            TextFormat::Markdown,
        ))
        .unwrap();
        let html = event["formatted_body"].as_str().unwrap();
        assert!(html.contains("<code>&lt;script&gt;</code>"), "{html}");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
    }

    #[test]
    fn plain_mode_never_adds_html_or_reinterprets_markup() {
        let source = "**literal** <script>alert(1)</script>";
        let event = to_value(room_text(source.to_owned(), TextFormat::Plain)).unwrap();
        assert_eq!(event["body"], source);
        assert!(event.get("formatted_body").is_none());
        assert!(event.get("format").is_none());
    }
    #[test]
    fn typed_plain_mentions_preserve_literal_markup_and_explicit_metadata() {
        let text = ComposeText::plain("**literal** <script> @casey:example.org");
        let start = text.body.find('@').unwrap();
        let (text, _) = text
            .insert_mention(
                start..text.body.len(),
                MentionTarget::User("@casey:example.org".into()),
            )
            .unwrap();
        let own = UserId::parse("@riley:example.org").unwrap();
        let event = to_value(super::compose_room(&text, &own).unwrap()).unwrap();
        assert_eq!(event["body"], text.body);
        assert_eq!(event["m.mentions"]["user_ids"][0], "@casey:example.org");
        let html = event["formatted_body"].as_str().unwrap();
        assert!(html.contains("**literal**"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("https://matrix.to/"));
        assert!(!html.contains("<strong>"));
    }

    #[test]
    fn typed_markdown_preserves_code_escaping_and_selected_anchor() {
        let text = ComposeText::new(
            "`<tag>` **hi** @casey:example.org <script>x</script>",
            TextFormat::Markdown,
        );
        let start = text.body.find('@').unwrap();
        let (text, _) = text
            .insert_mention(
                start..start + "@casey:example.org".len(),
                MentionTarget::User("@casey:example.org".into()),
            )
            .unwrap();
        let own = UserId::parse("@riley:example.org").unwrap();
        let event = to_value(super::compose_related(&text, &own).unwrap()).unwrap();
        let html = event["formatted_body"].as_str().unwrap();
        assert!(html.contains("<code>&lt;tag&gt;</code>"));
        assert!(html.contains("<strong>hi</strong>"));
        assert!(html.contains("href=\"https://matrix.to/"));
        assert!(!html.contains("<script>"));
    }

    #[test]
    fn pasted_ids_and_empty_metadata_do_not_use_legacy_notification_rules() {
        let own = UserId::parse("@riley:example.org").unwrap();
        let event = to_value(
            super::compose_room(&ComposeText::plain("@casey:example.org @room"), &own).unwrap(),
        )
        .unwrap();
        assert_eq!(event["m.mentions"], serde_json::json!({}));
        assert!(event.get("formatted_body").is_none());
    }
    #[test]
    fn markdown_significant_user_id_is_literal_inside_generated_anchor() {
        let own = UserId::parse("@riley:example.org").unwrap();
        for id in ["@_casey_:example.org", "@*casey*:example.org"] {
            let text = ComposeText::new(id, TextFormat::Markdown);
            let (selected, _) = text
                .insert_mention(0..id.len(), MentionTarget::User(id.into()))
                .unwrap();
            let event = to_value(super::compose_room(&selected, &own).unwrap()).unwrap();
            let html = event["formatted_body"].as_str().unwrap();
            assert!(!html.contains("<em>"));
            assert!(!html.contains("<strong>"));
            let parsed = ruma::html::Html::parse(html);
            fn text(node: &ruma::html::NodeRef) -> String {
                if let Some(value) = node.as_text() {
                    return value.borrow().to_string();
                }
                node.children().map(|child| text(&child)).collect()
            }
            assert_eq!(
                parsed
                    .children()
                    .map(|node| text(&node))
                    .collect::<String>()
                    .trim(),
                id
            );
        }
    }
}

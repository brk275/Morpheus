//! Outbound Matrix text content. Markdown is opt-in and its HTML is reduced to
//! the Matrix specification's safe subset before it can reach the send queue.

use matrix_sdk::ruma::events::room::message::{
    MessageType, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
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
    content
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
    content
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
}

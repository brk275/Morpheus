//! Bounded Matrix HTML projection for Slint's CommonMark subset.
//!
//! The SDK sanitizes incoming HTML before it reaches `MessageInfo`. This
//! conversion intentionally accepts less than Matrix permits: if a tag cannot
//! be represented faithfully, callers display the event's plain `body`.

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use ruma::html::{Html, NodeData, NodeRef};
use url::Url;

const MAX_HTML_BYTES: usize = 16 * 1024;
const MAX_MARKDOWN_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_LINK_BYTES: usize = 2048;

pub(crate) fn matrix_html_to_markdown(source: &str) -> Option<String> {
    if source.is_empty() || source.len() > MAX_HTML_BYTES || source.contains('\0') {
        return None;
    }
    let html = Html::parse(source);
    let mut blocks = Vec::new();
    let mut inline = String::new();
    for node in html.children() {
        match node.as_element().map(|element| element.name.local.as_ref()) {
            Some("p") => {
                flush_inline(&mut inline, &mut blocks);
                no_attributes(&node)?;
                let mut paragraph = String::new();
                render_inline_children(&node, &mut paragraph, 0)?;
                if !paragraph.trim().is_empty() {
                    blocks.push(paragraph);
                }
            }
            Some("ul" | "ol") => {
                flush_inline(&mut inline, &mut blocks);
                blocks.push(render_list(&node)?);
            }
            _ => render_inline(&node, &mut inline, 0)?,
        }
    }
    flush_inline(&mut inline, &mut blocks);
    let markdown = blocks.join("\n\n");
    if markdown.trim().is_empty() || markdown.len() > MAX_MARKDOWN_BYTES {
        None
    } else {
        Some(markdown)
    }
}

/// The accessibility label must describe the accepted rich presentation, not
/// an event's independently supplied Matrix plain-text fallback. A sender can
/// make those two bodies disagree.
pub(crate) fn markdown_to_accessible_text(markdown: &str) -> String {
    let mut plain = String::new();
    for event in Parser::new(markdown) {
        match event {
            Event::Text(text) | Event::Code(text) => plain.push_str(&text),
            Event::SoftBreak | Event::HardBreak => plain.push('\n'),
            Event::Start(Tag::Item) => {
                if !plain.is_empty() && !plain.ends_with('\n') {
                    plain.push('\n');
                }
                plain.push_str("• ");
            }
            Event::End(TagEnd::Paragraph | TagEnd::Item) => {
                if !plain.ends_with('\n') {
                    plain.push('\n');
                }
            }
            _ => {}
        }
        if plain.len() > MAX_MARKDOWN_BYTES {
            return String::new();
        }
    }
    plain.trim().to_owned()
}

fn flush_inline(inline: &mut String, blocks: &mut Vec<String>) {
    if !inline.trim().is_empty() {
        blocks.push(std::mem::take(inline));
    } else {
        inline.clear();
    }
}

fn render_list(node: &NodeRef) -> Option<String> {
    no_attributes(node)?;
    let ordered = node.as_element()?.name.local.as_ref() == "ol";
    let mut lines = Vec::new();
    for child in node.children() {
        match child.data() {
            NodeData::Text(text) if text.borrow().trim().is_empty() => continue,
            NodeData::Element(element) if element.name.local.as_ref() == "li" => {
                no_attributes(&child)?;
                let mut item = String::new();
                for part in child.children() {
                    if part
                        .as_element()
                        .is_some_and(|element| element.name.local.as_ref() == "p")
                    {
                        no_attributes(&part)?;
                        render_inline_children(&part, &mut item, 1)?;
                    } else {
                        render_inline(&part, &mut item, 1)?;
                    }
                }
                if item.trim().is_empty() || item.contains('\n') {
                    return None;
                }
                lines.push(if ordered {
                    format!("{}. {item}", lines.len() + 1)
                } else {
                    format!("- {item}")
                });
            }
            _ => return None,
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn render_inline_children(node: &NodeRef, output: &mut String, depth: usize) -> Option<()> {
    for child in node.children() {
        render_inline(&child, output, depth + 1)?;
    }
    Some(())
}

fn render_inline(node: &NodeRef, output: &mut String, depth: usize) -> Option<()> {
    if depth > MAX_DEPTH || output.len() > MAX_MARKDOWN_BYTES {
        return None;
    }
    match node.data() {
        NodeData::Text(text) => {
            append_escaped_text(output, &text.borrow());
            Some(())
        }
        NodeData::Element(element) => {
            let tag = element.name.local.as_ref();
            match tag {
                "strong" | "b" => wrap(node, output, depth, "**"),
                "em" | "i" => wrap(node, output, depth, "*"),
                "span" => {
                    // Sanitized room-mention markers are ordinary spans. An
                    // attribute-free span has no presentation of its own;
                    // preserve its contents without interpreting mention intent.
                    // Spoilers, colors and other attributes still fall back.
                    no_attributes(node)?;
                    render_inline_children(node, output, depth)
                }
                "code" => {
                    no_attributes(node)?;
                    let mut code = String::new();
                    for child in node.children() {
                        code.push_str(&child.as_text()?.borrow());
                    }
                    if code.is_empty()
                        || code.contains('`')
                        || code.contains(['\n', '\r'])
                        || code.starts_with(' ')
                        || code.ends_with(' ')
                    {
                        return None;
                    }
                    output.push('`');
                    output.push_str(&code);
                    output.push('`');
                    Some(())
                }
                "a" => {
                    let attributes = element.attrs.borrow();
                    if attributes.len() != 1 {
                        return None;
                    }
                    let href = attributes.iter().next()?;
                    if href.name.local.as_ref() != "href" {
                        return None;
                    }
                    let link = safe_link(&href.value)?;
                    drop(attributes);
                    let mut label = String::new();
                    render_inline_children(node, &mut label, depth)?;
                    if label.trim().is_empty() || label.contains('\n') {
                        return None;
                    }
                    output.push('[');
                    output.push_str(&label);
                    output.push_str("](");
                    output.push_str(&link);
                    output.push(')');
                    Some(())
                }
                "br" => {
                    no_attributes(node)?;
                    if node.has_children() {
                        return None;
                    }
                    output.push_str("  \n");
                    Some(())
                }
                _ => None,
            }
        }
        NodeData::Document | NodeData::Other => None,
    }
}

fn wrap(node: &NodeRef, output: &mut String, depth: usize, delimiter: &str) -> Option<()> {
    no_attributes(node)?;
    let mut inner = String::new();
    render_inline_children(node, &mut inner, depth)?;
    if inner.is_empty()
        || inner.chars().next().is_some_and(char::is_whitespace)
        || inner.chars().last().is_some_and(char::is_whitespace)
    {
        return None;
    }
    output.push_str(delimiter);
    output.push_str(&inner);
    output.push_str(delimiter);
    Some(())
}

fn no_attributes(node: &NodeRef) -> Option<()> {
    if node.as_element()?.attrs.borrow().is_empty() {
        Some(())
    } else {
        None
    }
}

fn safe_link(source: &str) -> Option<String> {
    if source.len() > MAX_LINK_BYTES || source.chars().any(char::is_control) {
        return None;
    }
    let parsed = Url::parse(source).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    // Parentheses in a Markdown destination are structural. Encoding them
    // keeps the URL target intact without letting it change the markup tree.
    Some(parsed.as_str().replace('(', "%28").replace(')', "%29"))
}

fn append_escaped_text(output: &mut String, source: &str) {
    let mut previous_was_space = false;
    for ch in source.chars() {
        if ch.is_whitespace() {
            if !previous_was_space {
                output.push(' ');
            }
            previous_was_space = true;
            continue;
        }
        previous_was_space = false;
        // CommonMark backslash escapes ASCII punctuation. Text from HTML is
        // data, never markup, even when it contains literal Markdown tokens.
        if ch.is_ascii_punctuation() {
            output.push('\\');
        }
        output.push(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::{markdown_to_accessible_text, matrix_html_to_markdown};

    #[test]
    fn keeps_supported_styles_and_decodes_entities() {
        let markdown = matrix_html_to_markdown(
            "<p>Hello <strong>team</strong> &amp; <em>friends</em>; <code>a&lt;b</code>.</p>",
        )
        .unwrap();
        assert!(markdown.contains("**team**"), "{markdown}");
        assert!(markdown.contains("*friends*"), "{markdown}");
        assert!(markdown.contains("`a<b`"), "{markdown}");
        assert!(markdown.contains("\\&"), "{markdown}");
    }

    #[test]
    fn preserves_literal_markdown_and_rejects_unsupported_html() {
        let markdown =
            matrix_html_to_markdown("<p>**literal** &lt;script&gt; <b>real</b></p>").unwrap();
        assert!(markdown.contains("\\*\\*literal\\*\\*"), "{markdown}");
        assert!(markdown.contains("\\<script\\>"), "{markdown}");
        assert!(markdown.contains("**real**"), "{markdown}");
        assert!(matrix_html_to_markdown("<img src=\"https://example.org/a.png\">").is_none());
        assert!(matrix_html_to_markdown("<pre><code>block</code></pre>").is_none());
    }

    #[test]
    fn accepts_only_safe_links_and_simple_lists() {
        let markdown = matrix_html_to_markdown(
            "<a href=\"https://example.org/a(b)\">go</a><ul><li>one</li><li>two</li></ul>",
        )
        .unwrap();
        assert!(
            markdown.contains("[go](https://example.org/a%28b%29)"),
            "{markdown}"
        );
        assert!(markdown.contains("- one\n- two"), "{markdown}");
        assert!(matrix_html_to_markdown("<a href=\"javascript:alert(1)\">go</a>").is_none());
        assert!(
            matrix_html_to_markdown("<a href=\"https://x.test\" onclick=\"x\">go</a>").is_none()
        );
        assert!(matrix_html_to_markdown("<a href=\"https://user:pass@x.test\">go</a>").is_none());
    }

    #[test]
    fn transparent_spans_keep_styles_but_semantic_attributes_still_fall_back() {
        let markdown = matrix_html_to_markdown(
            "<p><span>Literal **text** <span><strong>bold</strong></span> @room</span></p>",
        )
        .expect("Attribute-free spans are transparent containers");
        assert!(markdown.contains("**bold**"));
        assert_eq!(
            markdown_to_accessible_text(&markdown),
            "Literal **text** bold @room"
        );
        for attribute in [
            "data-mx-spoiler=\"reason\"",
            "data-mx-color=\"#00ff00\"",
            "class=\"custom\"",
            "onclick=\"action()\"",
            "data-morpheus-room=\"1\"",
        ] {
            let html = format!("<span {attribute}>content</span>");
            assert!(matrix_html_to_markdown(&html).is_none());
        }
        let nested = format!("{}text{}", "<span>".repeat(34), "</span>".repeat(34));
        assert!(matrix_html_to_markdown(&nested).is_none());
    }

    #[test]
    fn selected_room_mention_keeps_bold_in_the_sanitized_compose_projection() {
        use crate::{ComposeText, MentionTarget, TextFormat, formatting::compose_room};
        use matrix_sdk::ruma::{UserId, events::room::message::MessageType};

        let source = "Hello **main-projection** <script>alert(1)</script> @casey:example.org @room";
        let mut text = ComposeText::new(source, TextFormat::Markdown);
        for target in [
            MentionTarget::User("@casey:example.org".to_owned()),
            MentionTarget::Room,
        ] {
            let start = text.body.find(target.text()).unwrap();
            let end = start + target.text().len();
            text = text.insert_mention(start..end, target).unwrap().0;
        }
        let own = UserId::parse("@writer:example.org").unwrap();
        let content = compose_room(&text, &own).unwrap();
        let MessageType::Text(message) = content.msgtype else {
            panic!("Expected a text message");
        };
        let html = message.formatted.unwrap().body;
        assert!(html.contains("<span>@room</span>"));
        assert!(!html.contains("data-morpheus-room"));
        assert!(!html.contains("<script>"));
        let markdown = matrix_html_to_markdown(&html)
            .expect("Selected room mentions must not discard supported formatting");
        assert!(markdown.replace("\\-", "-").contains("**main-projection**"));
        assert_eq!(
            markdown_to_accessible_text(&markdown),
            "Hello main-projection <script>alert(1)</script> @casey:example.org @room"
        );
    }

    #[test]
    fn bounds_input_and_depth() {
        assert!(matrix_html_to_markdown(&"a".repeat(16 * 1024 + 1)).is_none());
        let nested = format!("{}text{}", "<strong>".repeat(34), "</strong>".repeat(34));
        assert!(matrix_html_to_markdown(&nested).is_none());
    }

    #[test]
    fn accessibility_text_matches_accepted_rich_content() {
        let markdown = matrix_html_to_markdown(
            "<p>Visible <strong>answer</strong> <a href=\"https://example.org\">here</a></p>",
        )
        .unwrap();
        assert_eq!(
            markdown_to_accessible_text(&markdown),
            "Visible answer here"
        );
    }
}

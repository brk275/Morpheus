# Outbound text formatting

The main room composer has an **Md** button. It is off by default. Turn it on
to send Markdown such as `**bold**`, `_emphasis_`, code spans, links, and lists;
the selected button shows when formatting is on. The same choice applies to a
reply or edit. Editing a supported formatted message opens with **Md** on and
a Markdown source that keeps its styling when saved unchanged. Sending with
**Md** off sends literal plain text. The thread composer still sends plain text.

Markdown conversion uses Ruma's Matrix message constructor. Before queueing,
Morpheus makes raw HTML tags literal text before rendering, then sanitizes
the generated HTML in strict Matrix mode. Unsafe URLs and attributes cannot
become active content in the formatted body. The original source remains in
Matrix `body` as a readable fallback for clients that do not render formatted
messages.

## Native timeline rendering

Morpheus reads the SDK's sanitized `org.matrix.custom.html` from text, notice,
and emote timeline events. It projects a bounded subset into Slint's native
`StyledText`: paragraphs, line breaks, bold, emphasis, inline code, simple
ordered or unordered lists, and HTTP(S) links. Links are checked again, and the
destination is shown for confirmation before opening the system browser. Plain
Matrix events always remain literal text.
Replies and edits use the same projection after the SDK resolves them, including
the thread reply panel.

The renderer accepts at most 16 KiB of formatted HTML per event and 32 levels
of nesting. Unsupported tags, attributes, malformed rich text that Slint
cannot parse, and non-browser link schemes fall back to the original plain
Matrix `body`. It never passes event HTML into Slint or a WebView. The
accessibility label follows the accepted rich text; plain events retain the
original body. Replies use the plain body, while edits of supported formatted
messages use their safe Markdown projection. Code blocks, tables, color spans,
custom emoji, spoilers, and Matrix
URI links are not rich-rendered yet. Rendered layout and link activation still
need live screen-reader and cross-platform acceptance.

Room drafts remain account and room scoped in the encrypted Matrix SDK store.
The source text survives account switches and app restarts, including a failed
queued send. The **Md** choice is scoped to each account and room for the
current app session; after a restart, formatting returns to the safe plain-text
default while the draft source remains available.

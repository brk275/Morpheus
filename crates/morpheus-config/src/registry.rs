//! The one registry of editable appearance tokens and Settings search targets.
//!
//! The TOML validator, the in-app color editor, the Settings search index, the
//! offline schema output and the documentation tables all read these tables.
//! A token is listed here only when the interface actually consumes it, so an
//! accepted key is never an inactive setting. Planned tokens are documented as
//! gaps instead of being accepted early.

/// A group of color tokens in the Appearance workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenGroup {
    Surfaces,
    Text,
    Accents,
    Controls,
    Navigation,
    Calls,
    Avatars,
}

impl TokenGroup {
    pub const ALL: [Self; 7] = [
        Self::Surfaces,
        Self::Text,
        Self::Accents,
        Self::Controls,
        Self::Navigation,
        Self::Calls,
        Self::Avatars,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Surfaces => "surfaces",
            Self::Text => "text",
            Self::Accents => "accents",
            Self::Controls => "controls",
            Self::Navigation => "navigation",
            Self::Calls => "calls",
            Self::Avatars => "avatars",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Surfaces => "Surfaces",
            Self::Text => "Text and messages",
            Self::Accents => "Accents and states",
            Self::Controls => "Inputs and controls",
            Self::Navigation => "Navigation and badges",
            Self::Calls => "Calls",
            Self::Avatars => "Avatars",
        }
    }
}

/// Where a theme pack supplies the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// One of the eleven required `[colors]` of a theme pack.
    Base,
    /// An optional `[roles]` entry of a theme pack.
    Role,
    /// An optional `[states]` entry of a theme pack: an interaction state
    /// that follows an existing color until a theme or `config.toml` sets it.
    State,
}

/// What a token shows when neither `config.toml` nor the theme pack sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fallback {
    /// The built-in palette of the effective theme mode.
    Palette,
    /// Follows another token.
    Token(&'static str),
    /// Each control keeps its previous built-in color, so the default differs
    /// between controls; `example` is the most common one, as `#RRGGBB(AA)`.
    Legacy { example: &'static str },
}

pub struct ColorToken {
    /// Stable key in `[colors]` of `config.toml` and in a theme pack.
    pub id: &'static str,
    pub group: TokenGroup,
    pub label: &'static str,
    pub description: &'static str,
    /// Extra search words, including alternative spellings.
    pub keywords: &'static [&'static str],
    pub source: TokenSource,
    pub fallback: Fallback,
}

const fn token(
    id: &'static str,
    group: TokenGroup,
    source: TokenSource,
    fallback: Fallback,
    label: &'static str,
    description: &'static str,
    keywords: &'static [&'static str],
) -> ColorToken {
    ColorToken {
        id,
        group,
        label,
        description,
        keywords,
        source,
        fallback,
    }
}

use Fallback::{Legacy, Palette, Token};
use TokenGroup::{Accents, Avatars, Calls, Controls, Navigation, Surfaces, Text};
use TokenSource::{Base, Role, State};

/// Every color token the interface consumes, in editor order.
pub const COLOR_TOKENS: &[ColorToken] = &[
    token(
        "rail",
        Surfaces,
        Base,
        Palette,
        "Space rail",
        "Background of the Space and navigation rail. The window transparency setting applies to it.",
        &["navigation", "spaces", "background", "window"],
    ),
    token(
        "sidebar",
        Surfaces,
        Base,
        Palette,
        "Room list",
        "Background of the room list and the text sidebar. The window transparency setting applies to it.",
        &["rooms", "channels", "navigation", "background", "window"],
    ),
    token(
        "canvas",
        Surfaces,
        Base,
        Palette,
        "Conversation canvas",
        "Main background behind conversations, Settings and dialogs. The window transparency setting applies to the window behind it.",
        &["background", "main", "chat", "window"],
    ),
    token(
        "panel",
        Surfaces,
        Base,
        Palette,
        "Panels",
        "Details panel, cards and secondary panels.",
        &["details", "members", "card", "background"],
    ),
    token(
        "elevated",
        Surfaces,
        Base,
        Palette,
        "Raised surfaces",
        "Inputs, buttons, cards and popups that sit above a panel.",
        &["input", "button", "card", "popup", "field"],
    ),
    token(
        "hover",
        Surfaces,
        Base,
        Palette,
        "Hover",
        "Background under the pointer, and selected controls when no selection color is set.",
        &["pointer", "highlight", "mouse"],
    ),
    token(
        "selection",
        Surfaces,
        Role,
        Token("hover"),
        "Selected item",
        "Selected navigation rows, messages, search results and text selections.",
        &["selected", "active", "highlight"],
    ),
    token(
        "own_surface",
        Surfaces,
        Role,
        Legacy {
            example: "#00000000",
        },
        "Your message background",
        "Background of your own messages when they are neither selected nor hovered. Unset keeps the normal row background.",
        &["own", "mine", "bubble", "message"],
    ),
    token(
        "scrim",
        Surfaces,
        Role,
        Legacy {
            example: "#000000AA",
        },
        "Dialog backdrop",
        "Dims the window behind dialogs. Unset keeps each dialog's built-in dimming.",
        &["modal", "overlay", "dim", "backdrop"],
    ),
    token(
        "shadow",
        Surfaces,
        Role,
        Legacy {
            example: "#00000044",
        },
        "Popup shadow",
        "Shadow under the account menu popup.",
        &["drop shadow", "popup", "menu"],
    ),
    token(
        "media_backdrop",
        Surfaces,
        Role,
        Legacy { example: "#11131B" },
        "Image viewer background",
        "Area around an opened image. The image itself is never recolored.",
        &["image", "viewer", "photo", "preview", "media"],
    ),
    token(
        "message_hover",
        Surfaces,
        State,
        Token("elevated"),
        "Message hover",
        "Background of a timeline message under the pointer. Selection colors still win.",
        &["message", "row", "pointer", "hover", "timeline"],
    ),
    token(
        "ink",
        Text,
        Base,
        Palette,
        "Primary text",
        "Main text and icons.",
        &["text", "font color", "foreground"],
    ),
    token(
        "muted",
        Text,
        Base,
        Palette,
        "Secondary text",
        "Descriptions, hints and inactive icons.",
        &["secondary", "hint", "subtle", "grey", "gray"],
    ),
    token(
        "message",
        Text,
        Role,
        Token("ink"),
        "Message text",
        "Body text of messages.",
        &["body", "chat text"],
    ),
    token(
        "own_message",
        Text,
        Role,
        Token("sender"),
        "Your message text",
        "Name and body text of your own messages. Unset follows Sender names and Message text, then Primary text.",
        &["own", "mine", "self"],
    ),
    token(
        "sender",
        Text,
        Role,
        Token("ink"),
        "Sender names",
        "Sender names while colored sender names are off.",
        &["name", "author", "nickname", "nick"],
    ),
    token(
        "timestamp",
        Text,
        Role,
        Token("muted"),
        "Timestamps",
        "Clock and delivery status in the timeline and thread replies.",
        &["time", "clock", "date"],
    ),
    token(
        "link",
        Text,
        Role,
        Token("accent"),
        "Links",
        "Links in rich messages.",
        &["url", "hyperlink"],
    ),
    token(
        "accent",
        Accents,
        Base,
        Palette,
        "Accent",
        "Highlights, the app mark, selected indicators and primary actions.",
        &["brand", "primary", "highlight color"],
    ),
    token(
        "accent_ink",
        Accents,
        Base,
        Palette,
        "Text on accent",
        "Text and icons drawn on the accent color.",
        &["accent text", "on accent"],
    ),
    token(
        "focus",
        Accents,
        Role,
        Legacy { example: "#AD91FF" },
        "Keyboard focus ring",
        "Outline of the control that has keyboard focus.",
        &["outline", "keyboard", "focus ring", "accessibility"],
    ),
    token(
        "unread",
        Accents,
        Role,
        Token("accent"),
        "Unread badge",
        "Background of unread-count badges.",
        &["badge", "count", "notification"],
    ),
    token(
        "unread_ink",
        Accents,
        Role,
        Token("accent_ink"),
        "Unread badge text",
        "Number on unread-count badges.",
        &["badge text", "count"],
    ),
    token(
        "selection_ink",
        Accents,
        Role,
        Legacy { example: "#FFFFFF" },
        "Text on selected item",
        "Text and icons on selected items and selected input text. Unset keeps each control's text color.",
        &["selected text"],
    ),
    token(
        "error",
        Accents,
        Role,
        Legacy { example: "#E47483" },
        "Error text",
        "Inline error messages.",
        &["warning", "failure", "red"],
    ),
    token(
        "error_surface",
        Accents,
        Role,
        Legacy { example: "#9F3F50" },
        "Error banner",
        "Background of error banners.",
        &["warning", "banner", "alert"],
    ),
    token(
        "error_ink",
        Accents,
        Role,
        Legacy { example: "#FFFFFF" },
        "Error banner text",
        "Text on error banners.",
        &["banner text", "alert"],
    ),
    token(
        "success",
        Accents,
        Role,
        Legacy { example: "#78C8B1" },
        "Success text",
        "Inline confirmations.",
        &["ok", "done", "green"],
    ),
    token(
        "success_surface",
        Accents,
        Role,
        Legacy { example: "#214A46" },
        "Success banner",
        "Background of success and transfer banners.",
        &["banner", "notice"],
    ),
    token(
        "success_ink",
        Accents,
        Role,
        Legacy { example: "#FFFFFF" },
        "Success banner text",
        "Text on success banners.",
        &["banner text", "notice"],
    ),
    token(
        "destructive",
        Accents,
        Role,
        Legacy { example: "#E47483" },
        "Destructive action",
        "Log out text and destructive buttons such as leaving a call.",
        &["danger", "delete", "remove", "leave"],
    ),
    token(
        "destructive_ink",
        Accents,
        Role,
        Legacy { example: "#10131B" },
        "Destructive action text",
        "Text on destructive buttons.",
        &["danger text"],
    ),
    token(
        "presence_online",
        Accents,
        Role,
        Legacy { example: "#64C9A7" },
        "Online status",
        "Dot beside members who are online.",
        &["presence", "status", "online", "member"],
    ),
    token(
        "divider",
        Controls,
        Base,
        Palette,
        "Dividers and borders",
        "Separator lines and control borders.",
        &["border", "line", "separator", "outline"],
    ),
    token(
        "control_hover",
        Controls,
        State,
        Legacy { example: "#32394B" },
        "Button hover",
        "Hover background of text buttons and icon buttons. Unset keeps each button's hover color.",
        &["button", "hover", "pointer", "icon button"],
    ),
    token(
        "control_pressed",
        Controls,
        State,
        Legacy {
            example: "#00000000",
        },
        "Pressed button",
        "Layer drawn while a button or navigation row is pressed. Unset draws no pressed layer.",
        &["pressed", "active", "click", "down", "button"],
    ),
    token(
        "control_disabled",
        Controls,
        State,
        Legacy { example: "#A7AFC1" },
        "Disabled button text",
        "Text and icons of unavailable buttons. Unset keeps each button's disabled look.",
        &["disabled", "unavailable", "inactive", "dimmed", "greyed"],
    ),
    token(
        "input_surface",
        Controls,
        State,
        Legacy { example: "#272D3B" },
        "Text field background",
        "Background of text fields in Settings, discovery, the quick switcher, highlight words and recovery.",
        &["input", "field", "text box", "search box", "background"],
    ),
    token(
        "input_border",
        Controls,
        State,
        Token("divider"),
        "Text field border",
        "Border of those text fields while they do not have keyboard focus.",
        &["input", "field", "outline", "border"],
    ),
    token(
        "composer_surface",
        Controls,
        State,
        Token("elevated"),
        "Composer background",
        "Background of the message and thread composers.",
        &["composer", "message box", "input", "background"],
    ),
    token(
        "composer_border",
        Controls,
        State,
        Token("divider"),
        "Composer border",
        "Border of the message composer.",
        &["composer", "message box", "outline", "border"],
    ),
    token(
        "composer_focus",
        Controls,
        State,
        Token("composer_border"),
        "Composer focus border",
        "Composer border while you type in it. Setting it also outlines the focused thread composer.",
        &["composer", "focus", "typing", "active", "outline"],
    ),
    token(
        "icon",
        Controls,
        State,
        Token("muted"),
        "Icons",
        "Icons of quiet buttons such as message actions, composer tools and room header actions.",
        &["icon", "glyph", "symbol", "toolbar"],
    ),
    token(
        "icon_active",
        Controls,
        State,
        Token("accent"),
        "Active icons",
        "Icons of primary or switched-on buttons, such as Send and open pickers.",
        &["icon", "active", "selected", "send", "toggle"],
    ),
    token(
        "icon_navigation",
        Controls,
        State,
        Token("icon"),
        "Navigation icons",
        "Route icons of the navigation rail and sidebar, such as Home, Explore and Settings. Unset follows Icons.",
        &["icon", "navigation", "rail", "sidebar", "route"],
    ),
    token(
        "icon_header",
        Controls,
        State,
        Token("icon"),
        "Header icons",
        "Conversation header actions such as search, pinned messages, calls and the details panel. Unset follows Icons.",
        &["icon", "header", "toolbar", "room header"],
    ),
    token(
        "icon_composer",
        Controls,
        State,
        Token("icon"),
        "Composer icons",
        "Composer tools such as attach, emoji, stickers and formatting, in the message and thread composers. Unset follows Icons.",
        &["icon", "composer", "attach", "emoji", "formatting"],
    ),
    token(
        "icon_message",
        Controls,
        State,
        Token("icon"),
        "Message action icons",
        "Quick actions on a message, such as reply, edit, react, thread and redact. Unset follows Icons.",
        &["icon", "message", "reply", "react", "quick actions"],
    ),
    token(
        "nav_hover",
        Navigation,
        State,
        Legacy { example: "#272D3B" },
        "Navigation hover",
        "Room, direct message, route and Space rows under the pointer. Unset keeps each row's hover color.",
        &["sidebar", "room list", "rail", "hover", "pointer"],
    ),
    token(
        "nav_selected",
        Navigation,
        State,
        Token("selection"),
        "Selected navigation item",
        "The open room, direct message, route or Space.",
        &[
            "sidebar",
            "room list",
            "rail",
            "selected",
            "active room",
            "current",
        ],
    ),
    token(
        "nav_selected_ink",
        Navigation,
        State,
        Token("selection_ink"),
        "Selected navigation text",
        "Text and icons of the open room, route or Space.",
        &["sidebar", "selected text", "active room"],
    ),
    token(
        "mention",
        Navigation,
        State,
        Token("destructive"),
        "Mention badge",
        "Badge of rooms with unread messages that mention you; plain unread counts keep the unread badge.",
        &[
            "mention",
            "highlight",
            "ping",
            "badge",
            "notification",
            "at",
        ],
    ),
    token(
        "mention_ink",
        Navigation,
        State,
        Token("destructive_ink"),
        "Mention badge text",
        "Number on mention badges.",
        &["mention", "badge text", "highlight count"],
    ),
    token(
        "voice",
        Calls,
        Role,
        Legacy { example: "#78C8B1" },
        "Voice rooms",
        "Voice-room icons, labels and voice-space status.",
        &["call", "audio", "voice space"],
    ),
    token(
        "avatar_ink",
        Avatars,
        Role,
        Legacy { example: "#FFFFFF" },
        "Avatar initials",
        "Initials drawn on avatar backgrounds.",
        &["avatar", "initials", "profile"],
    ),
];

pub fn color_token(id: &str) -> Option<&'static ColorToken> {
    COLOR_TOKENS.iter().find(|token| token.id == id)
}

/// A color in `#RRGGBB` or `#RRGGBBAA` form: hexadecimal digits in either
/// case, alpha last, `00` fully transparent and `FF` opaque. Returns the
/// red, green, blue and alpha bytes.
pub fn parse_hex_color(value: &str) -> Option<[u8; 4]> {
    let digits = value.strip_prefix('#')?;
    if !matches!(digits.len(), 6 | 8) || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |index: usize| u8::from_str_radix(&digits[index..index + 2], 16).ok();
    let alpha = if digits.len() == 8 { byte(6)? } else { u8::MAX };
    Some([byte(0)?, byte(2)?, byte(4)?, alpha])
}

/// The canonical spelling the editor writes: uppercase, and the alpha pair
/// only when the color is not opaque.
pub fn format_hex_color([red, green, blue, alpha]: [u8; 4]) -> String {
    if alpha == u8::MAX {
        format!("#{red:02X}{green:02X}{blue:02X}")
    } else {
        format!("#{red:02X}{green:02X}{blue:02X}{alpha:02X}")
    }
}

/// Window background opacity, in percent of each window surface's own alpha.
pub const WINDOW_OPACITY_RANGE: core::ops::RangeInclusive<u8> = 0..=100;

/// The type and accepted values of one `config.toml` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Bool,
    Integer {
        minimum: i64,
        maximum: i64,
    },
    Enum(&'static [&'static str]),
    /// A nonempty font family name of at most 128 bytes.
    FontFamily,
    /// A theme file name: ASCII letters, digits, `-` and `_`, at most 64.
    ThemeName,
    /// `#RRGGBB` or `#RRGGBBAA`.
    Color,
    /// 1 to 16 colors, each `#RRGGBB` or `#RRGGBBAA`.
    ColorList,
    /// A host-qualified audio device ID.
    DeviceId,
    /// Free text that is stored without validation.
    Text,
    TextList,
    RoomIdList,
    /// A table keyed by documented names; see the field's description.
    Table,
    /// Every workspace pane ID exactly once, or an empty list.
    PaneOrder,
    /// Distinct pane IDs taken from this set.
    PaneSet(&'static [&'static str]),
}

pub struct FieldSpec {
    /// Dotted TOML key. `<token>`, `<action>` and `<account>` stand for keys
    /// named in the description.
    pub key: &'static str,
    pub kind: FieldKind,
    /// The default, as TOML, or `(none)` when the key is normally omitted.
    pub default: &'static str,
    pub description: &'static str,
    /// The setting is stored but nothing in the app uses it yet.
    pub inactive: bool,
    /// Present when the typography and geometry workspace edits the field.
    /// Its range is the field's `Integer` kind, so the validator, the schema
    /// output, the editor and Settings search all read one table.
    pub editor: Option<MetricEditor>,
}

impl FieldSpec {
    /// The accepted range of an integer field.
    pub fn integer_range(&self) -> Option<(i64, i64)> {
        match self.kind {
            FieldKind::Integer { minimum, maximum } => Some((minimum, maximum)),
            _ => None,
        }
    }

    /// The default of an integer field, or `None` when it is normally omitted.
    pub fn integer_default(&self) -> Option<i64> {
        self.default.parse().ok()
    }

    /// Whether the field may be omitted to follow a built-in value.
    pub fn optional(&self) -> bool {
        self.default == "(none)"
    }
}

const fn field(
    key: &'static str,
    kind: FieldKind,
    default: &'static str,
    description: &'static str,
) -> FieldSpec {
    FieldSpec {
        key,
        kind,
        default,
        description,
        inactive: false,
        editor: None,
    }
}

const fn inactive(mut spec: FieldSpec) -> FieldSpec {
    spec.inactive = true;
    spec
}

/// How the editor writes a metric's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Pixels,
    Percent,
    Milliseconds,
    /// A CSS-style font weight number; shown without a suffix.
    Weight,
}

impl Unit {
    /// Shown after the number, including its separating space.
    pub fn suffix(self) -> &'static str {
        match self {
            Self::Pixels => " px",
            Self::Percent => "%",
            Self::Milliseconds => " ms",
            Self::Weight => "",
        }
    }

    /// Spoken after the number by assistive technology.
    pub fn spoken(self) -> &'static str {
        match self {
            Self::Pixels => "logical pixels",
            Self::Percent => "percent",
            Self::Milliseconds => "milliseconds",
            Self::Weight => "font weight",
        }
    }
}

/// A category of the typography and geometry workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricGroup {
    Text,
    Shapes,
    Spacing,
    IconsAndAvatars,
    Motion,
}

impl MetricGroup {
    pub const ALL: [Self; 5] = [
        Self::Text,
        Self::Shapes,
        Self::Spacing,
        Self::IconsAndAvatars,
        Self::Motion,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Shapes => "shapes",
            Self::Spacing => "spacing",
            Self::IconsAndAvatars => "icons",
            Self::Motion => "motion",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text sizes",
            Self::Shapes => "Corners, borders and focus",
            Self::Spacing => "Padding",
            Self::IconsAndAvatars => "Icons and avatars",
            Self::Motion => "Motion",
        }
    }
}

/// Editor metadata of one integer metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricEditor {
    pub group: MetricGroup,
    pub label: &'static str,
    pub unit: Unit,
    pub step: u16,
    /// Extra search words, including alternative spellings.
    pub keywords: &'static [&'static str],
    /// For a field that may be omitted: what omission shows.
    pub inherit: Option<Inherit>,
}

/// What an omitted optional metric draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inherit {
    /// Fixed text shown while the metric is omitted.
    pub text: &'static str,
    /// The value the built-in look is closest to; stepping starts here.
    pub start: u16,
}

const fn inherit(text: &'static str, start: u16) -> Option<Inherit> {
    Some(Inherit { text, start })
}

const fn metric(mut spec: FieldSpec, editor: MetricEditor) -> FieldSpec {
    spec.editor = Some(editor);
    spec
}

const fn editor(
    group: MetricGroup,
    label: &'static str,
    unit: Unit,
    step: u16,
    keywords: &'static [&'static str],
    inherit: Option<Inherit>,
) -> MetricEditor {
    MetricEditor {
        group,
        label,
        unit,
        step,
        keywords,
        inherit,
    }
}

/// The accepted range of one integer field of `config.toml`.
pub fn integer_range(key: &str) -> Option<(i64, i64)> {
    FIELDS
        .iter()
        .find(|field| field.key == key)
        .and_then(FieldSpec::integer_range)
}

/// Every metric the typography and geometry workspace edits, in editor order.
pub fn metric_fields() -> impl Iterator<Item = (&'static FieldSpec, &'static MetricEditor)> {
    FIELDS
        .iter()
        .filter_map(|field| field.editor.as_ref().map(|editor| (field, editor)))
}

/// One editor metric by its TOML key.
pub fn metric_field(key: &str) -> Option<(&'static FieldSpec, &'static MetricEditor)> {
    metric_fields().find(|(field, _)| field.key == key)
}

use FieldKind::{Bool, Integer};
use MetricGroup::{IconsAndAvatars, Motion, Shapes, Spacing, Text as TextSizes};
use Unit::{Milliseconds, Percent, Pixels, Weight};

const THEME_MODES: &[&str] = &["dark", "light", "high_contrast", "system"];
const DENSITIES: &[&str] = &["compact", "comfortable", "spacious"];

/// Every field of `config.toml` schema version 1. A test in this crate
/// (`the_field_reference_agrees_with_the_real_schema`) checks that a fully
/// populated configuration uses exactly these keys, that defaults match
/// `Config::default()`, and that every range and option agrees with
/// validation.
pub const FIELDS: &[FieldSpec] = &[
    field(
        "schema_version",
        Integer {
            minimum: 1,
            maximum: 1,
        },
        "1",
        "Configuration schema version.",
    ),
    field(
        "appearance.theme",
        FieldKind::Enum(THEME_MODES),
        "\"dark\"",
        "Base palette for all accounts.",
    ),
    field(
        "appearance.theme_pack",
        FieldKind::ThemeName,
        "(none)",
        "Theme file in the themes folder beside config.toml, or a built-in theme: nocturne, terminal, paper.",
    ),
    field(
        "appearance.font_family",
        FieldKind::FontFamily,
        "\"Inter\"",
        "Interface font family.",
    ),
    field(
        "appearance.density",
        FieldKind::Enum(DENSITIES),
        "\"comfortable\"",
        "Room and message row density.",
    ),
    field(
        "appearance.scale_percent",
        Integer {
            minimum: 75,
            maximum: 200,
        },
        "100",
        "Text scale in percent.",
    ),
    field(
        "appearance.reduce_motion",
        Bool,
        "false",
        "Immediate hover feedback instead of short fades.",
    ),
    field(
        "appearance.hover_duration_ms",
        Integer {
            minimum: 0,
            maximum: 1000,
        },
        "90",
        "Decorative button and navigation hover fade in milliseconds; reduce_motion overrides it.",
    ),
    metric(
        field(
            "appearance.popup_fade_ms",
            Integer {
                minimum: 0,
                maximum: 1000,
            },
            "0",
            "Decorative fade-in of menus, pickers and the quick switcher in milliseconds; 0 is off and reduce_motion overrides it.",
        ),
        editor(
            Motion,
            "Menu and picker fade",
            Milliseconds,
            10,
            &[
                "animation",
                "fade",
                "popup",
                "menu",
                "picker",
                "duration",
                "transition",
            ],
            None,
        ),
    ),
    metric(
        field(
            "appearance.panel_fade_ms",
            Integer {
                minimum: 0,
                maximum: 1000,
            },
            "0",
            "Decorative fade-in of the details panel in milliseconds; 0 is off and reduce_motion overrides it.",
        ),
        editor(
            Motion,
            "Details panel fade",
            Milliseconds,
            10,
            &[
                "animation",
                "fade",
                "panel",
                "details",
                "members",
                "duration",
                "transition",
            ],
            None,
        ),
    ),
    field(
        "typography.message_font_family",
        FieldKind::FontFamily,
        "(none)",
        "Message font family, and the composers' unless typography.composer_font_family is set; omitted follows appearance.font_family.",
    ),
    field(
        "typography.composer_font_family",
        FieldKind::FontFamily,
        "(none)",
        "Message and thread composer font family; omitted follows the message font family.",
    ),
    field(
        "typography.monospace_font_family",
        FieldKind::FontFamily,
        "\"monospace\"",
        "Timestamp and security-code font family.",
    ),
    field(
        "typography.message_size",
        Integer {
            minimum: 10,
            maximum: 32,
        },
        "14",
        "Message text size in logical pixels before text scaling.",
    ),
    field(
        "typography.sender_weight",
        Integer {
            minimum: 100,
            maximum: 900,
        },
        "(none)",
        "Sender-name font weight; omitted keeps each surface's weight.",
    ),
    field(
        "geometry.corner_radius",
        Integer {
            minimum: 0,
            maximum: 24,
        },
        "10",
        "Base corner radius in logical pixels.",
    ),
    field(
        "geometry.message_gap",
        Integer {
            minimum: 0,
            maximum: 32,
        },
        "12",
        "Base vertical message padding, adjusted by density.",
    ),
    field(
        "geometry.content_padding",
        Integer {
            minimum: 0,
            maximum: 48,
        },
        "24",
        "Conversation inset and composer margin.",
    ),
    field(
        "geometry.rail_width",
        Integer {
            minimum: 44,
            maximum: 100,
        },
        "70",
        "Space rail width.",
    ),
    metric(
        field(
            "typography.sidebar_size",
            Integer {
                minimum: 10,
                maximum: 24,
            },
            "13",
            "Room list and text-sidebar label size in logical pixels before text scaling.",
        ),
        editor(
            TextSizes,
            "Room list text size",
            Pixels,
            1,
            &[
                "sidebar",
                "room list",
                "channel list",
                "navigation",
                "font size",
                "rooms",
            ],
            None,
        ),
    ),
    metric(
        field(
            "typography.sender_size",
            Integer {
                minimum: 10,
                maximum: 32,
            },
            "(none)",
            "Sender-name size in the timeline before text scaling; omitted follows each layout's message-based size.",
        ),
        editor(
            TextSizes,
            "Sender name size",
            Pixels,
            1,
            &["name", "author", "nickname", "nick", "font size"],
            inherit("Follows the message size of each layout", 14),
        ),
    ),
    metric(
        field(
            "typography.timestamp_size",
            Integer {
                minimum: 8,
                maximum: 20,
            },
            "(none)",
            "Timestamp size in the timeline and thread replies before text scaling; omitted keeps 10.",
        ),
        editor(
            TextSizes,
            "Timestamp size",
            Pixels,
            1,
            &["time", "clock", "date", "font size"],
            inherit("Built-in 10 px", 10),
        ),
    ),
    metric(
        field(
            "typography.composer_size",
            Integer {
                minimum: 10,
                maximum: 32,
            },
            "(none)",
            "Composer text and placeholder size before text scaling; omitted follows typography.message_size.",
        ),
        editor(
            TextSizes,
            "Composer text size",
            Pixels,
            1,
            &["composer", "input", "typing", "message box", "font size"],
            inherit("Follows the message font size", 14),
        ),
    ),
    metric(
        field(
            "typography.line_height_percent",
            Integer {
                minimum: 100,
                maximum: 200,
            },
            "(none)",
            "Line spacing of the composer and plain-text previews, in percent of each font's natural line height; omitted keeps natural spacing. Rich message bodies keep their natural spacing.",
        ),
        editor(
            TextSizes,
            "Line height",
            Percent,
            5,
            &["line height", "line spacing", "leading", "between lines"],
            inherit("Natural line spacing", 100),
        ),
    ),
    metric(
        field(
            "typography.body_weight",
            Integer {
                minimum: 300,
                maximum: 800,
            },
            "(none)",
            "Default weight for message bodies, composers and other unweighted text, from 300 (light) to 800 (extra bold). Explicit heading, sender and Markdown weights remain; omitted keeps platform defaults.",
        ),
        editor(
            TextSizes,
            "Body text weight",
            Weight,
            100,
            &["weight", "bold", "light", "heavier", "thin", "body text"],
            inherit("Each surface's own weight", 400),
        ),
    ),
    metric(
        field(
            "geometry.control_radius",
            Integer {
                minimum: 0,
                maximum: 24,
            },
            "(none)",
            "Corner radius of text buttons and icon buttons; omitted keeps their share of geometry.corner_radius.",
        ),
        editor(
            Shapes,
            "Button corner radius",
            Pixels,
            1,
            &["button", "radius", "rounded", "corners", "square"],
            inherit("Share of the corner radius", 7),
        ),
    ),
    metric(
        field(
            "geometry.input_radius",
            Integer {
                minimum: 0,
                maximum: 24,
            },
            "(none)",
            "Corner radius of the text fields that geometry.input_padding lists; omitted keeps each field's built-in corners.",
        ),
        editor(
            Shapes,
            "Text field corner radius",
            Pixels,
            1,
            &["input", "field", "text box", "radius", "rounded", "corners"],
            inherit("Built-in corners of each field", 7),
        ),
    ),
    metric(
        field(
            "geometry.composer_radius",
            Integer {
                minimum: 0,
                maximum: 32,
            },
            "(none)",
            "Corner radius of the message and thread composers; omitted keeps their built-in corners.",
        ),
        editor(
            Shapes,
            "Composer corner radius",
            Pixels,
            1,
            &["composer", "message box", "radius", "rounded", "corners"],
            inherit("Built-in composer corners", 11),
        ),
    ),
    metric(
        field(
            "geometry.border_width",
            Integer {
                minimum: 0,
                maximum: 4,
            },
            "1",
            "Border width of text buttons, text fields, the message composer, attachment cards and reactions; high contrast draws at least 1.",
        ),
        editor(
            Shapes,
            "Border width",
            Pixels,
            1,
            &["border", "outline", "stroke", "line", "frame"],
            None,
        ),
    ),
    metric(
        field(
            "geometry.focus_width",
            Integer {
                minimum: 1,
                maximum: 6,
            },
            "2",
            "Keyboard focus ring width; appearance starting points keep it.",
        ),
        editor(
            Shapes,
            "Focus ring width",
            Pixels,
            1,
            &[
                "focus",
                "focus ring",
                "outline",
                "keyboard",
                "accessibility",
                "visible",
            ],
            None,
        ),
    ),
    metric(
        field(
            "geometry.control_padding",
            Integer {
                minimum: 4,
                maximum: 24,
            },
            "12",
            "Horizontal padding inside text buttons before text scaling.",
        ),
        editor(
            Spacing,
            "Button padding",
            Pixels,
            1,
            &["button", "padding", "spacing", "inset"],
            None,
        ),
    ),
    metric(
        field(
            "geometry.input_padding",
            Integer {
                minimum: 4,
                maximum: 20,
            },
            "10",
            "Horizontal padding inside the text fields of Settings, discovery, the quick switcher, highlight words and recovery.",
        ),
        editor(
            Spacing,
            "Text field padding",
            Pixels,
            1,
            &["input", "field", "text box", "padding", "inset"],
            None,
        ),
    ),
    metric(
        field(
            "geometry.composer_padding",
            Integer {
                minimum: 4,
                maximum: 24,
            },
            "10",
            "Horizontal padding inside the message and thread composers.",
        ),
        editor(
            Spacing,
            "Composer padding",
            Pixels,
            1,
            &["composer", "message box", "padding", "inset"],
            None,
        ),
    ),
    metric(
        field(
            "geometry.icon_size",
            Integer {
                minimum: 12,
                maximum: 28,
            },
            "18",
            "Icon size in icon buttons; buttons at their default size grow to fit.",
        ),
        editor(
            IconsAndAvatars,
            "Icon size",
            Pixels,
            1,
            &["icon", "glyph", "symbol", "toolbar", "button size"],
            None,
        ),
    ),
    metric(
        field(
            "geometry.avatar_size",
            Integer {
                minimum: 16,
                maximum: 64,
            },
            "(none)",
            "Message avatar size; omitted keeps 38 in Stacked and 26 in Compact and Columns.",
        ),
        editor(
            IconsAndAvatars,
            "Message avatar size",
            Pixels,
            2,
            &["avatar", "profile picture", "initials", "picture"],
            inherit("Built-in size of each layout", 38),
        ),
    ),
    metric(
        field(
            "geometry.avatar_roundness",
            Integer {
                minimum: 0,
                maximum: 50,
            },
            "(none)",
            "Avatar corner roundness in percent of each avatar's size; 50 is a circle; omitted keeps each avatar's built-in corners.",
        ),
        editor(
            IconsAndAvatars,
            "Avatar roundness",
            Percent,
            5,
            &["avatar", "circle", "round", "square", "radius", "corners"],
            inherit("Built-in corners of each avatar", 35),
        ),
    ),
    field(
        "chat.layout",
        FieldKind::Enum(&["stacked", "compact", "columns"]),
        "\"stacked\"",
        "Message layout.",
    ),
    field("chat.show_avatars", Bool, "true", "Show message avatars."),
    field(
        "chat.show_timestamps",
        Bool,
        "true",
        "Show message timestamps.",
    ),
    field(
        "chat.timestamp_format",
        FieldKind::Enum(&[
            "twelve_hour",
            "twenty_four_hour",
            "twenty_four_hour_seconds",
        ]),
        "\"twelve_hour\"",
        "Local clock format.",
    ),
    field(
        "chat.timestamp_brackets",
        Bool,
        "false",
        "Square brackets around timestamps.",
    ),
    field(
        "chat.timestamp_width",
        Integer {
            minimum: 48,
            maximum: 160,
        },
        "72",
        "Timestamp column width in Columns layout.",
    ),
    field(
        "chat.color_senders",
        Bool,
        "false",
        "Color sender names with the sender palette.",
    ),
    field(
        "chat.nickname_width",
        Integer {
            minimum: 64,
            maximum: 240,
        },
        "128",
        "Nickname column width in Columns layout.",
    ),
    field(
        "chat.nickname_brackets",
        Bool,
        "false",
        "Show Columns nicknames as <name>.",
    ),
    field(
        "chat.nickname_alignment",
        FieldKind::Enum(&["left", "right"]),
        "\"right\"",
        "Nickname alignment in Columns layout.",
    ),
    field(
        "conversation.header",
        FieldKind::Enum(&["full", "compact", "minimal"]),
        "\"full\"",
        "Room header presentation.",
    ),
    field(
        "layout.navigation",
        FieldKind::Enum(&["rail", "sidebar"]),
        "\"rail\"",
        "Icon rail or text sidebar.",
    ),
    field(
        "layout.room_list_width",
        Integer {
            minimum: 180,
            maximum: 480,
        },
        "260",
        "Room list width.",
    ),
    field(
        "layout.details_width",
        Integer {
            minimum: 200,
            maximum: 520,
        },
        "300",
        "Details panel width.",
    ),
    field(
        "layout.show_member_panel",
        Bool,
        "true",
        "Show the members and thread panel.",
    ),
    field(
        "layout.show_space_labels",
        Bool,
        "false",
        "Labels under icon-rail Spaces.",
    ),
    field(
        "layout.group_direct_messages",
        Bool,
        "true",
        "List direct messages separately.",
    ),
    field(
        "layout.sort_unread_first",
        Bool,
        "false",
        "Unread conversations first.",
    ),
    field(
        "layout.details_side",
        FieldKind::Enum(&["right", "left"]),
        "\"right\"",
        "Side of the conversation that shows the details panel.",
    ),
    field(
        "layout.direct_messages_first",
        Bool,
        "false",
        "With grouped lists, direct messages above rooms.",
    ),
    field(
        "layout.room_density",
        FieldKind::Enum(DENSITIES),
        "(none)",
        "Room-list row density; omitted follows appearance.density.",
    ),
    field(
        "layout.message_density",
        FieldKind::Enum(DENSITIES),
        "(none)",
        "Timeline message spacing density; omitted follows appearance.density.",
    ),
    field(
        "layout.pane_order",
        FieldKind::PaneOrder,
        "[]",
        "Left-to-right pane order: spaces, rooms, conversation, details, each once; empty follows details_side.",
    ),
    field(
        "layout.hidden_panes",
        FieldKind::PaneSet(&["spaces", "rooms", "details"]),
        "[]",
        "Panes not shown; the conversation and the account control always are.",
    ),
    field(
        "layout.collapsed_panes",
        FieldKind::PaneSet(&["rooms", "details"]),
        "[]",
        "Panes shown as a narrow strip with a restore control.",
    ),
    field(
        "activity.send_typing",
        Bool,
        "false",
        "Tell rooms while you are typing.",
    ),
    field(
        "activity.public_read_receipts",
        Bool,
        "false",
        "Public read receipts others can see; otherwise private.",
    ),
    field(
        "activity.read_tracking",
        Bool,
        "false",
        "Mark the newest message shown in a focused, live timeline as read.",
    ),
    field(
        "notifications.enabled",
        Bool,
        "true",
        "Desktop notifications.",
    ),
    field("notifications.sounds", Bool, "true", "Notification sounds."),
    field(
        "notifications.show_message_preview",
        Bool,
        "false",
        "Decrypted text in desktop notifications.",
    ),
    field(
        "notifications.show_room_and_sender",
        Bool,
        "true",
        "Room names and senders in desktop notifications; off names only the account.",
    ),
    field(
        "notifications.highlight_words",
        FieldKind::TextList,
        "[]",
        "Local highlight words and phrases; Settings accepts up to 32 of at most 64 characters.",
    ),
    field(
        "media.microphone_device",
        FieldKind::DeviceId,
        "(none)",
        "Microphone for the next call; omitted follows the system default.",
    ),
    field(
        "media.speaker_device",
        FieldKind::DeviceId,
        "(none)",
        "Speaker for the next call; omitted follows the system default.",
    ),
    inactive(field(
        "media.camera_device",
        FieldKind::Text,
        "(none)",
        "Saved camera; not used yet.",
    )),
    inactive(field(
        "media.noise_suppression",
        Bool,
        "true",
        "Noise suppression; not used yet.",
    )),
    field(
        "media.autoplay_animated_media",
        Bool,
        "false",
        "Start animated image previews playing when opened; reduced motion keeps them paused.",
    ),
    field(
        "media.previews_in_encrypted_rooms",
        Bool,
        "false",
        "Image previews in encrypted rooms.",
    ),
    field(
        "shortcuts.<action>",
        FieldKind::Table,
        "see description",
        "Key combination per action; `config schema` lists every action and its default. Omit an action to disable it.",
    ),
    inactive(field(
        "experimental.background_account_sync",
        Bool,
        "false",
        "Background sync of inactive accounts; not used yet.",
    )),
    field(
        "account_overrides.<account>.notifications_enabled",
        Bool,
        "(none)",
        "Desktop notifications for one local account ID.",
    ),
    field(
        "account_overrides.<account>.theme",
        FieldKind::Enum(THEME_MODES),
        "(none)",
        "Base palette while this account is active.",
    ),
    field(
        "account_overrides.<account>.local_voice_rooms",
        FieldKind::RoomIdList,
        "[]",
        "Rooms shown as voice spaces for this account.",
    ),
    field(
        "account_overrides.<account>.legacy_element_call_rooms",
        FieldKind::RoomIdList,
        "[]",
        "Rooms using the legacy Element Call mode.",
    ),
    field(
        "account_overrides.<account>.element_call_matrix_2_0_rooms",
        FieldKind::RoomIdList,
        "[]",
        "Rooms using the Element Call Matrix 2.0 mode.",
    ),
    field(
        "account_overrides.<account>.colors.<token>",
        FieldKind::Color,
        "(none)",
        "Color override while this local account is active, drawn over [colors]; removing the account removes it.",
    ),
    field(
        "account_overrides.<account>.room_notifications.<room>.quiet",
        Bool,
        "false",
        "No sound for this room's notifications on this account. At most 256 rooms per account; the room's level is its Matrix push rule, set in Settings or the room header.",
    ),
    field(
        "account_overrides.<account>.room_notifications.<room>.hide_preview",
        Bool,
        "false",
        "Never preview message text from this room, even when previews are on; it cannot turn previews on.",
    ),
    field(
        "account_overrides.<account>.room_notifications.<room>.no_highlights",
        Bool,
        "false",
        "Local highlight words do not mark this room's notifications; Matrix mentions and keywords still do.",
    ),
    field(
        "account_overrides.<account>.room_appearance.<room>.theme",
        FieldKind::Enum(THEME_MODES),
        "(none)",
        "Base palette while this room of this account is open; omitted inherits the account's, then the global one. At most 64 rooms per account.",
    ),
    field(
        "account_overrides.<account>.room_appearance.<room>.colors.<token>",
        FieldKind::Color,
        "(none)",
        "Color override while this room of this account is open, drawn over the account's and the global colors; removing the account removes it.",
    ),
    field(
        "colors.<token>",
        FieldKind::Color,
        "(none)",
        "Sparse color override for one token; see the token list.",
    ),
    field(
        "senders.colors",
        FieldKind::ColorList,
        "(none)",
        "Sender-name palette replacing the theme's; each sender keeps the slot derived from their user ID.",
    ),
    field(
        "window.opacity_percent",
        Integer {
            minimum: 0,
            maximum: 100,
        },
        "100",
        "Share of the window panes' own alpha that is painted; text, controls and dialogs stay opaque.",
    ),
];

/// Something a search result must have available to be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    Always,
    /// Shown only while a Matrix session is signed in.
    LiveSession,
}

/// Which control of Manage accounts receives focus. None of them acts on
/// focus; each still needs its own activation and that dialog's confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountsFocus {
    /// The dialog's close action.
    Close,
    /// Add account.
    Add,
    /// Set or change the app password.
    Protection,
    /// Open the current account's public profile editor. Focus alone opens
    /// nothing; the editor only reads until the user explicitly saves.
    Profile,
}

/// How a search result reaches its control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Scroll the Settings page to the control with this target's ID and
    /// focus it.
    Settings,
    /// Open Manage accounts and focus one of its controls.
    ManageAccounts(AccountsFocus),
    /// Show the transfer center of the account on screen and focus its
    /// heading. Nothing is started, cancelled or retried.
    Transfers,
    /// Open the invitations center of the account on screen on Home and
    /// focus its search field. Nothing is accepted or declined.
    Invitations,
}

pub struct SettingTarget {
    /// Stable ID; the Settings page names the same ID on the control.
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub keywords: &'static [&'static str],
    pub category: &'static str,
    pub breadcrumb: &'static str,
    pub route: Route,
    pub requires: Requirement,
}

const fn target(
    id: &'static str,
    category: &'static str,
    breadcrumb: &'static str,
    label: &'static str,
    description: &'static str,
    keywords: &'static [&'static str],
) -> SettingTarget {
    SettingTarget {
        id,
        label,
        description,
        keywords,
        category,
        breadcrumb,
        route: Route::Settings,
        requires: Requirement::Always,
    }
}

const fn live(mut setting: SettingTarget) -> SettingTarget {
    setting.requires = Requirement::LiveSession;
    setting
}

const fn accounts(mut setting: SettingTarget, focus: AccountsFocus) -> SettingTarget {
    setting.route = Route::ManageAccounts(focus);
    setting
}

const fn transfers(mut setting: SettingTarget) -> SettingTarget {
    setting.route = Route::Transfers;
    setting
}

const fn invitations(mut setting: SettingTarget) -> SettingTarget {
    setting.route = Route::Invitations;
    setting
}

const APPEARANCE: &str = "Appearance";

/// Every Settings search target. Each ID must have a destination in the
/// interface; a generated-window test reveals every one of them.
pub const SETTING_TARGETS: &[SettingTarget] = &[
    target(
        "appearance.presets",
        APPEARANCE,
        "Settings › Appearance › Starting points",
        "Appearance starting points",
        "Fill in fonts, layout, geometry and palette from an editable preset.",
        &[
            "preset", "theme", "style", "nocturne", "terminal", "paper", "template",
        ],
    ),
    target(
        "appearance.variations",
        APPEARANCE,
        "Settings › Appearance › Starting points › Variations",
        "Starting point variations",
        "Try a compact, large-print, soft, wide or high-contrast variation of a starting point before applying it.",
        &[
            "variation",
            "variations",
            "large print",
            "contrast reading",
            "nocturne compact",
            "terminal soft",
            "paper wide",
        ],
    ),
    target(
        "appearance.studio",
        APPEARANCE,
        "Settings › Appearance",
        "Unsaved appearance changes",
        "Look changes made in Settings are a draft: apply, discard, undo or reset a section.",
        &[
            "draft",
            "unsaved",
            "apply changes",
            "discard",
            "undo",
            "redo",
            "reset section",
            "revert",
        ],
    ),
    target(
        "appearance.style_profiles",
        APPEARANCE,
        "Settings › Appearance › Style profiles",
        "Style profiles",
        "Save, preview, import and export named looks.",
        &["profile", "saved look", "import", "export", "share look"],
    ),
    target(
        "appearance.theme_packs",
        APPEARANCE,
        "Settings › Appearance › Theme packs",
        "Theme packs",
        "Import a palette file, preview it and install it, or export the palette in use.",
        &[
            "theme pack",
            "palette file",
            "import theme",
            "export theme",
            "install theme",
        ],
    ),
    target(
        "appearance.colors",
        APPEARANCE,
        "Settings › Appearance › Colors and window",
        "Colors",
        "Edit any color with a hex value, and reset one color without resetting the theme.",
        &[
            "color", "colour", "colors", "colours", "hex", "palette", "theme", "rgb", "rgba",
        ],
    ),
    target(
        "appearance.sender_colors",
        APPEARANCE,
        "Settings › Appearance › Sender name colors",
        "Sender name colors",
        "Edit, reorder or remove the colors used for sender names, with a contrast check.",
        &[
            "sender color",
            "nick color",
            "name color",
            "nickname",
            "palette",
            "usernames",
        ],
    ),
    target(
        "appearance.window_opacity",
        APPEARANCE,
        "Settings › Appearance › Colors and window › Window",
        "Window transparency",
        "See the desktop through the window background while text stays opaque.",
        &[
            "transparency",
            "transparent",
            "opacity",
            "translucent",
            "alpha",
            "see through",
            "background",
        ],
    ),
    target(
        "appearance.base_palette",
        APPEARANCE,
        "Settings › Appearance › Base palette",
        "Base palette",
        "Dark, light, high contrast or follow the system.",
        &[
            "dark mode",
            "light mode",
            "high contrast",
            "system theme",
            "theme mode",
        ],
    ),
    target(
        "appearance.reduce_motion",
        APPEARANCE,
        "Settings › Appearance › Reduce motion",
        "Reduce motion",
        "Use immediate hover feedback instead of short fades.",
        &[
            "animation",
            "animations",
            "motion",
            "reduced motion",
            "accessibility",
        ],
    ),
    target(
        "appearance.hover_duration",
        APPEARANCE,
        "Settings › Appearance › Hover fade duration",
        "Hover fade duration",
        "Set button and navigation hover timing; Reduce motion takes priority.",
        &[
            "fade",
            "hover",
            "duration",
            "timing",
            "animation speed",
            "milliseconds",
        ],
    ),
    target(
        "appearance.fonts",
        APPEARANCE,
        "Settings › Appearance › Fonts",
        "Font families",
        "Interface, message, composer and monospaced font families, with a browser of the installed fonts.",
        &[
            "font",
            "typeface",
            "family",
            "monospace",
            "serif",
            "typography",
            "installed fonts",
            "font browser",
            "composer font",
        ],
    ),
    target(
        "appearance.message_size",
        APPEARANCE,
        "Settings › Appearance › Fonts",
        "Message font size",
        "Size of message text before text scaling.",
        &["font size", "text size", "typography"],
    ),
    target(
        "appearance.sender_weight",
        APPEARANCE,
        "Settings › Appearance › Fonts",
        "Sender font weight",
        "Weight of sender names in the timeline and threads.",
        &["bold", "weight", "font weight", "typography"],
    ),
    target(
        "appearance.room_header",
        APPEARANCE,
        "Settings › Appearance › Room header",
        "Room header",
        "Full, compact or minimal conversation header.",
        &["header", "topic", "title bar"],
    ),
    target(
        "appearance.message_layout",
        APPEARANCE,
        "Settings › Appearance › Conversation style",
        "Conversation style",
        "Stacked, compact or column message layout.",
        &["layout", "irc", "columns", "compact", "message style"],
    ),
    target(
        "appearance.message_details",
        APPEARANCE,
        "Settings › Appearance › Conversation style",
        "Avatars, timestamps and sender colors",
        "Show or hide message avatars and timestamps, and color sender names.",
        &[
            "avatar",
            "avatars",
            "timestamp",
            "sender color",
            "nick color",
        ],
    ),
    target(
        "appearance.timestamps",
        APPEARANCE,
        "Settings › Appearance › Timestamp style",
        "Timestamp style",
        "12-hour, 24-hour or 24-hour with seconds.",
        &["time", "clock", "24 hour", "seconds", "date format"],
    ),
    target(
        "appearance.geometry",
        APPEARANCE,
        "Settings › Appearance › Geometry",
        "Corner radius",
        "Roundness of controls and conversation surfaces.",
        &[
            "radius",
            "rounded",
            "corners",
            "square",
            "geometry",
            "border radius",
        ],
    ),
    target(
        "appearance.spacing",
        APPEARANCE,
        "Settings › Appearance › Geometry",
        "Message spacing and inset",
        "Space between messages and around the conversation.",
        &["spacing", "padding", "gap", "margin", "inset"],
    ),
    target(
        "appearance.rail_width",
        APPEARANCE,
        "Settings › Appearance › Geometry",
        "Space rail width",
        "Width of the Space and navigation rail.",
        &["rail", "pane size", "width", "sidebar width"],
    ),
    target(
        "appearance.metrics",
        APPEARANCE,
        "Settings › Appearance › Typography, geometry and motion",
        "Typography, geometry and motion details",
        "Text sizes, corners, borders, focus rings, padding, icons, avatars and fades, one at a time.",
        &[
            "metrics",
            "sizes",
            "advanced",
            "details",
            "border",
            "padding",
            "icon size",
            "avatar size",
            "fade",
        ],
    ),
    target(
        "appearance.font_scale",
        APPEARANCE,
        "Settings › Appearance › Font scale",
        "Font scale",
        "Scale all text from 75% to 200%.",
        &["zoom", "text size", "scale", "bigger text", "accessibility"],
    ),
    target(
        "appearance.density",
        APPEARANCE,
        "Settings › Appearance › Density",
        "Density",
        "Compact, comfortable or spacious room and message rows.",
        &["compact", "spacious", "row height", "spacing"],
    ),
    target(
        "layout.navigation",
        "Layout",
        "Settings › Layout › Navigation",
        "Navigation",
        "Icon rail or text sidebar, Space labels and direct message grouping.",
        &["sidebar", "rail", "spaces", "direct messages", "dm"],
    ),
    target(
        "layout.panes",
        "Layout",
        "Settings › Layout",
        "Pane sizes and members panel",
        "Room list width, details panel width and the members panel.",
        &["pane", "panel", "width", "members", "details", "resize"],
    ),
    target(
        "layout.composition",
        "Layout",
        "Settings › Layout › Navigation",
        "Pane order and density",
        "Details panel side, direct messages above rooms, and separate room-list and message density.",
        &[
            "pane order",
            "panel side",
            "left",
            "right",
            "direct messages first",
            "dm first",
            "density",
            "compact",
            "spacious",
        ],
    ),
    target(
        "layout.arrangement",
        "Layout",
        "Settings › Layout › Pane arrangement",
        "Pane arrangement",
        "Presets, pane order, and hiding or collapsing the Space rail, room list and details panel.",
        &[
            "arrangement",
            "pane order",
            "reorder",
            "hide panel",
            "collapse",
            "restore layout",
            "workspace",
            "preset",
        ],
    ),
    target(
        "layout.unread_first",
        "Layout",
        "Settings › Layout",
        "Unread rooms first",
        "Sort unread conversations above read ones.",
        &["sort", "order", "unread"],
    ),
    target(
        "notifications.desktop",
        "Notifications",
        "Settings › Notifications",
        "Desktop notifications",
        "Show notifications for the active account.",
        &["notification", "alerts", "banner", "popups"],
    ),
    target(
        "notifications.sounds",
        "Notifications",
        "Settings › Notifications",
        "Notification sounds",
        "Play a sound with notifications.",
        &["sound", "audio", "mute", "ping"],
    ),
    target(
        "notifications.previews",
        "Notifications",
        "Settings › Notifications",
        "Message previews in notifications",
        "Show decrypted text in desktop notifications.",
        &["preview", "privacy", "content"],
    ),
    target(
        "notifications.highlights",
        "Notifications",
        "Settings › Notifications",
        "Highlight words",
        "Words and phrases that mark a notification as a highlight.",
        &["keywords", "mentions", "highlight", "alert words"],
    ),
    live(target(
        "notifications.account",
        "Notifications",
        "Settings › Notifications",
        "Notifications for this account",
        "Turn desktop notifications and call alerts off for the active account only.",
        &[
            "account",
            "silence",
            "mute account",
            "per account",
            "work account",
        ],
    )),
    target(
        "notifications.identity",
        "Notifications",
        "Settings › Notifications",
        "Room and sender in notifications",
        "Show or hide room names and senders in desktop notifications.",
        &["privacy", "lock screen", "sender", "room name", "hide"],
    ),
    live(target(
        "notifications.defaults",
        "Notifications",
        "Settings › Notifications › Defaults",
        "Default notification level",
        "All messages, or mentions and keywords, for rooms and direct messages.",
        &["default", "direct messages", "dm", "group", "all messages"],
    )),
    live(target(
        "notifications.keywords",
        "Notifications",
        "Settings › Notifications › Keywords",
        "Notification keywords",
        "Matrix keywords that notify in rooms set to mentions and keywords, on all devices.",
        &[
            "keyword",
            "keywords",
            "alert words",
            "push rules",
            "mentions and keywords",
        ],
    )),
    live(target(
        "notifications.rooms",
        "Notifications",
        "Settings › Notifications › Rooms",
        "Room notification rules",
        "Search rooms and choose all messages, mentions and keywords, or mute.",
        &[
            "mute",
            "mentions",
            "keywords",
            "per room",
            "push rules",
            "room notifications",
        ],
    )),
    live(target(
        "notifications.this_room",
        "Notifications",
        "Settings › Notifications › Rooms",
        "This room's notifications",
        "Open the open room's panel: its level, and its sound, preview and highlights on this device.",
        &[
            "this room",
            "bell",
            "quiet room",
            "room sound",
            "room preview",
            "room highlights",
            "silence room",
        ],
    )),
    target(
        "privacy.activity",
        "Privacy",
        "Settings › Typing and read receipts",
        "Typing and read receipts",
        "Send typing indicators, public read receipts, and private automatic read tracking.",
        &[
            "send typing indicators",
            "typing indicator",
            "public read receipts",
            "private read receipts",
            "read receipt",
            "seen",
            "mark as read",
            "read marker",
            "read tracking",
        ],
    ),
    accounts(
        target(
            "accounts.manage",
            "Accounts",
            "Accounts › Manage accounts",
            "Add or open accounts",
            "Open Manage accounts to add, open or remove a saved account.",
            &[
                "account",
                "add account",
                "sign in",
                "login",
                "switch account",
            ],
        ),
        AccountsFocus::Add,
    ),
    accounts(
        target(
            "accounts.remove",
            "Accounts",
            "Accounts › Manage accounts",
            "Remove an account",
            "Each saved account has a Remove action in Manage accounts; removing asks for confirmation.",
            &[
                "remove",
                "delete account",
                "forget",
                "log out",
                "logout",
                "sign out",
            ],
        ),
        AccountsFocus::Close,
    ),
    accounts(
        target(
            "accounts.switch_password",
            "Security",
            "Accounts › Manage accounts › Switch protection",
            "App password for account switching",
            "Set or change the local password required to switch accounts.",
            &[
                "password",
                "app password",
                "switch password",
                "lock",
                "protection",
                "security",
            ],
        ),
        AccountsFocus::Protection,
    ),
    accounts(
        live(target(
            "accounts.public_profile",
            "Accounts",
            "Accounts › Manage accounts › Public profile",
            "Public profile (display name and avatar)",
            "Change the display name and avatar other people see for the current Matrix account.",
            &[
                "profile",
                "display name",
                "avatar",
                "picture",
                "photo",
                "identity",
                "alias",
            ],
        )),
        AccountsFocus::Profile,
    ),
    live(target(
        "security.recovery",
        "Security",
        "Settings › Security & recovery",
        "Security & recovery",
        "Device verification, recovery key setup and recovering this device, for the signed-in account.",
        &[
            "security",
            "encryption",
            "e2ee",
            "verify",
            "verification",
            "sas",
            "emoji",
            "device",
            "recovery",
            "recovery key",
            "passphrase",
            "backup",
            "key backup",
            "cross signing",
        ],
    )),
    live(target(
        "security.room_key_files",
        "Security",
        "Settings › Security & recovery › Encrypted room-key files",
        "Encrypted room-key files",
        "Import a Matrix-compatible encrypted key file, or export this account's room keys with a fresh passphrase.",
        &[
            "room keys",
            "import room keys",
            "export room keys",
            "encrypted key file",
            "key file import",
            "key file export",
            "Matrix key import",
            "Matrix key export",
        ],
    )),
    live(target(
        "security.peer_verification",
        "Security",
        "Settings › Security & recovery › Verify a person",
        "Verify a person",
        "Compare emoji or numbers with an exact Matrix ID, or review incoming verification requests.",
        &[
            "peer verification",
            "person verification",
            "verify person",
            "verify someone",
            "matrix id",
            "sas comparison",
            "emoji comparison",
            "incoming verification",
            "incoming request",
            "received request",
        ],
    )),
    live(target(
        "security.devices",
        "Security",
        "Settings › Security & recovery › Devices",
        "Devices and sessions",
        "Where this account is signed in: rename, verify or sign out other sessions.",
        &[
            "devices",
            "sessions",
            "session",
            "signed in",
            "other devices",
            "sign out other",
            "rename device",
            "verify session",
            "trusted devices",
        ],
    )),
    live(target(
        "privacy.ignored_users",
        "Security",
        "Settings › Security & recovery › Ignored users",
        "Ignored users",
        "People this account ignores: search the list and stop ignoring someone.",
        &[
            "ignored users",
            "ignore list",
            "unignore",
            "stop ignoring",
            "blocked users",
            "unblock",
            "block list",
        ],
    )),
    live(target(
        "rooms.addresses",
        "Rooms",
        "Settings › Room addresses",
        "Room addresses",
        "List the open room's addresses, add or remove ones on your homeserver, and choose the published ones.",
        &[
            "address",
            "addresses",
            "room address",
            "alias",
            "aliases",
            "canonical alias",
            "main address",
            "published address",
        ],
    )),
    target(
        "media.image_previews",
        "Media",
        "Settings › Image previews",
        "Image previews in encrypted rooms",
        "Allow image previews in encrypted conversations.",
        &["image", "images", "media", "preview", "photos"],
    ),
    target(
        "media.autoplay_animated_media",
        "Media",
        "Settings › Image previews › Animated image playback",
        "Animated image autoplay",
        "Choose whether animated image previews start playing when opened; reduced motion keeps them paused.",
        &[
            "autoplay",
            "animated images",
            "gif playback",
            "gif",
            "playback",
        ],
    ),
    target(
        "media.audio_devices",
        "Media",
        "Settings › Call audio devices",
        "Call audio devices",
        "Microphone and speaker for your next call.",
        &[
            "microphone",
            "mic",
            "speaker",
            "headphones",
            "audio",
            "sound device",
            "call",
        ],
    ),
    live(transfers(target(
        "files.transfers",
        "Files",
        "Conversation › Files",
        "Transfers",
        "Uploads, saved attachments and what recently ended for this account.",
        &[
            "transfer",
            "upload",
            "download",
            "attachment",
            "saved attachment",
            "progress",
            "cancel upload",
        ],
    ))),
    live(invitations(target(
        "rooms.invitations",
        "Rooms",
        "Home › Invitations",
        "Invitations",
        "Search this account's room and Space invitations and accept or decline each one.",
        &[
            "invitation",
            "invitations",
            "invite",
            "invited",
            "decline",
            "join room",
            "spam invites",
        ],
    ))),
    target(
        "shortcuts",
        "Shortcuts",
        "Settings › Keyboard shortcuts",
        "Keyboard shortcuts",
        "Record, type, disable or restore shortcuts for rooms, the composer, search and panels.",
        &[
            "shortcut",
            "hotkey",
            "keybinding",
            "keys",
            "keyboard",
            "record",
            "conflict",
        ],
    ),
    target(
        "advanced.config_file",
        "Advanced",
        "Settings › Advanced customization",
        "Settings file",
        "Edit config.toml and theme files directly; valid changes apply automatically.",
        &["toml", "config", "file", "theme file", "advanced"],
    ),
    target(
        "updates",
        "Updates",
        "Settings › Application updates",
        "Application updates",
        "Check for, download and install updates.",
        &["update", "upgrade", "version", "release"],
    ),
];

pub fn setting_target(id: &str) -> Option<&'static SettingTarget> {
    SETTING_TARGETS.iter().find(|target| target.id == id)
}

/// Longest query considered, in characters.
pub const MAX_SEARCH_QUERY_CHARS: usize = 64;
/// Most results returned for one query.
pub const MAX_SEARCH_RESULTS: usize = 8;

/// Search the Settings targets that are available now. Matching is
/// case-insensitive over the label, keywords, category and description; every
/// word of the query must match. Results are ranked, stable and bounded.
pub fn search_settings(
    query: &str,
    available: impl Fn(Requirement) -> bool,
) -> Vec<&'static SettingTarget> {
    let query: String = query.chars().take(MAX_SEARCH_QUERY_CHARS).collect();
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut ranked: Vec<(u32, usize, &'static SettingTarget)> = SETTING_TARGETS
        .iter()
        .enumerate()
        .filter(|(_, target)| available(target.requires))
        .filter_map(|(order, target)| Some((score(target, &words)?, order, target)))
        .collect();
    ranked.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    ranked
        .into_iter()
        .take(MAX_SEARCH_RESULTS)
        .map(|(_, _, target)| target)
        .collect()
}

/// The result-ID prefix of one color token in Settings search; the rest of
/// the ID is a registry token ID.
pub const COLOR_RESULT_PREFIX: &str = "color:";

/// The result-ID prefix of one typography or geometry metric in Settings
/// search; the rest of the ID is a registry field key.
pub const METRIC_RESULT_PREFIX: &str = "metric:";

/// One Settings search result. Every kind comes from the static registry
/// tables, so a result can only name a known setting, token or metric.
#[derive(Clone, Copy)]
pub enum SearchHit {
    /// Choosing it reveals and focuses the setting's control.
    Setting(&'static SettingTarget),
    /// Choosing it selects the token in the color editor and focuses its
    /// hex field.
    Color(&'static ColorToken),
    /// Choosing it selects the metric in the typography and geometry
    /// workspace and focuses its value.
    Metric(&'static FieldSpec, &'static MetricEditor),
}

impl SearchHit {
    /// The result ID: a setting ID, [`COLOR_RESULT_PREFIX`] and a token ID, or
    /// [`METRIC_RESULT_PREFIX`] and a field key.
    pub fn id(self) -> String {
        match self {
            Self::Setting(target) => target.id.to_owned(),
            Self::Color(token) => format!("{COLOR_RESULT_PREFIX}{}", token.id),
            Self::Metric(field, _) => format!("{METRIC_RESULT_PREFIX}{}", field.key),
        }
    }
}

/// The color token a Settings search result ID names. Only registry token
/// IDs are accepted.
pub fn color_result(id: &str) -> Option<&'static ColorToken> {
    id.strip_prefix(COLOR_RESULT_PREFIX).and_then(color_token)
}

/// The editor metric a Settings search result ID names. Only registry keys
/// with editor metadata are accepted.
pub fn metric_result(id: &str) -> Option<(&'static FieldSpec, &'static MetricEditor)> {
    id.strip_prefix(METRIC_RESULT_PREFIX).and_then(metric_field)
}

/// Search the available Settings targets, every color token and every
/// editor metric, so one query can reach a single color or metric directly.
/// Matching is as in [`search_settings`]; a token or metric also matches the
/// words of its ID. Results are ranked best first, then settings, tokens and
/// metrics at equal score, each in registry order, and bounded by
/// [`MAX_SEARCH_RESULTS`].
pub fn search(query: &str, available: impl Fn(Requirement) -> bool) -> Vec<SearchHit> {
    let query: String = query.chars().take(MAX_SEARCH_QUERY_CHARS).collect();
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return Vec::new();
    }
    let settings = SETTING_TARGETS
        .iter()
        .enumerate()
        .filter(|(_, target)| available(target.requires))
        .filter_map(|(order, target)| {
            Some((score(target, &words)?, 0, order, SearchHit::Setting(target)))
        });
    let colors = COLOR_TOKENS
        .iter()
        .enumerate()
        .filter_map(|(order, token)| {
            Some((
                token_score(token, &words)?,
                1,
                order,
                SearchHit::Color(token),
            ))
        });
    let metrics = metric_fields()
        .enumerate()
        .filter_map(|(order, (field, editor))| {
            Some((
                metric_score(field, editor, &words)?,
                2,
                order,
                SearchHit::Metric(field, editor),
            ))
        });
    let mut ranked: Vec<(u32, u8, usize, SearchHit)> =
        settings.chain(colors).chain(metrics).collect();
    ranked.sort_by(|left, right| {
        right
            .0
            .cmp(&left.0)
            .then(left.1.cmp(&right.1))
            .then(left.2.cmp(&right.2))
    });
    ranked
        .into_iter()
        .take(MAX_SEARCH_RESULTS)
        .map(|(_, _, _, hit)| hit)
        .collect()
}

fn token_score(token: &ColorToken, words: &[&str]) -> Option<u32> {
    let mut keywords: Vec<&str> = token.keywords.to_vec();
    keywords.push(token.id);
    keywords.extend(token.id.split('_'));
    score_fields(
        token.label,
        token.group.label(),
        &keywords,
        token.description,
        words,
    )
}

fn metric_score(field: &FieldSpec, editor: &MetricEditor, words: &[&str]) -> Option<u32> {
    let mut keywords: Vec<&str> = editor.keywords.to_vec();
    keywords.push(field.key);
    keywords.extend(field.key.split(['.', '_']));
    score_fields(
        editor.label,
        editor.group.label(),
        &keywords,
        field.description,
        words,
    )
}

fn score(target: &SettingTarget, words: &[&str]) -> Option<u32> {
    score_fields(
        target.label,
        target.category,
        target.keywords,
        target.description,
        words,
    )
}

fn score_fields(
    label: &str,
    category: &str,
    keywords: &[&str],
    description: &str,
    words: &[&str],
) -> Option<u32> {
    let label = label.to_lowercase();
    let category = category.to_lowercase();
    let description = description.to_lowercase();
    let mut total = 0;
    for word in words {
        let keyword = keywords.iter().map(|keyword| keyword.to_lowercase());
        let best = if label.split_whitespace().any(|part| part.starts_with(word)) {
            8
        } else if keyword.clone().any(|keyword| keyword == *word) {
            7
        } else if keyword.clone().any(|keyword| keyword.starts_with(word)) {
            5
        } else if label.contains(word) || category.starts_with(word) {
            4
        } else if keyword.clone().any(|keyword| keyword.contains(word)) {
            3
        } else if description.contains(word) {
            1
        } else {
            return None;
        };
        total += best;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn token_ids_are_unique_stable_keys_with_resolvable_fallbacks() {
        let mut ids = HashSet::new();
        for token in COLOR_TOKENS {
            assert!(ids.insert(token.id), "duplicate token {}", token.id);
            assert!(
                token
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{} is not a stable snake_case key",
                token.id
            );
            assert!(!token.label.is_empty() && !token.description.is_empty());
            match token.fallback {
                Fallback::Palette => assert_eq!(token.source, TokenSource::Base, "{}", token.id),
                Fallback::Token(other) => {
                    assert!(color_token(other).is_some(), "{} follows {other}", token.id);
                    assert_ne!(other, token.id);
                }
                Fallback::Legacy { example } => {
                    assert!(parse_hex_color(example).is_some(), "{}", token.id);
                }
            }
        }
        // The eleven theme-pack colors are all editable tokens.
        for base in [
            "rail",
            "sidebar",
            "canvas",
            "panel",
            "elevated",
            "hover",
            "ink",
            "muted",
            "divider",
            "accent",
            "accent_ink",
        ] {
            assert_eq!(
                color_token(base).map(|token| token.source),
                Some(TokenSource::Base)
            );
        }
        assert_eq!(
            COLOR_TOKENS
                .iter()
                .filter(|token| token.source == TokenSource::Base)
                .count(),
            11
        );
        for group in TokenGroup::ALL {
            assert!(COLOR_TOKENS.iter().any(|token| token.group == group));
        }
    }

    #[test]
    fn hex_colors_accept_rgb_and_rgba_with_alpha_last() {
        assert_eq!(parse_hex_color("#1e2330"), Some([0x1e, 0x23, 0x30, 0xff]));
        assert_eq!(parse_hex_color("#1E233080"), Some([0x1e, 0x23, 0x30, 0x80]));
        assert_eq!(parse_hex_color("#00000000"), Some([0, 0, 0, 0]));
        for invalid in [
            "",
            "#",
            "#123",
            "#1234",
            "#12345",
            "#1234567",
            "#123456789",
            "1E2330",
            "#GG2330",
            "#1E2330 ",
            " #1E2330",
            "#１２３４５６",
            "red",
            "#+12345",
        ] {
            assert_eq!(parse_hex_color(invalid), None, "{invalid:?}");
        }
        assert_eq!(format_hex_color([0x1e, 0x23, 0x30, 0xff]), "#1E2330");
        assert_eq!(format_hex_color([0x1e, 0x23, 0x30, 0x80]), "#1E233080");
        for value in ["#ABCDEF", "#ABCDEF01", "#00000000"] {
            assert_eq!(format_hex_color(parse_hex_color(value).unwrap()), value);
        }
    }

    #[test]
    fn setting_targets_are_unique_and_searchable_by_their_aliases() {
        let mut ids = HashSet::new();
        for target in SETTING_TARGETS {
            assert!(ids.insert(target.id), "duplicate target {}", target.id);
            assert!(!target.label.is_empty() && !target.description.is_empty());
            assert!(!target.category.is_empty() && !target.breadcrumb.is_empty());
        }
        let all = |_| true;
        let first = |query: &str| search_settings(query, all).first().map(|target| target.id);
        assert_eq!(first("colour"), Some("appearance.colors"));
        assert_eq!(first("COLOR"), Some("appearance.colors"));
        assert_eq!(first("transparency"), Some("appearance.window_opacity"));
        assert_eq!(first("opacity"), Some("appearance.window_opacity"));
        assert_eq!(first("font"), Some("appearance.fonts"));
        assert_eq!(first("animations"), Some("appearance.reduce_motion"));
        for query in ["autoplay", "animated images", "GIF playback"] {
            assert_eq!(
                first(query),
                Some("media.autoplay_animated_media"),
                "{query}"
            );
        }
        assert_eq!(first("switch password"), Some("accounts.switch_password"));
        assert_eq!(first("microphone"), Some("media.audio_devices"));
        assert_eq!(first("hotkey"), Some("shortcuts"));
        assert_eq!(first("recovery key"), Some("security.recovery"));
        assert!(search_settings("", all).is_empty());
        assert!(search_settings("   ", all).is_empty());
        assert!(search_settings("no such setting zzz", all).is_empty());
        // Bounded: a query matching many targets returns at most the limit.
        assert!(search_settings("e", all).len() <= MAX_SEARCH_RESULTS);
        let long = "a".repeat(10_000);
        assert!(search_settings(&long, all).is_empty());
        // Unavailable targets are never offered.
        let offline = |requirement| requirement == Requirement::Always;
        assert!(
            search_settings("verify", offline)
                .iter()
                .all(|target| target.requires == Requirement::Always)
        );
    }

    #[test]
    fn encrypted_room_key_file_search_is_live_and_keeps_own_recovery_separate() {
        let all = |_| true;
        for query in [
            "room keys",
            "import room keys",
            "export room keys",
            "encrypted key file",
            "key file import",
            "key file export",
            "Matrix key import",
            "Matrix key export",
        ] {
            assert_eq!(
                search_settings(query, all).first().map(|target| target.id),
                Some("security.room_key_files"),
                "{query}"
            );
        }
        let target = setting_target("security.room_key_files").unwrap();
        assert_eq!(target.route, Route::Settings);
        assert_eq!(target.requires, Requirement::LiveSession);
        let offline = |requirement| requirement == Requirement::Always;
        assert!(search_settings("room keys", offline).is_empty());
        assert!(search_settings("encrypted key file", offline).is_empty());
        assert_eq!(
            search_settings("recovery key", all)
                .first()
                .map(|target| target.id),
            Some("security.recovery")
        );
    }

    #[test]
    fn person_and_incoming_verification_search_reveal_one_live_control() {
        let all = |_| true;
        for query in [
            "verify person",
            "verify someone",
            "peer verification",
            "Matrix ID",
            "SAS comparison",
            "emoji comparison",
            "incoming verification",
            "incoming request",
            "received request",
        ] {
            assert_eq!(
                search_settings(query, all).first().map(|target| target.id),
                Some("security.peer_verification"),
                "{query}"
            );
        }
        let target = setting_target("security.peer_verification").unwrap();
        assert_eq!(target.route, Route::Settings);
        assert_eq!(target.requires, Requirement::LiveSession);
        let offline = |requirement| requirement == Requirement::Always;
        assert!(search_settings("peer verification", offline).is_empty());
        assert!(search_settings("incoming verification", offline).is_empty());
        // Own-device recovery retains its existing destination.
        assert_eq!(
            search_settings("recovery key", all)
                .first()
                .map(|target| target.id),
            Some("security.recovery")
        );
    }

    #[test]
    fn the_invitations_center_is_a_live_search_destination_that_answers_nothing() {
        let all = |_| true;
        let first = |query: &str| search_settings(query, all).first().map(|target| target.id);
        for query in ["invitations", "invited", "spam invites"] {
            assert_eq!(first(query), Some("rooms.invitations"), "{query}");
        }
        let target = setting_target("rooms.invitations").unwrap();
        assert_eq!(target.route, Route::Invitations);
        assert_eq!(target.requires, Requirement::LiveSession);
        let offline = |requirement| requirement == Requirement::Always;
        assert!(search_settings("invitations", offline).is_empty());
    }

    #[test]
    fn the_open_rooms_notification_panel_is_a_live_settings_destination() {
        let all = |_| true;
        let first = |query: &str| search_settings(query, all).first().map(|target| target.id);
        for query in ["bell", "quiet room", "room highlights", "this room"] {
            assert_eq!(first(query), Some("notifications.this_room"), "{query}");
        }
        // It only reveals a control, and only while signed in.
        let target = setting_target("notifications.this_room").unwrap();
        assert_eq!(target.route, Route::Settings);
        assert_eq!(target.requires, Requirement::LiveSession);
        let offline = |requirement| requirement == Requirement::Always;
        assert!(search_settings("bell", offline).is_empty());
    }

    #[test]
    fn single_color_tokens_are_direct_bounded_search_results() {
        let all = |_| true;
        let ids = |query: &str| {
            search(query, all)
                .into_iter()
                .map(SearchHit::id)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids("presence_online").first().map(String::as_str),
            Some("color:presence_online")
        );
        assert_eq!(
            ids("online").first().map(String::as_str),
            Some("color:presence_online")
        );
        assert!(ids("link").iter().any(|id| id == "color:link"));
        assert!(ids("scrim").iter().any(|id| id == "color:scrim"));
        // The generic Colors setting still leads for the general word.
        assert_eq!(
            ids("colour").first().map(String::as_str),
            Some("appearance.colors")
        );
        for query in ["e", "text", "background color", "a"] {
            assert!(search(query, all).len() <= MAX_SEARCH_RESULTS, "{query}");
        }
        assert!(search("", all).is_empty() && search(&"a".repeat(10_000), all).is_empty());
        // Every color result ID names exactly its registry token, and no other
        // text resolves.
        for token in COLOR_TOKENS {
            let id = SearchHit::Color(token).id();
            assert_eq!(color_result(&id).map(|found| found.id), Some(token.id));
        }
        for id in [
            "color:",
            "color:no_such_token",
            "link",
            "appearance.colors",
            "color:appearance.colors",
            "COLOR:link",
        ] {
            assert!(color_result(id).is_none(), "{id}");
        }
        // Setting results never collide with the color prefix.
        assert!(
            SETTING_TARGETS
                .iter()
                .all(|target| !target.id.starts_with(COLOR_RESULT_PREFIX))
        );
        // Availability still applies to settings and never hides a color.
        let offline = |requirement| requirement == Requirement::Always;
        assert!(search("recovery", offline).iter().all(|hit| match hit {
            SearchHit::Setting(target) => target.requires == Requirement::Always,
            SearchHit::Color(_) | SearchHit::Metric(..) => true,
        }));
    }

    #[test]
    fn metrics_are_direct_bounded_search_results_with_exact_ids() {
        let all = |_| true;
        let first = |query: &str| search(query, all).first().map(|hit| hit.id());
        for (query, id) in [
            ("focus width", "metric:geometry.focus_width"),
            ("avatar roundness", "metric:geometry.avatar_roundness"),
            ("icon size", "metric:geometry.icon_size"),
            ("composer_padding", "metric:geometry.composer_padding"),
            ("room list text size", "metric:typography.sidebar_size"),
        ] {
            assert_eq!(first(query).as_deref(), Some(id), "{query}");
        }
        // General words still lead with their setting or color.
        assert_eq!(first("colour").as_deref(), Some("appearance.colors"));
        assert_eq!(first("online").as_deref(), Some("color:presence_online"));
        for (field, editor) in metric_fields() {
            let id = SearchHit::Metric(field, editor).id();
            assert_eq!(
                metric_result(&id).map(|(found, _)| found.key),
                Some(field.key)
            );
            assert!(
                search(editor.label, all).iter().any(|hit| hit.id() == id),
                "{} is not found by its label",
                editor.label
            );
        }
        for id in [
            "metric:",
            "metric:geometry.corner_radius",
            "metric:colors.link",
            "geometry.focus_width",
            "METRIC:geometry.focus_width",
            "color:geometry.focus_width",
        ] {
            assert!(metric_result(id).is_none(), "{id}");
        }
        assert!(
            SETTING_TARGETS
                .iter()
                .all(|target| !target.id.starts_with(METRIC_RESULT_PREFIX))
        );
        for query in ["e", "size", "radius", "padding", "width"] {
            assert!(search(query, all).len() <= MAX_SEARCH_RESULTS, "{query}");
        }
        // Every metric field documents a range the validator reads.
        for (field, _) in metric_fields() {
            assert_eq!(integer_range(field.key), field.integer_range());
        }
        assert_eq!(integer_range("geometry.corner_radius"), Some((0, 24)));
        assert_eq!(integer_range("chat.layout"), None);
        assert_eq!(integer_range("no.such.field"), None);
    }

    #[test]
    fn account_results_open_a_dialog_without_focusing_removal() {
        let remove = setting_target("accounts.remove").unwrap();
        assert_eq!(remove.route, Route::ManageAccounts(AccountsFocus::Close));
        for target in SETTING_TARGETS {
            if let Route::ManageAccounts(focus) = target.route {
                assert!(matches!(
                    focus,
                    AccountsFocus::Close
                        | AccountsFocus::Add
                        | AccountsFocus::Protection
                        | AccountsFocus::Profile
                ));
            }
        }
    }
}

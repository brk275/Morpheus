//! Offline Unicode 17.0 picker data from the exactly pinned `emojis` crate.
//! Includes every canonical entry and every supported skin-tone combination.
//! Starter keys/order and familiar aliases remain available; other opaque keys
//! are canonical Unicode strings. The unchanged picker accepts a key only from
//! its current rows and inserts catalog text through its scoped literal path.

use std::{collections::HashSet, sync::OnceLock};

pub(super) struct Emoji {
    pub key: &'static str,
    pub text: &'static str,
    pub name: &'static str,
    search_words: String,
}

struct Starter {
    key: &'static str,
    text: &'static str,
    name: &'static str,
    words: &'static str,
}

pub(super) const MAX_QUERY_BYTES: usize = 64;
const MAX_CATALOG_ENTRIES: usize = 4096;
const MAX_EMOJI_BYTES: usize = 64;
const MAX_SEARCH_BYTES: usize = 1024;

macro_rules! starters {
    ($(($key:literal, $text:literal, $name:literal, $words:literal)),* $(,)?) => {
        const STARTERS: &[Starter] = &[$(Starter { key: $key, text: $text, name: $name, words: $words }),*];
    };
}
starters! {
    ("wave", "👋", "Waving hand", "hello goodbye hi"),
    ("smile", "😀", "Grinning face", "happy smile cheerful"),
    ("laugh", "😄", "Laughing face", "happy fun smile"),
    ("joy", "😂", "Tears of joy", "laugh funny lol"),
    ("warm-smile", "😊", "Warm smile", "happy kind blush"),
    ("wink", "😉", "Winking face", "playful"),
    ("love", "😍", "Heart eyes", "love happy"),
    ("think", "🤔", "Thinking face", "question consider hmm"),
    ("surprise", "😮", "Surprised face", "wow gasp"),
    ("sad", "😢", "Crying face", "sad tear"),
    ("laugh-sweat", "😅", "Smiling with sweat", "relief nervous"),
    ("sleep", "😴", "Sleeping face", "tired rest"),
    ("cool", "😎", "Sunglasses face", "cool sun"),
    ("party", "🥳", "Party face", "celebrate birthday"),
    ("eyes", "👀", "Eyes", "look watch"),
    ("thumbs-up", "👍", "Thumbs up", "yes okay good approve"),
    ("thumbs-down", "👎", "Thumbs down", "no disagree"),
    ("clap", "👏", "Clapping hands", "applause well done"),
    ("raised-hands", "🙌", "Raised hands", "celebrate hooray"),
    ("handshake", "🤝", "Handshake", "agree partnership"),
    ("heart", "❤️", "Red heart", "love thanks"),
    ("purple-heart", "💜", "Purple heart", "love violet"),
    ("green-heart", "💚", "Green heart", "love mint"),
    ("sparkles", "✨", "Sparkles", "magic shine"),
    ("star", "⭐", "Star", "favorite bright"),
    ("fire", "🔥", "Fire", "hot excellent"),
    ("confetti", "🎉", "Confetti", "celebrate congratulations party"),
    ("gift", "🎁", "Gift", "birthday present"),
    ("check", "✅", "Check mark", "done yes complete"),
    ("cross", "❌", "Cross mark", "no cancel"),
    ("warning", "⚠️", "Warning", "attention caution"),
    ("question", "❓", "Question mark", "help ask"),
    ("bulb", "💡", "Light bulb", "idea inspiration"),
    ("rocket", "🚀", "Rocket", "launch space"),
    ("tools", "🛠️", "Tools", "build repair work"),
    ("computer", "💻", "Laptop", "computer code work"),
    ("coffee", "☕", "Coffee", "drink break tea"),
    ("pizza", "🍕", "Pizza", "food lunch dinner"),
    ("cat", "🐈", "Cat", "pet animal"),
    ("dog", "🐕", "Dog", "pet animal"),
    ("leaf", "🌿", "Leaf", "nature plant green"),
    ("sun", "☀️", "Sun", "weather bright day"),
    ("moon", "🌙", "Crescent moon", "night sleep"),
    ("rainbow", "🌈", "Rainbow", "color weather"),
}

fn normalized_words(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .map(|ch| {
            if matches!(ch, '_' | '-' | ':') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn entry(value: &'static emojis::Emoji, base: &'static emojis::Emoji) -> Emoji {
    let text = value.as_str();
    let starter = STARTERS
        .iter()
        .find(|starter| starter.text == base.as_str());
    let original = starter.filter(|starter| starter.text == text);
    let mut words = String::from(value.name());
    // Toned variants inherit the root's shortcodes and our existing aliases.
    // Their own CLDR name retains the specific (including mixed) skin tones.
    for shortcode in value.shortcodes().chain(base.shortcodes()) {
        words.push(' ');
        words.push_str(shortcode);
    }
    if let Some(starter) = starter {
        for aliases in [starter.key, starter.name, starter.words] {
            words.push(' ');
            words.push_str(aliases);
        }
    }
    let search_words = normalized_words(&words);
    assert!(text.len() <= MAX_EMOJI_BYTES && search_words.len() <= MAX_SEARCH_BYTES);
    Emoji {
        key: original.map_or(text, |starter| starter.key),
        text,
        name: original.map_or_else(|| value.name(), |starter| starter.name),
        search_words,
    }
}

fn catalog() -> &'static [Emoji] {
    static CATALOG: OnceLock<Vec<Emoji>> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut entries = Vec::with_capacity(MAX_CATALOG_ENTRIES);
        let mut seen = HashSet::with_capacity(MAX_CATALOG_ENTRIES);
        let mut push = |value: &'static emojis::Emoji, base: &'static emojis::Emoji| {
            if seen.insert(value.as_str()) {
                // These limits constrain the pinned, compiled data; no remote
                // or user-supplied text enters this process-lifetime cache.
                assert!(entries.len() < MAX_CATALOG_ENTRIES);
                entries.push(entry(value, base));
            }
        };
        for starter in STARTERS {
            let value = emojis::get(starter.text).expect("pinned starter emoji exists");
            push(value, value);
        }
        for base in emojis::iter() {
            if let Some(tones) = base.skin_tones() {
                for value in tones {
                    push(value, base);
                }
            } else {
                push(base, base);
            }
        }
        entries
    })
}

pub(super) fn search(query: &str) -> Vec<&'static Emoji> {
    if query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
        return Vec::new();
    }
    let trimmed = query.trim();
    // Resolve qualified/unqualified glyph searches to one canonical row; a
    // literal tone modifier must not accidentally select the default tone.
    if let Some(exact) = emojis::get(trimmed) {
        return catalog()
            .iter()
            .filter(|entry| entry.text == exact.as_str())
            .collect();
    }
    let query = normalized_words(trimmed);
    catalog()
        .iter()
        .filter(|entry| {
            query
                .split_whitespace()
                .all(|part| entry.search_words.contains(part))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_catalog_covers_every_root_and_tone_once_with_bounded_literal_keys() {
        assert_eq!(emojis::UNICODE_VERSION, emojis::UnicodeVersion::new(17, 0));
        let mut expected = std::collections::BTreeSet::new();
        for base in emojis::iter() {
            expected.insert(base.as_str());
            if let Some(tones) = base.skin_tones() {
                expected.extend(tones.map(emojis::Emoji::as_str));
            }
        }
        let all = search("");
        let actual = all
            .iter()
            .map(|entry| entry.text)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(expected, actual);
        assert_eq!(all.len(), 3944);
        assert!(all.len() <= MAX_CATALOG_ENTRIES);
        assert_eq!(all.len(), actual.len());
        let mut keys = std::collections::BTreeSet::new();
        for entry in all {
            assert!(keys.insert(entry.key));
            assert!(entry.text.len() <= MAX_EMOJI_BYTES);
            assert!(!entry.text.contains('@') && !entry.text.chars().any(char::is_control));
            assert!(entry.search_words.len() <= MAX_SEARCH_BYTES);
        }
    }

    #[test]
    fn starter_order_keys_and_familiar_aliases_are_preserved() {
        let all = search("");
        for (starter, entry) in STARTERS.iter().zip(&all) {
            assert_eq!(
                (entry.key, entry.text, entry.name),
                (starter.key, starter.text, starter.name)
            );
            for alias in starter.words.split_whitespace().chain([starter.key]) {
                assert!(search(alias).iter().any(|found| found.key == starter.key));
            }
        }
        assert_eq!(search("purple love")[0].key, "purple-heart");
        assert_eq!(search("👋")[0].key, "wave");
    }

    #[test]
    fn search_finds_full_catalog_shortcodes_groups_and_new_unicode_entries() {
        for (query, glyph) in [(":t_rex:", "🦖"), ("flag japan", "🇯🇵"), ("orca", "🫍")] {
            assert!(
                search(query).iter().any(|entry| entry.text == glyph),
                "{query}"
            );
        }
        assert!(
            search("ballet dancer")
                .iter()
                .any(|entry| entry.name == "ballet dancer")
        );
        assert_eq!(search("purple love").len(), 1);
    }

    #[test]
    fn tone_aliases_and_mixed_tones_remain_individually_searchable() {
        let waving = search("wave");
        for value in emojis::get("👋").unwrap().skin_tones().unwrap() {
            assert!(waving.iter().any(|entry| entry.text == value.as_str()));
        }
        assert!(
            search("hello dark skin")
                .iter()
                .any(|entry| entry.text == "👋🏿")
        );
        let mixed = emojis::get("🤝")
            .unwrap()
            .with_skin_tone(emojis::SkinTone::LightAndDark)
            .unwrap();
        assert!(
            search("handshake light dark")
                .iter()
                .any(|entry| entry.text == mixed.as_str())
        );
        assert_eq!(
            search("👋🏿")
                .iter()
                .map(|entry| entry.text)
                .collect::<Vec<_>>(),
            ["👋🏿"]
        );
        assert_eq!(search("❤")[0].text, "❤️");
    }

    #[test]
    fn malformed_queries_are_rejected_and_unrelated_terms_do_not_match() {
        assert!(search(&"x".repeat(MAX_QUERY_BYTES + 1)).is_empty());
        assert!(search("hello\nwave").is_empty());
        assert!(search("hello\0wave").is_empty());
        assert!(search("rocket nonexistentalias").is_empty());
        assert!(search("@room").is_empty());
        assert_eq!(
            search("  THUMBS_UP  ")
                .iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>(),
            search("thumbs-up")
                .iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>()
        );
    }
}

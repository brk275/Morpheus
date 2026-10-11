//! Small original picker catalog: Unicode text only, no images or network.
//! The stable ASCII key selects a catalog entry; callers cannot submit arbitrary
//! text or mention intent through the picker callback.

pub(super) struct Emoji {
    pub key: &'static str,
    pub text: &'static str,
    pub name: &'static str,
    words: &'static str,
}

pub(super) const MAX_QUERY_BYTES: usize = 64;
macro_rules! catalog {
    ($(($key:literal, $text:literal, $name:literal, $words:literal)),* $(,)?) => {
        pub(super) const CATALOG: &[Emoji] = &[$(Emoji { key: $key, text: $text, name: $name, words: $words }),*];
    };
}
catalog! {
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

pub(super) fn search(query: &str) -> Vec<&'static Emoji> {
    if query.len() > MAX_QUERY_BYTES || query.chars().any(char::is_control) {
        return Vec::new();
    }
    let query = query.trim().to_lowercase();
    CATALOG
        .iter()
        .filter(|entry| {
            query.split_whitespace().all(|part| {
                entry.key.contains(part)
                    || entry.name.to_lowercase().contains(part)
                    || entry.words.contains(part)
                    || entry.text == part
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_is_bounded_unique_literal_text_and_search_matches_all_terms() {
        assert!(CATALOG.len() <= 48);
        let mut keys = std::collections::BTreeSet::new();
        for entry in CATALOG {
            assert!(keys.insert(entry.key));
            assert!(
                entry
                    .key
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'-')
            );
            assert!(!entry.text.is_empty() && entry.text.len() <= 16);
            assert!(!entry.text.contains('@') && !entry.text.chars().any(char::is_control));
        }
        assert_eq!(
            search("purple love")
                .iter()
                .map(|entry| entry.key)
                .collect::<Vec<_>>(),
            vec!["purple-heart"]
        );
        assert!(search(&"x".repeat(MAX_QUERY_BYTES + 1)).is_empty());
        assert!(search("bad\nquery").is_empty());
        assert_eq!(search("👋")[0].key, "wave");
    }
}

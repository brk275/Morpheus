use super::*;

fn item(id: &str, kind: RoomMediaKind, name: &str) -> RoomMediaItem {
    RoomMediaItem {
        event_id: id.to_owned(),
        kind,
        filename: name.to_owned(),
        mime_type: String::new(),
        size: None,
        timestamp_ms: 0,
        sender_id: "@ana:example.org".to_owned(),
        sender_name: "Ana".to_owned(),
        thread_root: None,
        encrypted: false,
        authenticity_warning: false,
    }
}

fn page(items: Vec<RoomMediaItem>, examined: usize, next: Option<&str>) -> RoomMediaPage {
    RoomMediaPage {
        room_id: "!media:example.org".to_owned(),
        items,
        next: next.map(str::to_owned),
        examined,
        ..RoomMediaPage::default()
    }
}

#[test]
fn a_summary_never_claims_more_history_than_was_read() {
    let mut loaded = Loaded::default();
    assert_eq!(summary(&loaded, 0, false), "");
    loaded.merge(page(Vec::new(), 300, Some("t1")));
    let text = summary(&loaded, 0, false);
    let none = "No readable media items were found in the 300 messages read so far.";
    assert!(text.contains(none), "{text}");
    assert!(text.contains("Older media is not loaded yet."));
    assert!(!text.contains("start of the history"));
    // Only the server's start makes it complete.
    let mut start = page(vec![item("$a:x", RoomMediaKind::Image, "a.png")], 1, None);
    start.reached_start = true;
    loaded.merge(start);
    let text = summary(&loaded, 1, false);
    assert!(text.starts_with("1 item from the 301 messages"), "{text}");
    let start = "start of the history this account can see";
    assert!(text.contains(start), "{text}");
    // Paging that cannot continue is not the start.
    let mut stuck = Loaded::default();
    let mut cut = page(Vec::new(), 50, None);
    cut.truncated = true;
    stuck.merge(cut);
    let text = summary(&stuck, 0, false);
    assert!(text.contains("incomplete"), "{text}");
    assert!(!text.contains("start of the history"));
    assert!(!stuck.can_load_older());
}

#[test]
fn a_search_says_it_looked_at_loaded_items_only() {
    let mut loaded = Loaded::default();
    let items = vec![
        item("$a:x", RoomMediaKind::Image, "beach.png"),
        item("$b:x", RoomMediaKind::File, "notes.pdf"),
    ];
    loaded.merge(page(items, 120, Some("t1")));
    let text = summary(&loaded, 1, true);
    let matched = "1 of 2 loaded items match. Only loaded items are searched.";
    assert!(text.starts_with(matched), "{text}");
    assert!(text.contains("Older media is not loaded yet."));
}

#[test]
fn undecryptable_messages_are_counted_and_said_to_hide_media() {
    let mut loaded = Loaded::default();
    let mut read = page(Vec::new(), 40, Some("t1"));
    read.undecryptable = 3;
    read.room_encrypted = true;
    loaded.merge(read);
    let text = summary(&loaded, 0, false);
    let unread = "3 encrypted messages could not be decrypted on this device";
    assert!(text.contains(unread), "{text}");
    assert!(text.contains("any media in them is not listed"));
    assert!(loaded.room_encrypted);
}

#[test]
fn partial_and_unreadable_coverage_survives_later_pages() {
    let mut loaded = Loaded::default();
    let mut cut = page(Vec::new(), 50, Some("unsafe-tail"));
    cut.truncated = true;
    cut.skipped = 2;
    loaded.merge(cut);
    assert!(loaded.truncated);
    assert_eq!(loaded.next, None);
    assert!(!loaded.can_load_older());
    let mut end = page(Vec::new(), 1, None);
    end.reached_start = true;
    loaded.merge(end);
    let text = summary(&loaded, 0, false);
    assert!(loaded.truncated && !loaded.reached_start);
    assert_eq!(loaded.skipped, 2);
    assert!(text.contains("No readable media items"), "{text}");
    assert!(text.contains("2 messages could not be read"), "{text}");
    assert!(text.contains("incomplete"), "{text}");
    assert!(!text.contains("start of the history"), "{text}");
}

#[test]
fn an_unreadable_final_message_is_not_claimed_to_have_no_media() {
    let mut loaded = Loaded::default();
    let mut end = page(Vec::new(), 1, None);
    end.reached_start = true;
    end.skipped = 1;
    loaded.merge(end);
    let text = summary(&loaded, 0, false);
    assert!(text.contains("No readable media items"), "{text}");
    assert!(text.contains("1 message could not be read"), "{text}");
    assert!(text.contains("any media in them is not listed"), "{text}");
    assert!(!text.contains("No images"), "{text}");
}

#[test]
fn the_final_load_gets_only_the_remaining_event_budget() {
    let mut loaded = Loaded::default();
    loaded.merge(page(Vec::new(), MAX_EXAMINED - 1, Some("last")));
    assert_eq!(loaded.remaining_events(), 1);
    assert!(loaded.can_load_older());
    loaded.merge(page(Vec::new(), 1, Some("older")));
    assert_eq!(loaded.examined, MAX_EXAMINED);
    assert_eq!(loaded.remaining_events(), 0);
    assert!(loaded.events_capped && !loaded.items_capped);
    assert!(!loaded.can_load_older());
}

#[test]
fn failed_or_discarded_loads_cannot_bypass_the_event_budget() {
    let mut loaded = Loaded::default();
    loaded.merge(page(Vec::new(), 1, Some("older")));
    while loaded.remaining_events() > 0 {
        let allowance = loaded
            .remaining_events()
            .min(MEDIA_LOAD_REQUESTS * MEDIA_PAGE_EVENTS as usize);
        loaded.spend_budget(allowance);
    }
    assert_eq!(
        loaded.examined, 1,
        "failed loads never pretend to be readable messages"
    );
    assert!(loaded.events_capped && !loaded.items_capped);
    assert!(!loaded.can_load_older());
    let text = summary(&loaded, 0, false);
    assert!(text.contains("read budget of 6,000 messages"), "{text}");
    assert!(!text.contains("1,000 items"), "{text}");
    assert!(text.contains("message count excludes reads"), "{text}");
    let mut only_errors = Loaded::default();
    only_errors.spend_budget(MAX_EXAMINED);
    let text = summary(&only_errors, 0, false);
    assert!(text.contains("read budget of 6,000 messages"), "{text}");
}

#[test]
fn loads_merge_each_event_once_within_the_browsers_bounds() {
    let mut loaded = Loaded::default();
    let first = vec![
        item("$a:x", RoomMediaKind::Image, "a.png"),
        item("$b:x", RoomMediaKind::Video, "b.mp4"),
    ];
    loaded.merge(page(first, 100, Some("t1")));
    // A later load repeating an event keeps one row for it.
    let again = vec![
        item("$b:x", RoomMediaKind::Video, "b.mp4"),
        item("$c:x", RoomMediaKind::File, "c.txt"),
    ];
    loaded.merge(page(again, 100, Some("t2")));
    let ids: Vec<_> = loaded
        .items
        .iter()
        .map(|item| item.event_id.as_str())
        .collect();
    assert_eq!(ids, ["$a:x", "$b:x", "$c:x"]);
    assert_eq!(loaded.examined, 200);
    assert!(loaded.can_load_older());
    // The item bound ends the list and Load older.
    let mut many = Vec::new();
    for index in 0..MAX_LOADED_ITEMS {
        many.push(item(&format!("$m{index}:x"), RoomMediaKind::Image, "m.png"));
    }
    loaded.merge(page(many, 100, Some("t3")));
    assert_eq!(loaded.items.len(), MAX_LOADED_ITEMS);
    assert!(loaded.capped());
    assert!(!loaded.can_load_older());
    assert!(summary(&loaded, 0, false).contains("limit of 1,000 items"));
    // So does the bound on events looked at.
    let mut read = Loaded::default();
    read.merge(page(Vec::new(), MAX_EXAMINED, Some("t1")));
    assert!(read.capped());
    assert!(!read.can_load_older());
    assert!(summary(&read, 0, false).contains("read budget of 6,000 messages"));
    assert!(!summary(&read, 0, false).contains("1,000 items"));
}

#[test]
fn search_and_kind_filter_only_loaded_fields() {
    let mut photo = item("$a:x", RoomMediaKind::Image, "Beach.PNG");
    photo.mime_type = "image/png".to_owned();
    let notes = item("$b:x", RoomMediaKind::File, "notes.pdf");
    let query = needle("  BEACH ");
    assert_eq!(query, "beach");
    assert!(matches(&photo, &query, 0));
    assert!(!matches(&notes, &query, 0));
    // The sender's name and the declared type are searched too.
    assert!(matches(&photo, "ana", 1));
    assert!(matches(&photo, "image/png", 0));
    // A kind filter keeps its kind only; an unknown one keeps everything.
    assert!(!matches(&photo, "", 4));
    assert!(matches(&notes, "", 4));
    assert!(matches(&notes, "", 9));
    let long = "q".repeat(1000);
    assert_eq!(needle(&long).chars().count(), MAX_QUERY_CHARS);
}

#[test]
fn rows_show_declared_metadata_as_plain_text_and_view_follows_the_preview_setting() {
    let mut photo = item("$a:x", RoomMediaKind::Image, "beach\u{202e}gnp.exe");
    photo.size = Some(2_048);
    photo.mime_type = "image/png".to_owned();
    photo.thread_root = Some("$root:x".to_owned());
    photo.authenticity_warning = true;
    let shown = row(&photo, true);
    assert_eq!(shown.id.as_str(), "$a:x");
    assert_eq!(shown.kind, 0);
    assert!(!shown.name.as_str().contains('\u{202e}'));
    assert_eq!(shown.detail.as_str(), "Image · 2.0 KB · image/png");
    assert_eq!(shown.origin.as_str(), "Ana");
    assert!(shown.thread);
    assert!(shown.warning);
    assert!(shown.can_view);
    // An encrypted room's image opens only with encrypted previews on.
    assert!(can_view(&photo, false, false));
    assert!(can_view(&photo, true, true));
    assert!(!can_view(&photo, true, false));
    let clip = item("$v:x", RoomMediaKind::Video, "clip.mp4");
    // Only images open in the viewer.
    assert!(!can_view(&clip, false, true));
    assert_eq!(row(&clip, false).detail.as_str(), "Video");
}

#[test]
fn sizes_read_in_decimal_units() {
    assert_eq!(size_text(999), "999 B");
    assert_eq!(size_text(2_048), "2.0 KB");
    assert_eq!(size_text(5_300_000), "5.3 MB");
    assert_eq!(size_text(7_000_000_000_000), "7000.0 GB");
}

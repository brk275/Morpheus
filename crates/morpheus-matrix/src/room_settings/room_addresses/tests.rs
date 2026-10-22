//! Regression source for room addresses: what may be added (a local part on
//! the account's own server only), how the published addresses are read
//! from the exact stored text and edited without losing what Morpheus does
//! not edit, how the known addresses merge, and that a change handed over
//! without a definite answer stays unknown.

use matrix_sdk::{
    HttpError,
    ruma::api::{
        client::uiaa::UiaaResponse,
        error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
    },
};
use serde_json::json;

use super::*;

fn api_error(status: u16, errcode: &str) -> HttpError {
    let body: StandardErrorBody = serde_json::from_value(json!({
        "errcode": errcode, "error": "private reflected response"
    }))
    .unwrap();
    HttpError::Api(Box::new(FromHttpResponseError::Server(
        UiaaResponse::MatrixError(Error::new(
            reqwest::StatusCode::from_u16(status).unwrap(),
            ErrorBody::Standard(body),
        )),
    )))
}

fn server(name: &str) -> OwnedServerName {
    OwnedServerName::try_from(name).unwrap()
}

#[test]
fn a_new_address_is_a_local_part_on_the_accounts_own_server() {
    let own = server("example.org");
    assert_eq!(
        local_alias("Lobby", &own).unwrap().as_str(),
        "#lobby:example.org"
    );
    assert_eq!(
        local_alias("  #team.notes ", &own).unwrap().as_str(),
        "#team.notes:example.org"
    );
    // Never another server, a URL, a Matrix ID of something else, or
    // characters outside the local part's set.
    let long = "x".repeat(MAX_ALIAS_LOCALPART_CHARS + 1);
    for refused in [
        "",
        "lobby:other.org",
        "https://x.org",
        "a b",
        "@user",
        long.as_str(),
    ] {
        assert_eq!(
            local_alias(refused, &own),
            Err(RoomAddressError::InvalidAlias),
            "{refused}"
        );
    }
}

#[test]
fn published_addresses_are_read_from_the_exact_text_or_left_read_only() {
    assert_eq!(
        published(None),
        Published {
            supported: true,
            ..Published::default()
        }
    );
    let stored = r##"{"alias":"#main:example.org","alt_aliases":["#alt:other.org"],"org.x":1.10}"##;
    let parsed = published(Some(stored));
    assert!(parsed.supported);
    assert_eq!(parsed.main.as_deref(), Some("#main:example.org"));
    assert_eq!(parsed.alternatives, ["#alt:other.org"]);
    // An empty or null main is none.
    assert!(published(Some(r#"{"alias":""}"#)).main.is_none());
    assert!(published(Some(r#"{"alias":null}"#)).supported);
    // Unsupported: not an alias, not a list, a name twice, not an object.
    for unsupported in [
        r#"{"alias":"lobby"}"#,
        r#"{"alias":7}"#,
        r##"{"alt_aliases":"#a:x.org"}"##,
        r##"{"alt_aliases":["#a:x.org","nope"]}"##,
        r##"{"alias":"#a:x.org","alias":"#b:x.org"}"##,
        "[]",
    ] {
        assert!(!published(Some(unsupported)).supported, "{unsupported}");
    }
}

#[test]
fn an_edit_of_the_published_addresses_keeps_every_other_field_byte_for_byte() {
    let stored = r##"{"alias":"#main:example.org","org.example.big":184467440737095516160,"org.example.rate":1.10,"alt_aliases":["#alt:example.org"]}"##;
    let parsed = published(Some(stored));
    // Clearing the main address keeps the alternatives and the unknown
    // fields as they were stored, and drops `alias`.
    let (main, alternatives, added) = edited(&parsed, &PublishEdit::ClearMain).unwrap();
    assert!(main.is_none() && added.is_none());
    let mut typed = RoomCanonicalAliasEventContent::new();
    typed.alt_aliases = alternatives
        .iter()
        .map(|a| parse_alias(a).unwrap())
        .collect();
    let text = edited_content(Some(stored), &["alias", "alt_aliases"], typed).unwrap();
    let text = text.get();
    assert!(
        text.contains("184467440737095516160") && text.contains("1.10"),
        "{text}"
    );
    assert!(!text.contains("\"alias\""), "{text}");
    assert!(text.contains("#alt:example.org"), "{text}");
}

#[test]
fn edits_say_which_address_they_newly_publish() {
    let parsed = Published {
        main: Some("#main:example.org".into()),
        alternatives: vec!["#alt:example.org".into()],
        supported: true,
    };
    // A new main: the old one stays published, as an alternative, and the
    // new one must point here.
    let (main, alternatives, added) =
        edited(&parsed, &PublishEdit::SetMain("#new:other.org".into())).unwrap();
    assert_eq!(main.as_deref(), Some("#new:other.org"));
    assert_eq!(alternatives, ["#alt:example.org", "#main:example.org"]);
    assert_eq!(added.as_deref(), Some("#new:other.org"));
    // Promoting an alternative still rechecks its current mapping.
    let (main, alternatives, added) =
        edited(&parsed, &PublishEdit::SetMain("#alt:example.org".into())).unwrap();
    assert_eq!(main.as_deref(), Some("#alt:example.org"));
    assert_eq!(alternatives, ["#main:example.org"]);
    assert_eq!(added.as_deref(), Some("#alt:example.org"));
    let (_, alternatives, added) =
        edited(&parsed, &PublishEdit::AddAlternative("#more:x.org".into())).unwrap();
    assert_eq!(alternatives.len(), 2);
    assert_eq!(added.as_deref(), Some("#more:x.org"));
    let (_, alternatives, _) = edited(
        &parsed,
        &PublishEdit::RemoveAlternative("#alt:example.org".into()),
    )
    .unwrap();
    assert!(alternatives.is_empty());
    assert_eq!(
        edited(&parsed, &PublishEdit::AddAlternative("not an alias".into())),
        Err(RoomAddressError::InvalidAlias)
    );
    // Bounded.
    let full = Published {
        main: None,
        alternatives: (0..MAX_ALTERNATIVE_ADDRESSES)
            .map(|n| format!("#a{n}:x.org"))
            .collect(),
        supported: true,
    };
    assert_eq!(
        edited(&full, &PublishEdit::AddAlternative("#one:x.org".into())),
        Err(RoomAddressError::Unsupported)
    );
    // A stored list already past the bound may still shrink.
    let over = Published {
        main: None,
        alternatives: (0..=MAX_ALTERNATIVE_ADDRESSES + 1)
            .map(|n| format!("#a{n}:x.org"))
            .collect(),
        supported: true,
    };
    let (_, alternatives, added) =
        edited(&over, &PublishEdit::RemoveAlternative("#a0:x.org".into())).unwrap();
    assert_eq!(alternatives.len(), MAX_ALTERNATIVE_ADDRESSES + 1);
    assert_eq!(added, None);
}

#[test]
fn known_addresses_merge_once_with_their_ownership_and_order() {
    let parsed = Published {
        main: Some("#main:other.org".into()),
        alternatives: vec!["#alt:example.org".into()],
        supported: true,
    };
    let listed = [
        "#b:example.org".to_owned(),
        "#alt:example.org".to_owned(),
        "junk".to_owned(),
    ];
    let (addresses, truncated) = merged(&listed, &parsed, "example.org");
    assert!(!truncated);
    let aliases = addresses
        .iter()
        .map(|a| a.alias.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        aliases,
        ["#main:other.org", "#alt:example.org", "#b:example.org"]
    );
    // Remote: published only, never removable here.
    assert!(addresses[0].main && !addresses[0].local && !addresses[0].listed);
    assert!(addresses[1].alternative && addresses[1].local && addresses[1].listed);
    assert!(addresses[2].local && addresses[2].listed && !addresses[2].main);
    let many = (0..=MAX_ROOM_ADDRESSES)
        .map(|n| format!("#r{n}:example.org"))
        .collect::<Vec<_>>();
    let (addresses, truncated) = merged(&many, &Published::default(), "example.org");
    assert!(truncated && addresses.len() == MAX_ROOM_ADDRESSES);
}

#[test]
fn a_change_handed_over_without_a_definite_answer_stays_unknown() {
    // Before the hand-off an error is what it is; after it, unknown; once
    // applied, a failed read-back says it was saved.
    assert_eq!(
        outcome(0, RoomAddressError::Cancelled),
        RoomAddressError::Cancelled
    );
    assert_eq!(
        outcome(SENT, RoomAddressError::Cancelled),
        RoomAddressError::Unknown
    );
    assert_eq!(
        outcome(SENT, RoomAddressError::TimedOut),
        RoomAddressError::Unknown
    );
    assert_eq!(
        outcome(APPLIED, RoomAddressError::Cancelled),
        RoomAddressError::SavedNotRead
    );
    assert_eq!(
        outcome(REFUSED, RoomAddressError::AliasTaken),
        RoomAddressError::AliasTaken
    );
    assert!(RoomAddressError::Unknown.may_have_changed());
    assert!(!RoomAddressError::Refused.may_have_changed());
    // Definite 4xx answers are fixed categories without the server's text.
    for (status, errcode, change, expected) in [
        (409, "M_UNKNOWN", Change::Add, RoomAddressError::AliasTaken),
        (
            404,
            "M_NOT_FOUND",
            Change::Remove,
            RoomAddressError::NotFound,
        ),
        (
            403,
            "M_FORBIDDEN",
            Change::Remove,
            RoomAddressError::PermissionDenied,
        ),
        (
            400,
            "M_BAD_ALIAS",
            Change::Publish,
            RoomAddressError::MappingChanged,
        ),
        (
            400,
            "M_BAD_JSON",
            Change::Publish,
            RoomAddressError::Refused,
        ),
        (
            429,
            "M_LIMIT_EXCEEDED",
            Change::Add,
            RoomAddressError::RateLimited,
        ),
        (
            400,
            "M_INVALID_PARAM",
            Change::Add,
            RoomAddressError::Refused,
        ),
    ] {
        let refused = answer_refusal(&api_error(status, errcode), change);
        assert_eq!(refused, Some(expected), "{status} {errcode}");
        assert!(!expected.to_string().contains("private"));
    }
    // A server error or no answer may have applied.
    assert_eq!(
        answer_refusal(&api_error(502, "M_UNKNOWN"), Change::Remove),
        None
    );
}

#[test]
fn address_reads_are_feature_local_single_attempt_without_auth_refresh() {
    let config = format!("{:?}", address_read_config());
    assert!(config.contains("retry_limit: 0"));
    assert!(config.contains("max_response_bytes: 65536"));
    assert!(config.contains("disable_auth_refresh: true"));
    assert_eq!(
        read_failure(api_error(401, "M_UNKNOWN_TOKEN")),
        RoomAddressError::SignInExpired
    );
}

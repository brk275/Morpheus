//! Regression source for room and Space creation: validation, the exact
//! request a plan sends, encryption defaults, and which failures may have
//! created a room.

use matrix_sdk::ruma::api::{
    client::uiaa::UiaaResponse,
    error::{Error, ErrorBody, FromHttpResponseError, StandardErrorBody},
};
use serde_json::{Value, json};

use super::*;

const SPACE: &str = "!team:example.org";

fn context(parent: Option<bool>) -> CreationContext {
    CreationContext {
        user_id: "@alice:example.org".into(),
        server_name: "example.org".into(),
        room_version: Some("10".into()),
        space_members_supported: true,
        parent: parent.map(|can_link| ParentContext {
            space_id: SPACE.into(),
            name: "Team".into(),
            can_link,
        }),
    }
}

fn choices(kind: CreationKind, name: &str) -> CreationChoices {
    let mut choices = CreationChoices::new(kind, None);
    choices.name = name.into();
    choices
}

/// The initial state of a request, by event type.
fn initial_state(request: &create_room::v3::Request) -> Vec<Value> {
    request
        .initial_state
        .iter()
        .map(|raw| serde_json::from_str(raw.json().get()).unwrap())
        .collect()
}

fn of_type<'a>(state: &'a [Value], kind: &str) -> Vec<&'a Value> {
    state.iter().filter(|event| event["type"] == kind).collect()
}

#[test]
fn rooms_start_encrypted_and_invite_only_and_spaces_are_never_encrypted() {
    let room = CreationChoices::new(CreationKind::Room, None);
    assert!(room.encrypted);
    assert_eq!(room.access, CreationAccess::InviteOnly);
    assert!(!room.listed);
    let space = CreationChoices::new(CreationKind::Space, None);
    assert!(!space.encrypted);

    let plan = CreationPlan::new(&choices(CreationKind::Room, "Design"), &context(None)).unwrap();
    let request = plan.request().unwrap();
    let state = initial_state(&request);
    let encryption = of_type(&state, "m.room.encryption");
    assert_eq!(encryption.len(), 1);
    assert_eq!(
        encryption[0]["content"]["algorithm"],
        "m.megolm.v1.aes-sha2"
    );
    assert_eq!(
        of_type(&state, "m.room.join_rules")[0]["content"]["join_rule"],
        "invite"
    );
    assert_eq!(
        of_type(&state, "m.room.guest_access")[0]["content"]["guest_access"],
        "forbidden"
    );
    assert_eq!(
        of_type(&state, "m.room.history_visibility")[0]["content"]["history_visibility"],
        "shared"
    );
    assert_eq!(request.visibility, Visibility::Private);
    assert!(request.creation_content.is_none());

    // A Space asked to be encrypted is refused, never quietly encrypted.
    let mut space = choices(CreationKind::Space, "Team");
    space.encrypted = true;
    assert_eq!(
        CreationPlan::new(&space, &context(None)),
        Err(CreationError::EncryptedSpace)
    );
    space.encrypted = false;
    let request = CreationPlan::new(&space, &context(None))
        .unwrap()
        .request()
        .unwrap();
    assert!(of_type(&initial_state(&request), "m.room.encryption").is_empty());
    let creation: Value =
        serde_json::from_str(request.creation_content.as_ref().unwrap().json().get()).unwrap();
    assert_eq!(creation["type"], "m.space");

    // An unencrypted room sends nothing about encryption: no event can turn
    // a homeserver's own default off.
    let mut open = choices(CreationKind::Room, "Lobby");
    open.encrypted = false;
    let plan = CreationPlan::new(&open, &context(None)).unwrap();
    assert!(
        of_type(
            &initial_state(&plan.request().unwrap()),
            "m.room.encryption"
        )
        .is_empty()
    );
    let messages = plan
        .preview()
        .into_iter()
        .find(|line| line.label == "Messages")
        .unwrap();
    assert!(messages.value.contains("may still turn encryption on"));
}

#[test]
fn names_topics_and_addresses_are_validated() {
    let plan = |name: &str, topic: &str, alias: &str| {
        let mut choices = choices(CreationKind::Room, name);
        choices.topic = topic.into();
        choices.alias = alias.into();
        CreationPlan::new(&choices, &context(None))
    };
    assert_eq!(plan("  ", "", ""), Err(CreationError::NameMissing));
    assert_eq!(
        plan(&"n".repeat(256), "", ""),
        Err(CreationError::NameTooLong)
    );
    assert_eq!(plan("Two\nlines", "", ""), Err(CreationError::NameInvalid));
    assert_eq!(
        plan("Ok", &"t".repeat(4097), ""),
        Err(CreationError::TopicTooLong)
    );
    assert_eq!(
        plan("Ok", "bell\u{7}", ""),
        Err(CreationError::TopicInvalid)
    );
    assert!(plan("Ok", "line one\nline two", "").is_ok());
    for alias in ["team", "#team", "#team:example.org", " team "] {
        let plan = plan("Ok", "", alias).unwrap();
        assert_eq!(plan.alias().as_deref(), Some("#team:example.org"));
        assert_eq!(
            plan.request().unwrap().room_alias_name.as_deref(),
            Some("team")
        );
    }
    assert_eq!(
        plan("Ok", "", "#team:other.org"),
        Err(CreationError::AliasServer)
    );
    let long = "a".repeat(65);
    for invalid in ["Team", "te am", "team/x", "#", long.as_str()] {
        assert_eq!(
            plan("Ok", "", invalid),
            Err(CreationError::AliasInvalid),
            "{invalid}"
        );
    }
    assert_eq!(plan("Ok", "", "").unwrap().alias(), None);
}

#[test]
fn access_listing_and_placement_follow_what_the_context_allows() {
    let mut public = choices(CreationKind::Room, "Lobby");
    public.listed = true;
    assert_eq!(
        CreationPlan::new(&public, &context(None)),
        Err(CreationError::ListedNeedsPublic)
    );
    public.access = CreationAccess::Public;
    let request = CreationPlan::new(&public, &context(None))
        .unwrap()
        .request()
        .unwrap();
    assert_eq!(request.visibility, Visibility::Public);
    assert_eq!(
        request.preset,
        Some(create_room::v3::RoomPreset::PublicChat)
    );
    let public_state = initial_state(&request);
    assert_eq!(
        of_type(&public_state, "m.room.join_rules")[0]["content"]["join_rule"],
        "public"
    );

    // Space members only inside a Space whose homeserver supports it.
    let mut members = choices(CreationKind::Room, "Planning");
    members.access = CreationAccess::SpaceMembers;
    assert_eq!(
        CreationPlan::new(&members, &context(None)),
        Err(CreationError::AccessUnavailable)
    );
    members.parent = Some(SPACE.into());
    let mut unsupported = context(Some(true));
    unsupported.space_members_supported = false;
    assert_eq!(
        CreationPlan::new(&members, &unsupported),
        Err(CreationError::AccessUnavailable)
    );
    members.suggested = true;
    let plan = CreationPlan::new(&members, &context(Some(true))).unwrap();
    assert_eq!(plan.parent_id(), Some(SPACE));
    let state = initial_state(&plan.request().unwrap());
    let rules = of_type(&state, "m.room.join_rules");
    let rule = &rules[0]["content"];
    assert_eq!(rule["join_rule"], "restricted");
    assert_eq!(rule["allow"][0]["room_id"], SPACE);
    let parent = of_type(&state, "m.space.parent");
    assert_eq!(parent[0]["state_key"], SPACE);
    assert_eq!(parent[0]["content"]["canonical"], true);
    assert_eq!(parent[0]["content"]["via"][0], "example.org");
    assert!(
        plan.preview()
            .iter()
            .any(|line| line.value.contains("suggested"))
    );

    // Placement needs a fresh right to list rooms in that very Space.
    let mut placed = choices(CreationKind::Room, "Notes");
    placed.parent = Some(SPACE.into());
    assert_eq!(
        CreationPlan::new(&placed, &context(Some(false))),
        Err(CreationError::PlacementNotAllowed)
    );
    assert_eq!(
        CreationPlan::new(&placed, &context(None)),
        Err(CreationError::ContextChanged)
    );
    placed.parent = Some("!other:example.org".into());
    assert_eq!(
        CreationPlan::new(&placed, &context(Some(true))),
        Err(CreationError::ContextChanged)
    );
    // A Home room names no parent.
    let home = CreationPlan::new(
        &choices(CreationKind::Room, "Home room"),
        &context(Some(true)),
    )
    .unwrap();
    assert!(of_type(&initial_state(&home.request().unwrap()), "m.space.parent").is_empty());
}

#[test]
fn the_preview_states_server_identity_and_every_applied_setting() {
    let mut chosen = choices(CreationKind::Room, "Design\u{202e}Review");
    chosen.alias = "design".into();
    let plan = CreationPlan::new(&chosen, &context(None)).unwrap();
    let preview = plan.preview();
    let labels = preview.iter().map(|line| line.label).collect::<Vec<_>>();
    for label in [
        "Creates",
        "On",
        "Who can join",
        "Directory",
        "Address",
        "Messages",
        "History",
        "Guests",
        "Placement",
        "Room version",
    ] {
        assert!(labels.contains(&label), "{label}");
    }
    let on = &preview[1].value;
    assert!(on.contains("example.org") && on.contains("@alice:example.org"));
    assert!(!preview[0].value.contains('\u{202e}'), "plain text only");
    assert!(
        preview
            .iter()
            .any(|line| line.value == "#design:example.org")
    );
}

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

#[test]
fn only_a_definite_refusal_says_nothing_was_created() {
    for (status, errcode, expected) in [
        (400, "M_ROOM_IN_USE", CreationError::AliasTaken),
        (429, "M_LIMIT_EXCEEDED", CreationError::RateLimited),
        (403, "M_FORBIDDEN", CreationError::Forbidden),
        (
            400,
            "M_UNSUPPORTED_ROOM_VERSION",
            CreationError::Unsupported,
        ),
        (400, "M_INVALID_ROOM_STATE", CreationError::Rejected),
    ] {
        let failure = create_failure(&api_error(status, errcode));
        assert_eq!(failure, expected, "{errcode}");
        assert!(!failure.may_exist());
        assert!(failure.text().contains("Nothing was created"));
        assert!(!failure.text().contains("private"));
    }
    // With the replay refused, an expired token returns its 401 as is: it
    // reads as unconfirmed, for the create and for the Space link alike.
    assert_eq!(
        create_failure(&api_error(401, "M_UNKNOWN_TOKEN")),
        CreationError::OutcomeUnknown
    );
    assert_eq!(
        link_failure(&api_error(401, "M_UNKNOWN_TOKEN")),
        LinkFailure::OutcomeUnknown
    );
    // A server error or a reply that cannot be read may follow a creation:
    // the caller must say so and must not send again by itself.
    for error in [
        api_error(500, "M_UNKNOWN"),
        api_error(502, "M_UNKNOWN"),
        HttpError::ResponseTooLarge {
            limit: MAX_REPLY_BYTES,
        },
    ] {
        let failure = create_failure(&error);
        assert_eq!(failure, CreationError::OutcomeUnknown);
        assert!(failure.may_exist());
        assert!(failure.text().contains("may or may not"));
    }
    // A Space link that got no reply may be retried; a refusal is final.
    assert_eq!(
        link_failure(&api_error(504, "M_UNKNOWN")),
        LinkFailure::OutcomeUnknown
    );
    // An oversize reply to the link is not proof that it was written.
    let oversize = HttpError::ResponseTooLarge {
        limit: MAX_REPLY_BYTES,
    };
    assert_eq!(link_failure(&oversize), LinkFailure::OutcomeUnknown);
    // Every creation request is sent once and capped.
    let config = format!("{:?}", reply_config(CREATE_TIMEOUT));
    assert!(
        config.contains(&format!("max_response_bytes: {MAX_REPLY_BYTES}")),
        "{config}"
    );
    assert!(config.contains("retry_limit: 0"), "{config}");
    assert_eq!(
        link_failure(&api_error(403, "M_FORBIDDEN")),
        LinkFailure::NotAllowed
    );
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

/// The pinned client's 401 path, as client/futures.rs runs it: one send,
/// refused with M_UNKNOWN_TOKEN, a wait for the token refresh, then the same
/// request sent again. `sends` counts what reaches the wire.
async fn send_with_held_refresh(
    sends: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    refresh: std::sync::Arc<tokio::sync::Notify>,
) -> Result<&'static str, HttpError> {
    use std::sync::atomic::Ordering;
    sends.fetch_add(1, Ordering::SeqCst);
    refresh.notified().await;
    sends.fetch_add(1, Ordering::SeqCst);
    Ok("!created:example.org")
}

#[test]
fn a_revocation_while_a_token_refresh_waits_sends_nothing_more_and_reads_unknown() {
    use crate::account_selection::Selections;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    runtime().block_on(async {
        let selections = Selections::default();
        selections.replace(Some("a"));
        let selection = selections.for_account("a").unwrap();
        let sends = Arc::new(AtomicUsize::new(0));
        let refresh = Arc::new(tokio::sync::Notify::new());
        let racing = tokio::spawn({
            let send = send_with_held_refresh(sends.clone(), refresh.clone());
            async move { race_revocation(&selection, send).await }
        });
        while sends.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // A -> B while the refresh waits: the resend for A is dropped...
        selections.replace(Some("b"));
        let outcome = racing.await.unwrap();
        assert!(outcome.is_none(), "a revoked send is not reported as sent");
        // ...and even once the refresh completes, nothing more goes out.
        refresh.notify_waiters();
        tokio::task::yield_now().await;
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        // create_room_for reports a revoked admitted send as unknown: it may
        // have reached the homeserver before it was dropped.
        assert!(CreationError::OutcomeUnknown.may_exist());

        // Without a revocation the base SDK still replays the request after
        // its refresh: retry_limit 0 and this race alone do not make one
        // attempt on the wire. mutation_config therefore also opts out of
        // the replay (the vendored disable_auth_refresh).
        selections.replace(Some("a"));
        let selection = selections.for_account("a").unwrap();
        let replayed = Arc::new(AtomicUsize::new(0));
        let refresh = Arc::new(tokio::sync::Notify::new());
        let racing = tokio::spawn({
            let send = send_with_held_refresh(replayed.clone(), refresh.clone());
            async move { race_revocation(&selection, send).await }
        });
        while replayed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        refresh.notify_one();
        assert!(racing.await.unwrap().is_some());
        assert_eq!(
            replayed.load(Ordering::SeqCst),
            2,
            "the replay is not prevented here"
        );

        // A send that finished first keeps its confirmed result, even when
        // the selection is revoked right after.
        selections.replace(Some("a"));
        let selection = selections.for_account("a").unwrap();
        let done = race_revocation(&selection, async {
            Ok::<_, HttpError>("!room:example.org")
        });
        let confirmed = done.await;
        selections.replace(None);
        assert_eq!(confirmed.map(Result::ok), Some(Some("!room:example.org")));
    });
}

#[test]
fn an_unconfirmed_link_says_its_placement_is_unconfirmed() {
    let text = LinkFailure::OutcomeUnknown.text();
    assert!(text.contains("unconfirmed"), "{text}");
    assert!(
        !text.contains("not added") && !text.contains("not listed"),
        "{text}"
    );
}

#[test]
fn a_revocation_ready_with_the_read_starts_no_write() {
    use crate::account_selection::Selections;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    runtime().block_on(async {
        let selections = Selections::default();
        selections.replace(Some("a"));
        let selection = selections.for_account("a").unwrap();
        let writes = Arc::new(AtomicUsize::new(0));
        // The step before the write (a permission read) is already done, so
        // the write would start on the first poll...
        let write = {
            let writes = writes.clone();
            async move {
                std::future::ready(()).await;
                writes.fetch_add(1, Ordering::SeqCst);
                Ok::<_, HttpError>(())
            }
        };
        // ...but the account is left before that poll: both are ready.
        selections.replace(Some("b"));
        assert!(race_revocation(&selection, write).await.is_none());
        assert_eq!(
            writes.load(Ordering::SeqCst),
            0,
            "a write started after revocation"
        );
    });
}

#[test]
fn a_space_without_a_usable_name_is_shown_by_its_id_as_bounded_plain_text() {
    // Accepted room IDs only need the sigil and no NUL, and may be of any
    // length (compat-arbitrary-length-ids): never shown raw.
    const BIDI: &str = "!evil\u{202e}gnp.exe\u{7}\u{200b}:example.org";
    let long = format!("!{}:example.org", "a".repeat(600));
    let hidden = |shown: &str| {
        shown.chars().any(|ch| {
            ch.is_control() || matches!(ch, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}')
        })
    };
    // No name, an empty one, a blank one, one of only controls and
    // formatting characters: the ID, through the same plain-text policy.
    for name in [
        None,
        Some(""),
        Some("   "),
        Some("\u{7}\u{202e}\u{200b}\u{feff}"),
    ] {
        assert_eq!(parent_label(name, SPACE), SPACE);
        let shown = parent_label(name, BIDI);
        assert!(!hidden(&shown), "{shown:?}");
        assert!(shown.starts_with("!evil") && shown.ends_with(":example.org"));
        let shown = parent_label(name, &long);
        assert!(shown.chars().count() <= MAX_LABEL_CHARS && shown.ends_with('…'));
    }
    // A usable name is shown as plain text, bounded the same way.
    assert_eq!(parent_label(Some("  Team\u{202e} "), BIDI), "Team");
    assert!(parent_label(Some(&"n".repeat(500)), SPACE).chars().count() <= MAX_LABEL_CHARS);

    // Display only: every ID the plan checks, sends or acts on stays exact.
    for space_id in [BIDI, long.as_str()] {
        assert!(RoomId::parse(space_id).is_ok(), "an accepted ID");
        let mut context = context(Some(true));
        let parent = context.parent.as_mut().unwrap();
        parent.space_id = space_id.into();
        parent.name = parent_label(None, space_id);
        let mut choices = choices(CreationKind::Room, "Notes");
        choices.parent = Some(space_id.into());
        let plan = CreationPlan::new(&choices, &context).unwrap();
        assert_eq!(plan.parent_id(), Some(space_id));
        let state = initial_state(&plan.request().unwrap());
        assert_eq!(of_type(&state, "m.space.parent")[0]["state_key"], space_id);
        let placement = plan
            .preview()
            .into_iter()
            .find(|line| line.label == "Placement")
            .unwrap();
        assert!(!hidden(&placement.value), "{:?}", placement.value);
        assert!(placement.value.chars().count() <= MAX_LABEL_CHARS + 40);
    }
}

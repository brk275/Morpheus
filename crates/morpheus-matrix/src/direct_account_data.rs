//! Listing one room under one user in the account's `m.direct` data, with
//! typed, bounded requests: a fresh authoritative read, a re-read
//! immediately before the write, and one write.
//!
//! The stored content is merged as raw JSON text, never through a parsed
//! value: every other top-level field (other users and any unknown field,
//! numbers kept lexically, beyond u64 or decimal) is copied byte for byte,
//! and only the one `user -> room` entry is changed, by appending to its
//! raw list. Content that is not an object, has duplicate fields, or whose
//! target entry is not a list of valid room ID strings is refused rather
//! than rewritten. The two reads are compared as the exact bytes the server
//! returned; if they differ at all, nothing is written (another client
//! changed the listing, or it was reformatted; the caller may read again). Matrix account data has no
//! compare-and-swap: a change landing between the re-read and the write can
//! still be overwritten. That window is as short as one round trip, and it
//! is stated, not hidden.
//!
//! Every request is sent at most once: no HTTP retry and no token-refresh
//! replay. Each step is raced
//! against the caller's guard (account selection and cancellation); a guard
//! that ends before the write leaves nothing written, and one that ends
//! while the write may have left reports `Unknown`, never retried.
//!
//! One listing at a time in this process (`direct_turn`): every caller of
//! `list_direct_room` (People's new conversation, an accepted direct-message
//! invitation, the unbound accept) waits its turn before its first read and
//! keeps it until its write is answered, so no two in-process writers can
//! both read the same baseline and then each write over the other's room.
//! The wait is raced against the caller's guard and bounded
//! (`DIRECT_TURN_WAIT`); a caller that stops or runs out of time while
//! waiting has sent nothing. A write whose answer is unknown ends its turn
//! like any other, and may still land after the next listing read its
//! baseline. None of this is a conditional write against other clients or
//! devices: Matrix account data has none.
//!
//! The module is shared: people_directory uses it after creating a direct
//! conversation, and the invitations line's direct-message listing on join
//! can share `list_direct_room` without changing the creation result.

use std::{collections::BTreeMap, future::Future, sync::OnceLock, time::Duration};

use matrix_sdk::{
    Client, HttpError,
    config::RequestConfig,
    ruma::{
        RoomId, UserId,
        api::{
            client::config::{get_global_account_data, set_global_account_data},
            error::ErrorKind,
        },
        events::GlobalAccountDataEventType,
        serde::Raw,
    },
};
use serde::Deserialize;
use serde_json::value::RawValue;

/// Largest stored `m.direct` content read; larger is refused, not cut.
pub const MAX_DIRECT_CONTENT_BYTES: u64 = 1024 * 1024;
const MAX_WRITE_RESPONSE_BYTES: u64 = 16 * 1024;
const DIRECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest wait for this process's `m.direct` turn: a listing holds it for
/// at most two reads and a write.
const DIRECT_TURN_WAIT: Duration = Duration::from_secs(60);

/// Ends a listing step: the account selection and the caller's cancellation.
pub trait DirectWriteGuard {
    /// Whether the step must not start (selection ended or cancelled).
    fn stopped(&self) -> bool;
    /// Resolves when the selection ends or the caller cancels.
    fn wait_stopped(&self) -> impl Future<Output = ()> + Send;
}

/// What happened to the listing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectListing {
    /// The write was accepted; the room is now listed under the user.
    Listed,
    /// The fresh read already listed it; nothing was written.
    AlreadyListed,
}

/// Why the room is not (known to be) listed. Fixed and sanitized.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DirectWriteError {
    #[error("the direct-message list could not be read; nothing was changed")]
    Unreadable,
    #[error("the direct-message list changed on the server; nothing was changed")]
    BaselineChanged,
    #[error("the direct-message list was not updated")]
    NotSent,
    #[error("the homeserver refused the direct-message list update")]
    Refused,
    #[error("the direct-message list update may or may not have applied")]
    Unknown,
}

/// A JSON object's fields as raw text. A normal map deserializer silently
/// keeps only the last duplicate key; refuse ambiguous objects instead of
/// dropping a field on write.
struct UniqueFields(BTreeMap<String, Box<RawValue>>);

impl<'de> Deserialize<'de> for UniqueFields {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FieldsVisitor;
        impl<'de> serde::de::Visitor<'de> for FieldsVisitor {
            type Value = UniqueFields;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object with unique field names")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, Box<RawValue>>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate m.direct field"));
                    }
                }
                Ok(UniqueFields(fields))
            }
        }
        deserializer.deserialize_map(FieldsVisitor)
    }
}

/// The stored content (raw JSON text, `None` when none is stored) with
/// `room` appended to `user`'s list, as raw JSON text; `Ok(None)` when it is
/// already listed. Every other field is copied as its exact raw text; see
/// the module notes for what is refused.
pub fn merged_direct(
    stored: Option<&str>,
    user: &str,
    room: &str,
) -> Result<Option<String>, DirectWriteError> {
    let unreadable = |_| DirectWriteError::Unreadable;
    let mut fields = match stored {
        None => BTreeMap::new(),
        Some(raw) => {
            serde_json::from_str::<UniqueFields>(raw)
                .map_err(unreadable)?
                .0
        }
    };
    let room_json = serde_json::to_string(room).map_err(unreadable)?;
    let entry = match fields.get(user) {
        None => format!("[{room_json}]"),
        Some(raw) => {
            // The entry being changed must be exactly a list of room ID
            // strings; anything else is refused, not rewritten.
            let elements: Vec<Box<RawValue>> =
                serde_json::from_str(raw.get()).map_err(unreadable)?;
            let mut listed = false;
            for element in &elements {
                let id: String = serde_json::from_str(element.get()).map_err(unreadable)?;
                if RoomId::parse(&id).is_err() {
                    return Err(DirectWriteError::Unreadable);
                }
                listed |= id == room;
            }
            if listed {
                return Ok(None);
            }
            let mut parts: Vec<&str> = elements.iter().map(|element| element.get()).collect();
            parts.push(&room_json);
            format!("[{}]", parts.join(","))
        }
    };
    let entry = RawValue::from_string(entry).map_err(unreadable)?;
    fields.insert(user.to_owned(), entry);
    let mut body = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        body.push_str(&serde_json::to_string(key).map_err(unreadable)?);
        body.push(':');
        body.push_str(value.get());
    }
    body.push('}');
    Ok(Some(body))
}

fn read_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(DIRECT_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_DIRECT_CONTENT_BYTES)
        .disable_auth_refresh()
}

fn write_config() -> RequestConfig {
    RequestConfig::new()
        .retry_limit(0)
        .timeout(DIRECT_TIMEOUT)
        .force_auth()
        .max_response_bytes(MAX_WRITE_RESPONSE_BYTES)
        .disable_auth_refresh()
}

fn not_found(error: &HttpError) -> bool {
    error.as_client_api_error().is_some_and(|api| {
        api.status_code.as_u16() == 404 && api.error_kind() == Some(&ErrorKind::NotFound)
    })
}

/// A client error: the server answered and did not apply the request.
fn refused_by_server(error: &HttpError) -> bool {
    error
        .as_client_api_error()
        .is_some_and(|api| api.status_code.is_client_error())
}

/// One authoritative read of the stored `m.direct` content as the exact raw
/// text the server returned, raced against the guard. `Ok(None)`: none is
/// stored (HTTP 404 with M_NOT_FOUND only; anything else is refused).
async fn read_direct(
    client: &Client,
    own: &UserId,
    guard: &impl DirectWriteGuard,
) -> Result<Option<String>, DirectWriteError> {
    if guard.stopped() {
        return Err(DirectWriteError::NotSent);
    }
    let request = get_global_account_data::v3::Request::new(
        own.to_owned(),
        GlobalAccountDataEventType::Direct,
    );
    let response = tokio::select! {
        biased;
        _ = guard.wait_stopped() => return Err(DirectWriteError::NotSent),
        response = async {
            if guard.stopped() {
                return Err(DirectWriteError::NotSent);
            }
            Ok(client.send(request).with_request_config(read_config()).await)
        } => response?,
    };
    match response {
        Ok(response) => Ok(Some(response.account_data.json().get().to_owned())),
        Err(error) if not_found(&error) => Ok(None),
        Err(_) => Err(DirectWriteError::Unreadable),
    }
}

/// This process's one `m.direct` turn (see the module notes).
fn direct_turn() -> &'static tokio::sync::Mutex<()> {
    static TURN: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    TURN.get_or_init(Default::default)
}

/// List `room_id` under `target` in the `m.direct` data of the account
/// `own`, which must be the client's own user. Its turn, a fresh read, a
/// re-read immediately before the single write, and the write; see the
/// module notes.
pub async fn list_direct_room(
    client: &Client,
    own: &UserId,
    target: &UserId,
    room_id: &RoomId,
    guard: &impl DirectWriteGuard,
) -> Result<DirectListing, DirectWriteError> {
    // Only ever the exact account whose client this is.
    if client.user_id() != Some(own) {
        return Err(DirectWriteError::NotSent);
    }
    if guard.stopped() {
        return Err(DirectWriteError::NotSent);
    }
    // Waiting for the turn sends nothing; it is kept until this listing's
    // write is answered or ends.
    let _turn = tokio::select! {
        biased;
        _ = guard.wait_stopped() => return Err(DirectWriteError::NotSent),
        _ = tokio::time::sleep(DIRECT_TURN_WAIT) => return Err(DirectWriteError::NotSent),
        turn = direct_turn().lock() => turn,
    };
    list_in_turn(client, own, target, room_id, guard).await
}

/// `list_direct_room` once it is the caller's turn.
async fn list_in_turn(
    client: &Client,
    own: &UserId,
    target: &UserId,
    room_id: &RoomId,
    guard: &impl DirectWriteGuard,
) -> Result<DirectListing, DirectWriteError> {
    let first = read_direct(client, own, guard).await?;
    let Some(merged) = merged_direct(first.as_deref(), target.as_str(), room_id.as_str())? else {
        return Ok(DirectListing::AlreadyListed);
    };
    // Fresh baseline immediately before the write, compared as the exact
    // bytes returned: refuse, never overwrite, any change meanwhile.
    let second = read_direct(client, own, guard).await?;
    if second != first {
        return Err(DirectWriteError::BaselineChanged);
    }
    let raw = RawValue::from_string(merged).map_err(|_| DirectWriteError::NotSent)?;
    let request = set_global_account_data::v3::Request::new_raw(
        own.to_owned(),
        GlobalAccountDataEventType::Direct,
        Raw::from_json(raw),
    );
    if guard.stopped() {
        return Err(DirectWriteError::NotSent);
    }
    let response = tokio::select! {
        biased;
        // The write may already be on its way: unknown, never retried.
        _ = guard.wait_stopped() => return Err(DirectWriteError::Unknown),
        response = async {
            if guard.stopped() {
                return Err(DirectWriteError::NotSent);
            }
            Ok(client.send(request).with_request_config(write_config()).await)
        } => response?,
    };
    match response {
        Ok(_) => Ok(DirectListing::Listed),
        Err(error) if refused_by_server(&error) => Err(DirectWriteError::Refused),
        Err(_) => Err(DirectWriteError::Unknown),
    }
}

/// Raw account-data content (raw JSON text, `None` when none is stored) with
/// one top-level `field` set to the exact JSON text `value`, every other
/// field copied as its exact raw text; `Ok(None)` when the field already
/// holds exactly that text. The same lexical rules as `merged_direct`
/// (content that is not an object or has duplicate fields is refused), but
/// separate from it and from the m.direct turn: only the room
/// `m.marked_unread` writer (unread_navigation) calls it.
pub(crate) fn merged_field(
    stored: Option<&str>,
    field: &str,
    value: &str,
) -> Result<Option<String>, DirectWriteError> {
    let unreadable = |_| DirectWriteError::Unreadable;
    let mut fields = match stored {
        None => BTreeMap::new(),
        Some(raw) => {
            serde_json::from_str::<UniqueFields>(raw)
                .map_err(unreadable)?
                .0
        }
    };
    if fields
        .get(field)
        .is_some_and(|current| current.get() == value)
    {
        return Ok(None);
    }
    let value = RawValue::from_string(value.to_owned()).map_err(unreadable)?;
    fields.insert(field.to_owned(), value);
    let mut body = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        body.push_str(&serde_json::to_string(key).map_err(unreadable)?);
        body.push(':');
        body.push_str(value.get());
    }
    body.push('}');
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BIG: &str = "123456789012345678901234567890";

    #[test]
    fn unknown_fields_and_numbers_survive_byte_for_byte() {
        let stored = format!(
            r#"{{"@other:example.org":["!x:example.org","!y:example.org"],"@target:example.org":["!old:example.org"],"org.example.unknown":{{"big":{BIG},"decimal":1.10,"exp":2.5E-3}}}}"#
        );
        let merged = merged_direct(Some(&stored), "@target:example.org", "!new:example.org")
            .unwrap()
            .unwrap();
        assert!(merged.contains(&format!(r#""big":{BIG}"#)), "{merged}");
        assert!(merged.contains(r#""decimal":1.10"#), "{merged}");
        assert!(merged.contains(r#""exp":2.5E-3"#), "{merged}");
        assert!(merged.contains(r#""@other:example.org":["!x:example.org","!y:example.org"]"#));
        assert!(
            merged.contains(r#""@target:example.org":["!old:example.org","!new:example.org"]"#)
        );
        // Nothing stored yet: only the new entry.
        assert_eq!(
            merged_direct(None, "@target:example.org", "!new:example.org")
                .unwrap()
                .as_deref(),
            Some(r#"{"@target:example.org":["!new:example.org"]}"#)
        );
    }

    #[test]
    fn an_existing_listing_writes_nothing_and_invalid_targets_are_refused() {
        let stored = r#"{"@target:example.org":["!new:example.org"]}"#;
        assert_eq!(
            merged_direct(Some(stored), "@target:example.org", "!new:example.org"),
            Ok(None)
        );
        // The entry being changed must be a list of valid room ID strings;
        // the content must be an object without duplicate fields.
        for odd in [
            "[]",
            r#""text""#,
            r#"{"@target:example.org":"!new:example.org"}"#,
            r#"{"@target:example.org":["!a:example.org",42]}"#,
            r#"{"@target:example.org":["not a room id"]}"#,
            r#"{"@target:example.org":[],"@target:example.org":[]}"#,
        ] {
            assert_eq!(
                merged_direct(Some(odd), "@target:example.org", "!new:example.org"),
                Err(DirectWriteError::Unreadable),
                "{odd}"
            );
        }
        // Another user's odd entry is not the target: it is kept as is.
        let other = r#"{"@other:example.org":"odd","@target:example.org":[]}"#;
        let merged = merged_direct(Some(other), "@target:example.org", "!new:example.org")
            .unwrap()
            .unwrap();
        assert!(merged.contains(r#""@other:example.org":"odd""#));
    }

    #[test]
    fn a_baseline_differing_only_in_a_big_number_is_a_change() {
        // Baselines are compared as the exact bytes returned, so a change a
        // parsed number would round away is still a change.
        let first = format!(r#"{{"org.example":{{"n":{BIG}}}}}"#);
        let second = r#"{"org.example":{"n":123456789012345678901234567891}}"#.to_owned();
        let parsed = |text: &str| {
            serde_json::from_str::<serde_json::Value>(text).unwrap()["org.example"]["n"]
                .as_f64()
                .map(f64::to_bits)
        };
        // A parsed comparison would call these equal...
        assert_eq!(parsed(&first), parsed(&second));
        // ...the raw baseline comparison does not.
        assert_ne!(Some(first), Some(second));
    }

    #[test]
    fn requests_are_bounded_single_attempts() {
        for config in [read_config(), write_config()] {
            let shown = format!("{config:?}");
            assert!(shown.contains("retry_limit: 0") || shown.contains("retry_limit: Some(0)"));
            assert!(shown.contains("force_auth: true"));
            assert!(shown.contains("max_response_bytes"));
        }
        assert!(format!("{:?}", read_config()).contains(&MAX_DIRECT_CONTENT_BYTES.to_string()));
    }
}

//! Warnings about room members whose cryptographic identity changed.
//!
//! The SDK reports a member in "pin violation" when their identity differs
//! from the one first seen for them, and in "verification violation" when an
//! identity this account had verified was replaced. Morpheus shows these and
//! changes trust only on the person's explicit choice: accepting a changed
//! identity pins it, and withdrawing a lost verification stops requiring it.
//! Nothing is accepted, verified or withdrawn automatically. Every feed and
//! action is bound to the account selection it started under.

use std::collections::BTreeMap;

use futures_util::StreamExt;
use matrix_sdk::{
    Client, RoomState,
    ruma::{OwnedUserId, RoomId, UserId},
};
use matrix_sdk_crypto::IdentityState;
use tokio::{sync::watch, task::JoinHandle};

use crate::{AccountManager, AccountSelection, MatrixCoreError, Result, selected_loaded_for};

/// Most warnings published for one room; the count of the rest is kept.
pub const MAX_IDENTITY_WARNINGS: usize = 50;
const MAX_NAME_CHARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdentityWarningKind {
    /// An identity this account verified was replaced (the more serious case).
    VerificationLost,
    /// The member's identity differs from the one first seen for them.
    Changed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityWarning {
    pub user_id: String,
    /// Bounded, sanitized room display name, if the member has one.
    pub display_name: Option<String>,
    pub kind: IdentityWarningKind,
}

/// The current warnings of one room, most serious first, at most
/// [`MAX_IDENTITY_WARNINGS`], plus how many more exist.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IdentityWarnings {
    pub warnings: Vec<IdentityWarning>,
    pub more: usize,
}

/// A live warning feed. Dropping it stops the SDK subscription; it also ends
/// with its account selection.
pub struct IdentityWarningFeed {
    pub account_id: String,
    pub room_id: String,
    updates: watch::Receiver<IdentityWarnings>,
    task: JoinHandle<()>,
}

impl IdentityWarningFeed {
    pub fn subscribe(&self) -> watch::Receiver<IdentityWarnings> {
        self.updates.clone()
    }
}

impl Drop for IdentityWarningFeed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdentityAckError {
    #[error("The account changed. Nothing was changed.")]
    AccountChanged,
    #[error("This member's identity changed again. Review the new warning.")]
    StateChanged,
    #[error("Could not update this identity. Try again.")]
    Unavailable,
}

impl AccountManager {
    /// Follow the identity warnings of one joined room.
    pub async fn identity_warnings_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> Result<IdentityWarningFeed> {
        let client = self.identity_client(selection).await?;
        let id = RoomId::parse(room_id).map_err(|e| MatrixCoreError::InvalidRoomId(e.to_string()))?;
        let room = client
            .get_room(&id)
            .filter(|room| room.state() == RoomState::Joined)
            .ok_or_else(|| MatrixCoreError::RoomNotFound(room_id.into()))?;
        let changes = room
            .subscribe_to_identity_status_changes()
            .await
            .map_err(|_| MatrixCoreError::Sdk("identity status is unavailable".into()))?;
        // The SDK stream is not Unpin; the task owns it pinned.
        let mut changes = Box::pin(changes);
        let (updates, receiver) = watch::channel(IdentityWarnings::default());
        let owner = selection.clone();
        let task = tokio::spawn(async move {
            let mut known: BTreeMap<OwnedUserId, IdentityWarningKind> = BTreeMap::new();
            while let Some(Some(batch)) = owner.guard(changes.next()).await {
                for change in batch {
                    match warning_kind(&change.changed_to) {
                        Some(kind) => {
                            known.insert(change.user_id, kind);
                        }
                        None => {
                            known.remove(&change.user_id);
                        }
                    }
                }
                let mut warnings = Vec::new();
                for (user_id, kind) in ordered(&known).take(MAX_IDENTITY_WARNINGS) {
                    let display_name = match room.get_member_no_sync(user_id).await {
                        Ok(Some(member)) => member.display_name().and_then(bounded_name),
                        _ => None,
                    };
                    warnings.push(IdentityWarning {
                        user_id: user_id.to_string(),
                        display_name,
                        kind,
                    });
                }
                let more = known.len().saturating_sub(warnings.len());
                if !owner.is_current() {
                    break;
                }
                updates.send_replace(IdentityWarnings { warnings, more });
            }
        });
        Ok(IdentityWarningFeed {
            account_id: selection.account_id().to_owned(),
            room_id: room_id.to_owned(),
            updates: receiver,
            task,
        })
    }

    /// Apply the person's explicit choice for one warning, after checking the
    /// member's identity is still in the state they were shown.
    pub async fn acknowledge_identity_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        user_id: &str,
        kind: IdentityWarningKind,
    ) -> std::result::Result<(), IdentityAckError> {
        let client = self
            .identity_client(selection)
            .await
            .map_err(|_| IdentityAckError::AccountChanged)?;
        let room = RoomId::parse(room_id)
            .ok()
            .and_then(|id| client.get_room(&id))
            .filter(|room| room.state() == RoomState::Joined)
            .ok_or(IdentityAckError::Unavailable)?;
        let user = UserId::parse(user_id).map_err(|_| IdentityAckError::Unavailable)?;
        if room.own_user_id() == &*user {
            return Err(IdentityAckError::Unavailable);
        }
        let identity = client
            .encryption()
            .get_user_identity(&user)
            .await
            .map_err(|_| IdentityAckError::Unavailable)?
            .ok_or(IdentityAckError::Unavailable)?;
        if !self.identity_owner(selection) {
            return Err(IdentityAckError::AccountChanged);
        }
        let shown = matches!(
            (kind, identity.has_verification_violation(), identity.is_verified()),
            (IdentityWarningKind::VerificationLost, true, _)
                | (IdentityWarningKind::Changed, false, false)
        );
        if !shown {
            return Err(IdentityAckError::StateChanged);
        }
        let result = match kind {
            IdentityWarningKind::VerificationLost => identity.withdraw_verification().await,
            IdentityWarningKind::Changed => identity.pin().await,
        };
        result.map_err(|_| IdentityAckError::Unavailable)
    }

    fn identity_owner(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| current.same_epoch(selection))
    }

    /// The client of this exact selection epoch.
    async fn identity_client(&self, selection: &AccountSelection) -> Result<Client> {
        if !self.identity_owner(selection) {
            return Err(MatrixCoreError::SelectedAccountChanged);
        }
        let state = self.state.lock().await;
        let loaded = selected_loaded_for(&state, selection.account_id())?;
        Ok(loaded.client.clone())
    }
}

fn warning_kind(state: &IdentityState) -> Option<IdentityWarningKind> {
    match state {
        IdentityState::PinViolation => Some(IdentityWarningKind::Changed),
        IdentityState::VerificationViolation => Some(IdentityWarningKind::VerificationLost),
        _ => None,
    }
}

/// Lost verifications first, then by user ID.
fn ordered(
    known: &BTreeMap<OwnedUserId, IdentityWarningKind>,
) -> impl Iterator<Item = (&OwnedUserId, IdentityWarningKind)> {
    let lost = known
        .iter()
        .filter(|(_, kind)| **kind == IdentityWarningKind::VerificationLost);
    let changed = known
        .iter()
        .filter(|(_, kind)| **kind == IdentityWarningKind::Changed);
    lost.chain(changed).map(|(user_id, kind)| (user_id, *kind))
}

/// One display line without control, invisible or direction-override
/// characters, at most [`MAX_NAME_CHARS`].
fn bounded_name(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200b}'..='\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2060}'..='\u{206f}'
                        | '\u{feff}'
                )
        })
        .collect();
    let bounded: String = cleaned.trim().chars().take(MAX_NAME_CHARS).collect();
    let bounded = bounded.trim_end();
    (!bounded.is_empty()).then(|| bounded.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;

    #[test]
    fn only_violations_warn_and_lost_verifications_come_first() {
        use IdentityWarningKind::{Changed, VerificationLost};
        assert_eq!(warning_kind(&IdentityState::PinViolation), Some(Changed));
        let lost = warning_kind(&IdentityState::VerificationViolation);
        assert_eq!(lost, Some(VerificationLost));
        assert_eq!(warning_kind(&IdentityState::Pinned), None);
        assert_eq!(warning_kind(&IdentityState::Verified), None);

        let known = BTreeMap::from([
            (user_id!("@a:x.org").to_owned(), Changed),
            (user_id!("@b:x.org").to_owned(), VerificationLost),
            (user_id!("@c:x.org").to_owned(), Changed),
        ]);
        let order: Vec<_> = ordered(&known).map(|(user, _)| user.as_str()).collect();
        assert_eq!(order, ["@b:x.org", "@a:x.org", "@c:x.org"]);
    }

    #[test]
    fn display_names_are_bounded_and_stripped_of_spoofing_characters() {
        assert_eq!(bounded_name(" Bob\u{202e}\n "), Some("Bob".into()));
        assert_eq!(bounded_name("\u{200b} "), None);
        let long = "n".repeat(200);
        let count = bounded_name(&long).map(|name| name.chars().count());
        assert_eq!(count, Some(MAX_NAME_CHARS));
    }
}

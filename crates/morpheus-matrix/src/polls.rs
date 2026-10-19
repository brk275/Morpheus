//! Native Matrix polls through the MSC3381 event types the pinned timeline
//! aggregates (`org.matrix.msc3381.poll.*`); stable `m.poll.*` events are not
//! aggregated by that timeline and are never sent here.
//!
//! Poll state, edits, votes and ends always come from the SDK timeline item;
//! nothing here counts votes itself. Undisclosed results never leave this
//! module before the poll has ended. Every action is bound to the account
//! selection the person acted under, revalidated against the live timeline
//! item and the room's power levels, and sent once through the SDK's normal
//! room send path, which encrypts in encrypted rooms. Votes and ends are not
//! queued as local echoes: the pinned timeline cannot show their delivery
//! state and cannot undo a failed end, so a vote or end appears only when the
//! server confirms it, and a failure is reported instead.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use matrix_sdk::{
    Room, RoomState,
    deserialized_responses::RawSyncOrStrippedState,
    ruma::{
        EventId, OwnedTransactionId, TransactionId, UInt, UserId,
        api::error::ErrorKind,
        events::{
            MessageLikeEventType,
            poll::{
                start::PollKind,
                unstable_end::UnstablePollEndEventContent,
                unstable_response::UnstablePollResponseEventContent,
                unstable_start::{
                    NewUnstablePollStartEventContent, UnstablePollAnswer, UnstablePollAnswers,
                    UnstablePollStartContentBlock, UnstablePollStartEventContent,
                },
            },
            room::power_levels::{
                RoomPowerLevels, RoomPowerLevelsEventContent, RoomPowerLevelsSource,
            },
        },
    },
};
use matrix_sdk_ui::timeline::{EventTimelineItem, PollState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    AccountManager, AccountSelection, LoadedAccount, MatrixCoreError, account_room,
    resume_room_queue, selected_loaded_for,
};

/// Longest question Morpheus sends or shows, in characters.
pub const MAX_POLL_QUESTION_CHARS: usize = 300;
/// Longest option Morpheus sends or shows, in characters.
pub const MAX_POLL_ANSWER_CHARS: usize = 120;
/// Fewest options a poll created here may have.
pub const MIN_POLL_ANSWER_COUNT: usize = 2;
/// Most options a poll may have (the protocol limit).
pub const MAX_POLL_ANSWER_COUNT: usize = 20;

const MAX_POLL_EVENT_BYTES: usize = 16 * 1024;
const MAX_POLL_ANSWER_ID_BYTES: usize = 1024;
const MAX_POWER_EVENT_BYTES: usize = 512 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(20);

/// What the person typed into the create dialog; nothing is sent until it
/// validates and they choose Send.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PollDraft {
    pub question: String,
    pub answers: Vec<String>,
    pub max_selections: usize,
    pub disclosed: bool,
}

/// A draft that passed validation; only `PollDraft::validate` builds one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidPoll {
    question: String,
    answers: Vec<String>,
    max_selections: usize,
    disclosed: bool,
}

/// The transaction of one draft's create. Retrying the same draft with the
/// same key lets the homeserver drop a duplicate if the first send arrived.
#[derive(Clone, Debug)]
pub struct PollSendKey(OwnedTransactionId);

impl PollSendKey {
    pub fn new() -> Self {
        Self(TransactionId::new())
    }
}

impl Default for PollSendKey {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PollDraftError {
    #[error("Enter a question.")]
    QuestionMissing,
    #[error("Keep the question within {} characters.", MAX_POLL_QUESTION_CHARS)]
    QuestionTooLong,
    #[error("Add at least {} options.", MIN_POLL_ANSWER_COUNT)]
    TooFewAnswers,
    #[error("A poll can have at most {} options.", MAX_POLL_ANSWER_COUNT)]
    TooManyAnswers,
    #[error("Fill in or remove the empty option.")]
    AnswerMissing,
    #[error("Keep each option within {} characters.", MAX_POLL_ANSWER_CHARS)]
    AnswerTooLong,
    #[error("Each option must be different.")]
    DuplicateAnswer,
    #[error("Choose how many options people can pick.")]
    InvalidMaxSelections,
    #[error("This poll is too large to send.")]
    TooLarge,
}

/// Fixed failure categories. No server text, event content or identifiers
/// are retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PollError {
    #[error("The account changed before this poll action ran. Nothing was sent.")]
    AccountChanged,
    #[error("This poll is not available here anymore. Nothing was sent.")]
    Unavailable,
    #[error("This poll has already ended.")]
    Ended,
    #[error("That choice is not valid for this poll. Nothing was sent.")]
    InvalidChoice,
    #[error("Only the person who created this poll can end it.")]
    NotCreator,
    #[error("Your room permissions do not allow this. Nothing was sent.")]
    NotAllowed,
    #[error("Room permissions could not be checked. Nothing was sent.")]
    PermissionsUnknown,
    #[error("Morpheus is still checking saved messages for this room. Try again shortly.")]
    RecoveryPending,
    #[error("Sending is paused in this room because it holds data this version cannot read.")]
    RoomPaused,
    #[error("{0}")]
    Draft(PollDraftError),
    #[error("The server refused this. Nothing was sent.")]
    Rejected,
    #[error("The server is limiting requests. Wait a moment, then try again.")]
    RateLimited,
    #[error("This timed out before anything was sent.")]
    TimedOut,
    #[error("The connection failed while sending. It may still arrive, so check the poll first.")]
    Uncertain,
}

/// The poll part of a timeline row. Counts are withheld (`None`) while the
/// results are undisclosed and the poll is open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollInfo {
    pub question: String,
    pub answers: Vec<PollAnswerInfo>,
    /// At most the number of answers; zero means the poll accepts no votes.
    pub max_selections: u32,
    pub disclosed: bool,
    pub ended: bool,
    pub edited: bool,
    /// Accounts with a counted vote, or `None` while results are withheld.
    pub voters: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollAnswerInfo {
    /// Opaque answer ID; only sent back in a vote.
    pub id: String,
    pub text: String,
    /// Counted votes, or `None` while results are withheld.
    pub votes: Option<u32>,
    /// Has the most votes (more than zero) and results are visible.
    pub leading: bool,
    /// This account's counted vote includes this answer.
    pub chosen: bool,
}

impl PollInfo {
    /// Results may be shown: disclosed polls while open, every poll once ended.
    pub fn results_visible(&self) -> bool {
        self.disclosed || self.ended
    }

    /// The answer IDs this account's counted vote selects.
    pub fn own_selection(&self) -> Vec<&str> {
        self.answers
            .iter()
            .filter(|answer| answer.chosen)
            .map(|answer| answer.id.as_str())
            .collect()
    }
}

impl PollDraft {
    pub fn validate(&self) -> Result<ValidPoll, PollDraftError> {
        let question = clean_text(&self.question);
        if question.is_empty() {
            return Err(PollDraftError::QuestionMissing);
        }
        if question.chars().count() > MAX_POLL_QUESTION_CHARS {
            return Err(PollDraftError::QuestionTooLong);
        }
        if self.answers.len() > MAX_POLL_ANSWER_COUNT {
            return Err(PollDraftError::TooManyAnswers);
        }
        let mut answers: Vec<String> = Vec::with_capacity(self.answers.len());
        for answer in &self.answers {
            let answer = clean_text(answer);
            if answer.is_empty() {
                return Err(PollDraftError::AnswerMissing);
            }
            if answer.chars().count() > MAX_POLL_ANSWER_CHARS {
                return Err(PollDraftError::AnswerTooLong);
            }
            let folded = answer.to_lowercase();
            if answers.iter().any(|known| known.to_lowercase() == folded) {
                return Err(PollDraftError::DuplicateAnswer);
            }
            answers.push(answer);
        }
        if answers.len() < MIN_POLL_ANSWER_COUNT {
            return Err(PollDraftError::TooFewAnswers);
        }
        if self.max_selections == 0 || self.max_selections > answers.len() {
            return Err(PollDraftError::InvalidMaxSelections);
        }
        Ok(ValidPoll {
            question,
            answers,
            max_selections: self.max_selections,
            disclosed: self.disclosed,
        })
    }
}

impl ValidPoll {
    /// The MSC3381 start content with fresh opaque answer IDs and a plain
    /// text fallback for clients without poll support.
    fn start_content(&self) -> Result<NewUnstablePollStartEventContent, PollError> {
        let answers: Vec<UnstablePollAnswer> = self
            .answers
            .iter()
            .map(|text| UnstablePollAnswer::new(Uuid::new_v4().to_string(), text.as_str()))
            .collect();
        let answers = UnstablePollAnswers::try_from(answers)
            .map_err(|_| PollError::Draft(PollDraftError::TooManyAnswers))?;
        let mut block = UnstablePollStartContentBlock::new(self.question.as_str(), answers);
        block.kind = if self.disclosed {
            PollKind::Disclosed
        } else {
            PollKind::Undisclosed
        };
        block.max_selections = u32::try_from(self.max_selections)
            .map(UInt::from)
            .map_err(|_| PollError::Draft(PollDraftError::InvalidMaxSelections))?;
        let mut fallback = self.question.clone();
        for (index, answer) in self.answers.iter().enumerate() {
            fallback.push_str(&format!("\n{}. {answer}", index + 1));
        }
        Ok(NewUnstablePollStartEventContent::plain_text(
            fallback, block,
        ))
    }
}

impl AccountManager {
    /// Send a new poll to the room, after validation and the room's
    /// permission and send gates. The poll appears from the server's echo.
    /// A retry of the same draft must pass the same `key`.
    pub async fn create_poll_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        draft: &PollDraft,
        key: &PollSendKey,
    ) -> Result<(), PollError> {
        let poll = draft.validate().map_err(PollError::Draft)?;
        let content = UnstablePollStartEventContent::New(poll.start_content()?);
        let encoded = serde_json::to_vec(&content).map_err(|_| PollError::Unavailable)?;
        if encoded.len() > MAX_POLL_EVENT_BYTES {
            return Err(PollError::Draft(PollDraftError::TooLarge));
        }
        let entered = AtomicBool::new(false);
        let work = async {
            let (loaded, room) = self.poll_room(selection, room_id).await?;
            if !may_send(&room, MessageLikeEventType::UnstablePollStart).await? {
                return Err(PollError::NotAllowed);
            }
            self.ready_to_send(selection, &loaded, &room).await?;
            entered.store(true, Ordering::Release);
            room.send(content)
                .with_transaction_id(key.0.clone())
                .await
                .map(drop)
                .map_err(|error| send_failure(&error))
        };
        selected_action(selection, &entered, work).await
    }

    /// Vote, or change this account's vote, on a poll shown in the room
    /// timeline or in the thread of `thread_root`.
    pub async fn vote_poll_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
        poll_event_id: &str,
        answers: &[String],
    ) -> Result<(), PollError> {
        let event_id = EventId::parse(poll_event_id).map_err(|_| PollError::Unavailable)?;
        let entered = AtomicBool::new(false);
        let work = async {
            let (loaded, room) = self.poll_room(selection, room_id).await?;
            let item = poll_item(&loaded, room_id, thread_root, &event_id).await?;
            let poll = item.content().as_poll().ok_or(PollError::Unavailable)?;
            let results = poll.results();
            let known: Vec<&str> = results
                .answers
                .iter()
                .map(|answer| answer.id.as_str())
                .collect();
            let ended = results.end_time.is_some();
            check_vote(&known, results.max_selections, ended, answers)?;
            if !may_send(&room, MessageLikeEventType::UnstablePollResponse).await? {
                return Err(PollError::NotAllowed);
            }
            self.ready_to_send(selection, &loaded, &room).await?;
            let selections = answers.to_vec();
            let content = UnstablePollResponseEventContent::new(selections, event_id.clone());
            entered.store(true, Ordering::Release);
            room.send(content)
                .await
                .map(drop)
                .map_err(|error| send_failure(&error))
        };
        selected_action(selection, &entered, work).await
    }

    /// End one of this account's own open polls. The composer exposes only
    /// creator ends; received creator and authorized moderator ends are
    /// checked by the patched timeline aggregation path.
    pub async fn end_poll_for(
        &self,
        selection: &AccountSelection,
        room_id: &str,
        thread_root: Option<&str>,
        poll_event_id: &str,
    ) -> Result<(), PollError> {
        let event_id = EventId::parse(poll_event_id).map_err(|_| PollError::Unavailable)?;
        let entered = AtomicBool::new(false);
        let work = async {
            let (loaded, room) = self.poll_room(selection, room_id).await?;
            let item = poll_item(&loaded, room_id, thread_root, &event_id).await?;
            if !item.is_own() || item.sender() != room.own_user_id() {
                return Err(PollError::NotCreator);
            }
            let poll = item.content().as_poll().ok_or(PollError::Unavailable)?;
            let results = poll.results();
            if results.end_time.is_some() {
                return Err(PollError::Ended);
            }
            let answers: Vec<(&str, &str)> = results
                .answers
                .iter()
                .map(|answer| (answer.id.as_str(), answer.text.as_str()))
                .collect();
            let text = end_text(&answers, &results.votes);
            if !may_send(&room, MessageLikeEventType::UnstablePollEnd).await? {
                return Err(PollError::NotAllowed);
            }
            self.ready_to_send(selection, &loaded, &room).await?;
            let content = UnstablePollEndEventContent::new(text, event_id.clone());
            entered.store(true, Ordering::Release);
            room.send(content)
                .await
                .map(drop)
                .map_err(|error| send_failure(&error))
        };
        selected_action(selection, &entered, work).await
    }

    fn owns(&self, selection: &AccountSelection) -> bool {
        selection.is_current()
            && self
                .account_selection(selection.account_id())
                .is_some_and(|current| current.same_epoch(selection))
    }

    /// The joined room of this exact selection epoch. The switch lock is
    /// released before any timeline or network work.
    async fn poll_room(
        &self,
        selection: &AccountSelection,
        room_id: &str,
    ) -> Result<(Arc<LoadedAccount>, Room), PollError> {
        if !self.owns(selection) {
            return Err(PollError::AccountChanged);
        }
        let state = selection
            .guard(self.state.lock())
            .await
            .ok_or(PollError::AccountChanged)?;
        // Waiting for the lock may span A -> B -> A; the ID alone is not authority.
        if !self.owns(selection) {
            return Err(PollError::AccountChanged);
        }
        let loaded = selected_loaded_for(&state, selection.account_id())
            .map_err(core_failure)?
            .clone();
        let room = account_room(&state, selection.account_id(), room_id).map_err(core_failure)?;
        drop(state);
        if room.state() != RoomState::Joined {
            return Err(PollError::Unavailable);
        }
        Ok((loaded, room))
    }

    /// The last checks before a send: the same selection epoch, still joined,
    /// and the room's existing discovery/recovery send gate.
    async fn ready_to_send(
        &self,
        selection: &AccountSelection,
        loaded: &LoadedAccount,
        room: &Room,
    ) -> Result<(), PollError> {
        if !self.owns(selection) {
            return Err(PollError::AccountChanged);
        }
        if let Err(error) = resume_room_queue(loaded, room).await {
            return Err(core_failure(error));
        }
        if !self.owns(selection) {
            return Err(PollError::AccountChanged);
        }
        if room.state() != RoomState::Joined {
            return Err(PollError::Unavailable);
        }
        Ok(())
    }
}

/// The poll start item as the timeline the person saw it in holds it now.
async fn poll_item(
    loaded: &LoadedAccount,
    room_id: &str,
    thread_root: Option<&str>,
    event_id: &EventId,
) -> Result<EventTimelineItem, PollError> {
    let timeline = match thread_root {
        Some(root) => loaded.thread_timeline(room_id, root).await,
        None => loaded.timeline(room_id).await,
    }
    .map_err(|_| PollError::Unavailable)?;
    timeline
        .item_by_event_id(event_id)
        .await
        .ok_or(PollError::Unavailable)
}

/// A vote must name at least one known answer, each once, within the poll's
/// limit, while the poll is open.
fn check_vote(
    known: &[&str],
    max_selections: u64,
    ended: bool,
    chosen: &[String],
) -> Result<(), PollError> {
    if ended {
        return Err(PollError::Ended);
    }
    let count = u64::try_from(chosen.len()).unwrap_or(u64::MAX);
    if chosen.is_empty() || count > max_selections {
        return Err(PollError::InvalidChoice);
    }
    let mut known_ids = HashSet::new();
    if known.len() > MAX_POLL_ANSWER_COUNT
        || known.iter().any(|id| {
            id.is_empty() || id.len() > MAX_POLL_ANSWER_ID_BYTES || !known_ids.insert(*id)
        })
    {
        return Err(PollError::InvalidChoice);
    }
    let mut seen = HashSet::new();
    for id in chosen {
        if id.len() > MAX_POLL_ANSWER_ID_BYTES
            || !seen.insert(id.as_str())
            || !known_ids.contains(id.as_str())
        {
            return Err(PollError::InvalidChoice);
        }
    }
    Ok(())
}

/// Predict from one validated power-level snapshot whether this account may
/// send `event_type`, and in an encrypted room also the encrypted wrapper the
/// server actually checks. The server stays the authority; an unreadable
/// snapshot refuses rather than guesses.
async fn may_send(room: &Room, event_type: MessageLikeEventType) -> Result<bool, PollError> {
    let rules = room
        .version()
        .and_then(|version| version.rules())
        .ok_or(PollError::PermissionsUnknown)?;
    let creators = room.creators().ok_or(PollError::PermissionsUnknown)?;
    let state = room
        .get_state_event_static::<RoomPowerLevelsEventContent>()
        .await
        .map_err(|_| PollError::PermissionsUnknown)?;
    let levels = match state {
        // A room without power levels uses the specification's defaults.
        None => RoomPowerLevels::new(RoomPowerLevelsSource::None, &rules.authorization, creators),
        Some(RawSyncOrStrippedState::Sync(raw)) => {
            if raw.json().get().len() > MAX_POWER_EVENT_BYTES {
                return Err(PollError::PermissionsUnknown);
            }
            let content = raw
                .get_field::<serde_json::Value>("content")
                .map_err(|_| PollError::PermissionsUnknown)?
                .ok_or(PollError::PermissionsUnknown)?;
            power_content_shape(&content)?;
            raw.deserialize()
                .map_err(|_| PollError::PermissionsUnknown)?
                .power_levels(&rules.authorization, creators)
        }
        Some(_) => return Err(PollError::PermissionsUnknown),
    };
    let encryption = room
        .latest_encryption_state()
        .await
        .map_err(|_| PollError::PermissionsUnknown)?;
    if encryption.is_unknown() {
        return Err(PollError::PermissionsUnknown);
    }
    let own = room.own_user_id();
    let mut allowed = levels.user_can_send_message(own, event_type);
    if encryption.is_encrypted() {
        allowed &= levels.user_can_send_message(own, MessageLikeEventType::RoomEncrypted);
    }
    Ok(allowed)
}

fn power_content_shape(content: &serde_json::Value) -> Result<(), PollError> {
    let object = content.as_object().ok_or(PollError::PermissionsUnknown)?;
    for field in ["users", "events"] {
        if object.get(field).is_some_and(|value| !value.is_object()) {
            return Err(PollError::PermissionsUnknown);
        }
    }
    Ok(())
}

fn core_failure(error: MatrixCoreError) -> PollError {
    match error {
        MatrixCoreError::SelectedAccountChanged | MatrixCoreError::NoActiveAccount => {
            PollError::AccountChanged
        }
        MatrixCoreError::RecoveryIncomplete => PollError::RecoveryPending,
        MatrixCoreError::RecoveryBlocked => PollError::RoomPaused,
        _ => PollError::Unavailable,
    }
}

// Keep only a fixed category. A gateway timeout or server failure can follow
// acceptance, so only a definite client rejection claims nothing was sent.
fn send_failure(error: &matrix_sdk::Error) -> PollError {
    match error.as_client_api_error() {
        Some(error)
            if error.status_code.as_u16() == 429
                || matches!(error.error_kind(), Some(ErrorKind::LimitExceeded(_))) =>
        {
            PollError::RateLimited
        }
        Some(error)
            if (400..500).contains(&error.status_code.as_u16())
                && error.status_code.as_u16() != 408 =>
        {
            PollError::Rejected
        }
        _ => PollError::Uncertain,
    }
}

/// Cancel pending lookup/permission/send waits with their exact selection.
/// Cancellation cannot recall a request already handed to the SDK/server;
/// never claim that such a request was not sent.
async fn selected_action(
    selection: &AccountSelection,
    entered: &AtomicBool,
    work: impl std::future::Future<Output = Result<(), PollError>>,
) -> Result<(), PollError> {
    match tokio::time::timeout(SEND_TIMEOUT, selection.guard(work)).await {
        Ok(Some(result)) => result,
        Ok(None) if entered.load(Ordering::Acquire) => Err(PollError::Uncertain),
        Ok(None) => Err(PollError::AccountChanged),
        Err(_) => Err(timed_out(entered)),
    }
}

fn timed_out(entered: &AtomicBool) -> PollError {
    if entered.load(Ordering::Acquire) {
        PollError::Uncertain
    } else {
        PollError::TimedOut
    }
}

/// Plain fallback text for the end event, from the results at the time of
/// ending (ending discloses them anyway).
fn end_text(answers: &[(&str, &str)], votes: &HashMap<String, Vec<String>>) -> String {
    let counts: Vec<usize> = answers
        .iter()
        .map(|(id, _)| votes.get(*id).map_or(0, Vec::len))
        .collect();
    let top = counts.iter().copied().max().unwrap_or(0);
    if top == 0 {
        return "The poll has ended with no votes.".into();
    }
    let leaders: Vec<String> = answers
        .iter()
        .zip(&counts)
        .filter(|(_, count)| **count == top)
        .map(|((_, text), _)| display_text(text, MAX_POLL_ANSWER_CHARS))
        .collect();
    if leaders.len() == 1 {
        format!("The poll has ended. Top answer: {}", leaders[0])
    } else {
        format!("The poll has ended. Top answers: {}", leaders.join(", "))
    }
}

/// The bounded poll projection of a timeline item.
pub(crate) fn poll_info(poll: &PollState, own_user_id: Option<&UserId>) -> PollInfo {
    let results = poll.results();
    let answers: Vec<(&str, &str)> = results
        .answers
        .iter()
        .map(|answer| (answer.id.as_str(), answer.text.as_str()))
        .collect();
    project(
        &results.question,
        &results.kind,
        results.max_selections,
        &answers,
        &results.votes,
        (results.end_time.is_some(), results.has_been_edited),
        own_user_id,
    )
}

fn project(
    question: &str,
    kind: &PollKind,
    max_selections: u64,
    answers: &[(&str, &str)],
    votes: &HashMap<String, Vec<String>>,
    (ended, edited): (bool, bool),
    own_user_id: Option<&UserId>,
) -> PollInfo {
    let disclosed = matches!(kind, PollKind::Disclosed);
    let visible = disclosed || ended;
    let answers = &answers[..answers.len().min(MAX_POLL_ANSWER_COUNT)];
    let counts: Vec<u32> = answers
        .iter()
        .map(|(id, _)| u32::try_from(voters_of(votes, id).len()).unwrap_or(u32::MAX))
        .collect();
    let top = counts.iter().copied().max().unwrap_or(0);
    let voters: HashSet<&str> = answers
        .iter()
        .flat_map(|(id, _)| voters_of(votes, id).iter().map(String::as_str))
        .collect();
    let own = own_user_id.map(UserId::as_str);
    let mut ids = HashSet::new();
    let usable_ids = answers
        .iter()
        .all(|(id, _)| !id.is_empty() && id.len() <= MAX_POLL_ANSWER_ID_BYTES && ids.insert(*id));
    let limit = if usable_ids {
        max_selections.min(u64::try_from(answers.len()).unwrap_or(0))
    } else {
        0
    };
    PollInfo {
        question: display_text(question, MAX_POLL_QUESTION_CHARS),
        answers: answers
            .iter()
            .zip(&counts)
            .map(|((id, text), count)| PollAnswerInfo {
                // Never truncate an opaque ID into a different vote target.
                // An unsupported poll stays readable but offers no vote.
                id: if usable_ids {
                    (*id).to_owned()
                } else {
                    String::new()
                },
                text: display_text(text, MAX_POLL_ANSWER_CHARS),
                votes: visible.then_some(*count),
                leading: visible && top > 0 && *count == top,
                chosen: usable_ids
                    && own.is_some_and(|own| voters_of(votes, id).iter().any(|user| user == own)),
            })
            .collect(),
        max_selections: u32::try_from(limit).unwrap_or(0),
        disclosed,
        ended,
        edited,
        voters: visible.then(|| u32::try_from(voters.len()).unwrap_or(u32::MAX)),
    }
}

fn voters_of<'a>(votes: &'a HashMap<String, Vec<String>>, id: &str) -> &'a [String] {
    votes.get(id).map_or(&[][..], Vec::as_slice)
}

fn hidden_character(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
    )
}

/// Typed text without control, invisible or direction-override characters.
fn clean_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() && !hidden_character(*character))
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Remote text as one bounded display line.
fn display_text(text: &str, max_chars: usize) -> String {
    let line: String = text
        .trim()
        .chars()
        .filter(|character| !hidden_character(*character))
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        // Remote text is never copied in full merely to display a short line.
        .take(max_chars.saturating_add(1))
        .collect();
    let line = line.trim();
    if line.chars().count() <= max_chars {
        return line.to_owned();
    }
    let mut bounded: String = line.chars().take(max_chars.saturating_sub(1)).collect();
    bounded.push('…');
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::user_id;

    #[test]
    fn revoked_poll_actions_release_pending_work_and_report_uncertain_sends() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let selections = crate::account_selection::Selections::default();
                for already_sending in [false, true] {
                    selections.replace(Some("synthetic-local"));
                    let selection = selections.for_account("synthetic-local").unwrap();
                    let entered = AtomicBool::new(already_sending);
                    let held = Arc::new(());
                    let waiting = held.clone();
                    let work = async move {
                        let _held = waiting;
                        std::future::pending::<Result<(), PollError>>().await
                    };
                    let mut action = Box::pin(selected_action(&selection, &entered, work));
                    assert!(futures_util::poll!(action.as_mut()).is_pending());
                    assert_eq!(Arc::strong_count(&held), 2);
                    selections.replace(Some("another-synthetic-local"));
                    selections.replace(Some("synthetic-local"));
                    let result = tokio::time::timeout(Duration::from_secs(2), action)
                        .await
                        .unwrap();
                    assert_eq!(
                        result,
                        Err(if already_sending {
                            PollError::Uncertain
                        } else {
                            PollError::AccountChanged
                        })
                    );
                    assert_eq!(Arc::strong_count(&held), 1);
                }
            });
    }

    fn draft(answers: &[&str], max_selections: usize) -> PollDraft {
        PollDraft {
            question: "  Lunch?\u{202e} ".into(),
            answers: answers.iter().map(|answer| (*answer).to_owned()).collect(),
            max_selections,
            disclosed: false,
        }
    }

    #[test]
    fn drafts_are_cleaned_and_bounded_before_anything_is_built() {
        let valid = draft(&["Soup", " Salad "], 1).validate().unwrap();
        assert_eq!(valid.question, "Lunch?");
        assert_eq!(valid.answers, ["Soup", "Salad"]);
        let long = "x".repeat(121);
        for (draft, error) in [
            (draft(&["A"], 1), PollDraftError::TooFewAnswers),
            (draft(&["A", ""], 1), PollDraftError::AnswerMissing),
            (draft(&["A", "a"], 1), PollDraftError::DuplicateAnswer),
            (draft(&["A", "B"], 0), PollDraftError::InvalidMaxSelections),
            (draft(&["A", "B"], 3), PollDraftError::InvalidMaxSelections),
            (draft(&[&long, "B"], 1), PollDraftError::AnswerTooLong),
        ] {
            assert_eq!(draft.validate(), Err(error));
        }
        let many: Vec<String> = (0..21).map(|index| index.to_string()).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(
            draft(&many, 1).validate(),
            Err(PollDraftError::TooManyAnswers)
        );
        let mut long = draft(&["Soup", "Salad"], 1);
        long.question = "q".repeat(301);
        assert_eq!(long.validate(), Err(PollDraftError::QuestionTooLong));
        long.question = " \n ".into();
        assert_eq!(long.validate(), Err(PollDraftError::QuestionMissing));
    }

    #[test]
    fn start_content_uses_the_aggregated_msc3381_shape() {
        let mut poll = draft(&["Soup", "Salad", "Pie"], 2);
        poll.disclosed = true;
        let content = poll.validate().unwrap().start_content().unwrap();
        let json = serde_json::to_value(&content).unwrap();
        let block = &json["org.matrix.msc3381.poll.start"];
        assert_eq!(block["kind"], "org.matrix.msc3381.poll.disclosed");
        assert_eq!(block["max_selections"], 2);
        assert_eq!(block["question"]["org.matrix.msc1767.text"], "Lunch?");
        let answers = block["answers"].as_array().unwrap();
        let ids: HashSet<&str> = answers
            .iter()
            .map(|answer| answer["id"].as_str().unwrap())
            .collect();
        assert_eq!((answers.len(), ids.len()), (3, 3));
        assert_eq!(
            json["org.matrix.msc1767.text"],
            "Lunch?\n1. Soup\n2. Salad\n3. Pie"
        );
    }

    #[test]
    fn votes_must_be_known_unique_within_the_limit_and_open() {
        let known = ["a", "b", "c"];
        let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
        assert_eq!(check_vote(&known, 2, false, &ids(&["a", "c"])), Ok(()));
        for (max, ended, chosen, error) in [
            (2, true, ids(&["a"]), PollError::Ended),
            (2, false, ids(&[]), PollError::InvalidChoice),
            (1, false, ids(&["a", "b"]), PollError::InvalidChoice),
            (2, false, ids(&["a", "a"]), PollError::InvalidChoice),
            (2, false, ids(&["z"]), PollError::InvalidChoice),
            (0, false, ids(&["a"]), PollError::InvalidChoice),
        ] {
            assert_eq!(check_vote(&known, max, ended, &chosen), Err(error));
        }
    }

    fn tallies() -> HashMap<String, Vec<String>> {
        HashMap::from([
            (
                "a".to_owned(),
                vec!["@me:x.org".to_owned(), "@ann:x.org".to_owned()],
            ),
            ("b".to_owned(), vec!["@bob:x.org".to_owned()]),
            ("c".to_owned(), Vec::new()),
        ])
    }

    #[test]
    fn undisclosed_results_stay_withheld_until_the_poll_ends() {
        let answers = [("a", "Soup"), ("b", "Salad"), ("c", "Pie")];
        let own = Some(user_id!("@me:x.org"));
        let open = project(
            "Lunch?",
            &PollKind::Undisclosed,
            1,
            &answers,
            &tallies(),
            (false, false),
            own,
        );
        assert!(!open.results_visible());
        assert_eq!(open.voters, None);
        assert!(open.answers.iter().all(|answer| answer.votes.is_none()));
        assert!(open.answers.iter().all(|answer| !answer.leading));
        // The person's own counted vote is theirs to see.
        assert_eq!(open.own_selection(), ["a"]);

        let ended = project(
            "Lunch?",
            &PollKind::Undisclosed,
            1,
            &answers,
            &tallies(),
            (true, false),
            own,
        );
        assert_eq!(ended.voters, Some(3));
        let counts: Vec<_> = ended.answers.iter().map(|answer| answer.votes).collect();
        assert_eq!(counts, [Some(2), Some(1), Some(0)]);
        assert!(ended.answers[0].leading && !ended.answers[2].leading);
    }

    #[test]
    fn projection_bounds_text_answers_and_selection_limit() {
        let ids: Vec<String> = (0..25).map(|index| format!("id{index}")).collect();
        let long = "w\u{202e}".repeat(400);
        let pairs: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), long.as_str())).collect();
        let info = project(
            &long,
            &PollKind::Disclosed,
            u64::MAX,
            &pairs,
            &HashMap::new(),
            (false, true),
            None,
        );
        assert_eq!(info.answers.len(), MAX_POLL_ANSWER_COUNT);
        assert_eq!(info.max_selections, 20);
        assert_eq!(info.question.chars().count(), MAX_POLL_QUESTION_CHARS);
        assert!(!info.question.contains('\u{202e}'));
        assert_eq!(info.answers[0].text.chars().count(), MAX_POLL_ANSWER_CHARS);
        assert_eq!(info.voters, Some(0));
        assert!(info.edited && info.answers.iter().all(|answer| !answer.chosen));
    }

    #[test]
    fn unsupported_remote_answer_ids_are_not_truncated_into_vote_targets() {
        let huge = "i".repeat(MAX_POLL_ANSWER_ID_BYTES + 1);
        for answers in [
            vec![(huge.as_str(), "A"), ("b", "B")],
            vec![("same", "A"), ("same", "B")],
        ] {
            let info = project(
                "Question",
                &PollKind::Disclosed,
                1,
                &answers,
                &HashMap::new(),
                (false, false),
                None,
            );
            assert_eq!(info.max_selections, 0);
            assert!(
                info.answers
                    .iter()
                    .all(|answer| answer.id.is_empty() && !answer.chosen)
            );
            let known: Vec<_> = answers.iter().map(|(id, _)| *id).collect();
            assert_eq!(
                check_vote(&known, 1, false, &["b".to_owned()]),
                Err(PollError::InvalidChoice)
            );
        }
        let long = "x".repeat(1_000_000);
        assert_eq!(
            display_text(&long, MAX_POLL_QUESTION_CHARS).chars().count(),
            MAX_POLL_QUESTION_CHARS
        );
    }

    #[test]
    fn end_text_names_the_leaders_or_no_votes() {
        let answers = [("a", "Soup"), ("b", "Salad"), ("c", "Pie")];
        assert_eq!(
            end_text(&answers, &tallies()),
            "The poll has ended. Top answer: Soup"
        );
        assert_eq!(
            end_text(&answers, &HashMap::new()),
            "The poll has ended with no votes."
        );
    }
}

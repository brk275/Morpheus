//! The poll card projection of one timeline row. It only reformats the
//! core's bounded `PollInfo`: counts the core withheld stay hidden, and a
//! poll that is still being sent, or has ended, offers no vote.

use morpheus_matrix::{MessageInfo, PollInfo};
use slint::{ModelRc, VecModel};

use crate::{PollCardData, PollOptionItem};

pub(crate) fn poll_card(info: &MessageInfo) -> PollCardData {
    let Some(poll) = info.poll.as_ref() else {
        return PollCardData::default();
    };
    let total: u64 = poll
        .answers
        .iter()
        .filter_map(|answer| answer.votes)
        .map(u64::from)
        .sum();
    let options: Vec<PollOptionItem> = poll
        .answers
        .iter()
        .map(|answer| PollOptionItem {
            id: answer.id.as_str().into(),
            text: answer.text.as_str().into(),
            votes: answer.votes.map(vote_label).unwrap_or_default().into(),
            share: share(answer.votes, total),
            voted: answer.chosen,
            leading: answer.leading,
        })
        .collect();
    let sent = info.event_id.is_some() && !info.is_local_echo;
    let has_vote = poll.answers.iter().any(|answer| answer.chosen);
    PollCardData {
        present: true,
        question: poll.question.as_str().into(),
        options: ModelRc::new(VecModel::from(options)),
        multiple: poll.max_selections > 1,
        hint: hint(poll).into(),
        status: status(poll, has_vote).into(),
        ended: poll.ended,
        has_vote,
        can_vote: sent && !poll.ended && poll.max_selections > 0,
        can_end: sent && info.is_own && !poll.ended,
    }
}

fn vote_label(votes: u32) -> String {
    if votes == 1 {
        "1 vote".into()
    } else {
        format!("{votes} votes")
    }
}

fn share(votes: Option<u32>, total: u64) -> f32 {
    match votes {
        Some(votes) if total > 0 => (f64::from(votes) / total as f64) as f32,
        _ => 0.0,
    }
}

fn hint(poll: &PollInfo) -> String {
    if poll.ended {
        "Poll ended".into()
    } else if poll.max_selections == 0 {
        "Voting is unavailable for this poll".into()
    } else if poll.max_selections == 1 {
        "Choose one option, then vote".into()
    } else {
        format!("Choose up to {} options, then vote", poll.max_selections)
    }
}

fn status(poll: &PollInfo, has_vote: bool) -> String {
    let voters = |count: u32| {
        if count == 1 {
            "1 person voted".to_owned()
        } else {
            format!("{count} people voted")
        }
    };
    let mut status = match (poll.ended, poll.voters) {
        (true, Some(count)) => format!("Final results · {}", voters(count)),
        (false, Some(count)) => format!("{} · Results update as votes arrive", voters(count)),
        (_, None) if has_vote => "You voted · Results are shown when the poll ends".to_owned(),
        (_, None) => "Results are shown when the poll ends".to_owned(),
    };
    if poll.edited {
        status.push_str(" · Edited");
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpheus_matrix::{MessageSendState, PollAnswerInfo};
    use slint::Model;

    fn answer(id: &str, votes: Option<u32>, chosen: bool) -> PollAnswerInfo {
        PollAnswerInfo {
            id: id.into(),
            text: format!("Option {id}"),
            votes,
            leading: votes == Some(2),
            chosen,
        }
    }

    fn row(poll: PollInfo, own: bool, event_id: Option<&str>) -> MessageInfo {
        MessageInfo {
            key: "poll".into(),
            event_id: event_id.map(Into::into),
            sender: "@me:example.org".into(),
            body: format!("Poll: {}", poll.question),
            rich_markdown: None,
            rich_accessible: None,
            attachment_name: None,
            attachment_is_image: false,
            attachment_is_audio: false,
            poll: Some(poll),
            reply_preview: None,
            reaction_summary: String::new(),
            timestamp_ms: 0,
            is_own: own,
            is_local_echo: event_id.is_none(),
            send_state: MessageSendState::Sent,
        }
    }

    fn poll(ended: bool, disclosed: bool) -> PollInfo {
        let visible = ended || disclosed;
        PollInfo {
            question: "Lunch?".into(),
            answers: vec![
                answer("a", visible.then_some(2), true),
                answer("b", visible.then_some(1), false),
            ],
            max_selections: 1,
            disclosed,
            ended,
            edited: false,
            voters: visible.then_some(3),
        }
    }

    #[test]
    fn withheld_results_show_no_counts_or_bars() {
        let card = poll_card(&row(poll(false, false), false, Some("$poll")));
        assert!(card.present && card.can_vote && !card.can_end && card.has_vote);
        for option in card.options.iter() {
            assert!(option.votes.is_empty());
            assert_eq!(option.share, 0.0);
        }
        assert_eq!(
            card.status.as_str(),
            "You voted · Results are shown when the poll ends"
        );
    }

    #[test]
    fn ended_polls_show_final_results_and_offer_no_actions() {
        let card = poll_card(&row(poll(true, false), true, Some("$poll")));
        assert!(!card.can_vote && !card.can_end);
        let first = card.options.row_data(0).unwrap();
        assert_eq!(first.votes.as_str(), "2 votes");
        assert!((first.share - 2.0 / 3.0).abs() < 0.001);
        assert_eq!(card.status.as_str(), "Final results · 3 people voted");
    }

    #[test]
    fn only_sent_polls_can_be_voted_on_and_only_their_creator_ends_them() {
        let pending = poll_card(&row(poll(false, true), true, None));
        assert!(!pending.can_vote && !pending.can_end);
        let own = poll_card(&row(poll(false, true), true, Some("$poll")));
        assert!(own.can_vote && own.can_end);
        let other = poll_card(&row(poll(false, true), false, Some("$poll")));
        assert!(other.can_vote && !other.can_end);
        assert_eq!(
            poll_card(&row(poll(false, true), false, None)).question,
            "Lunch?"
        );
        let mut plain = row(poll(false, true), false, Some("$text"));
        plain.poll = None;
        assert!(!poll_card(&plain).present);
    }
}

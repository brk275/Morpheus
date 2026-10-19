//! Native Matrix polls: timeline poll cards projected from the SDK's
//! aggregated poll state, explicit vote, change-vote and end actions, and the
//! create dialog. Sending, permission checks and protocol details live in
//! morpheus-matrix; this module only presents and fences them.

mod card;
mod controller;

pub(crate) use card::poll_card;
pub(crate) use controller::{clear_presentation, register};

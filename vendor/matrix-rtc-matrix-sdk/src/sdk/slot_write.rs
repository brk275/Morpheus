// Copyright Morpheus contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::future::Future;

use matrix_rtc_core::CommandError;
use matrix_sdk::{HttpError, ruma::api::error::ErrorKind};

const STABLE: &str = "m.rtc.slot";
const UNSTABLE: &str = "org.matrix.msc4143.rtc.slot";

/// Preserve a present stable slot's authority; a failed read is never absence.
/// There is one actual write and no alternate-type retry after that write.
pub(super) async fn send<R, RF, W, WF>(
    event_type: String,
    read_stable: R,
    admit: &(dyn Fn() -> bool + Send + Sync),
    write: W,
) -> Result<String, CommandError>
where
    R: FnOnce() -> RF,
    RF: Future<Output = Result<(), HttpError>>,
    W: FnOnce(String) -> WF,
    WF: Future<Output = Result<String, CommandError>>,
{
    // RoomState memberships and all other state traffic keep their old path.
    if event_type != STABLE && event_type != UNSTABLE {
        return write(event_type).await;
    }
    if !admit() {
        return Err(CommandError::from_message(
            "RTC slot write admission closed",
        ));
    }
    let authority = match read_stable().await {
        // Presence includes closed/malformed content, as in the feeder's
        // stable-priority rule. Content does not grant any new authority.
        Ok(()) => STABLE,
        Err(error)
            if error.as_client_api_error().is_some_and(|answer| {
                answer.status_code.as_u16() == 404
                    && answer.error_kind() == Some(&ErrorKind::NotFound)
            }) =>
        {
            UNSTABLE
        }
        Err(_) => return Err(CommandError::from_message("RTC slot authority read failed")),
    };
    // The original account/room predicate survives the entered read and is
    // checked at the new actual side-effect boundary. No authority is renewed.
    if !admit() {
        return Err(CommandError::from_message(
            "RTC slot write admission closed",
        ));
    }
    write(authority.to_owned()).await
}

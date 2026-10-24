use super::{
    CallbackBody, Cancellation, Failure, Identity, Mode, NativeRetirement, OperationFailure,
    Report, Retirement,
};
use crate::{CloseReason, Notification, NotificationResponse};
use mac_notification_sys::owned::{self, Response};

impl Notification {
    /// Run one owned plain macOS banner on the caller's actual native worker.
    ///
    /// This is the existing NSUserNotificationCenter product route, with exact
    /// registration required and no Finder fallback or submit-on-Drop handle.
    /// No thread/timer is created. Every success and failure preserves the
    /// backend's actual identity-retention facts; worker/TLS/supervisor joins
    /// and original submission/fresh protected click authority are caller duties.
    pub fn show_owned_with_response(
        mut self,
        identity: Identity,
        cancellation: Cancellation,
        mode: Mode,
        handover: impl FnOnce() -> bool,
        submitted: impl FnOnce(),
        response: impl FnOnce(NotificationResponse),
    ) -> Result<Report, Failure> {
        // Current app banners are plain text/default sound/contents click. Do
        // not silently substitute this owned API for preview UN/action/image
        // options. All existing legacy APIs retain their original features.
        if cfg!(feature = "preview-macos-un")
            || !self.actions.is_empty()
            || self.path_to_image.is_some()
            || self.sound_name.as_deref().is_some_and(|sound| {
                !sound.is_empty() && sound != "NSUserNotificationDefaultSoundName"
            })
        {
            return Err(Failure {
                operation: OperationFailure::Preparation,
                retirement: Retirement {
                    native: NativeRetirement::NotEntered,
                    callback_body: CallbackBody::NotEntered,
                },
            });
        }
        let title = std::mem::take(&mut self.summary);
        let body = std::mem::take(&mut self.body);
        let subtitle = self.subtitle.take().unwrap_or_default();
        let sound = self.sound_name.as_deref() == Some("NSUserNotificationDefaultSoundName");
        drop(self);
        owned::run_owned(
            identity.bytes(),
            title,
            subtitle,
            body,
            sound,
            mode == Mode::Response,
            || cancellation.is_cancelled(),
            handover,
            submitted,
            move |event| {
                response(match event {
                    Response::Click => NotificationResponse::Default,
                    Response::Dismissed => NotificationResponse::Closed(CloseReason::Dismissed),
                })
            },
        )
    }
}

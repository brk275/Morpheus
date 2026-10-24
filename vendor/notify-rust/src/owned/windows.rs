use super::{Cancellation, Failure, Identity, Mode, OperationFailure, Report, Retirement};
use crate::{Notification, NotificationResponse};
use winrt_notification::owned::{Identity as WindowsIdentity, Response};

impl Notification {
    /// Run one owned Windows notification on the caller's actual native worker.
    ///
    /// No worker or detached listener is created here. The caller reserves its
    /// bounded identity before entering, publishes only `submitted` as the
    /// actual Show receipt, and must join the worker/TLS and exact supervisor
    /// separately. An early native response is retained until Show returns.
    ///
    /// `handover` consumes original app/account submission authority. An already
    /// submitted message response is a fresh protected intent; the response
    /// callback must still validate the saved exact record and current app gate.
    /// No new message submit, Answer, capture, or SDK authority comes from it.
    pub fn show_owned_with_response(
        self,
        identity: Identity,
        cancellation: Cancellation,
        mode: Mode,
        handover: impl FnOnce() -> bool,
        submitted: impl FnOnce(),
        response: impl FnOnce(NotificationResponse),
    ) -> Result<Report, Failure> {
        fn unsupported() -> Failure {
            Failure {
                operation: Some(OperationFailure::Unsupported),
                retirement: Retirement::default(),
            }
        }
        // No PowerShell identity fallback exists in this owned path. The app
        // supplies its exact registered AUMID before reaching the worker.
        if self
            .app_id
            .as_ref()
            .is_none_or(|id| id.is_empty() || id.len() > 128)
            || self.summary.len() > 4096
            || self.body.len() > 65536
            || self
                .subtitle
                .as_ref()
                .is_some_and(|subtitle| subtitle.len() > 4096)
            || self.path_to_image.is_some()
            || self.id.is_some()
            || self
                .sound_name
                .as_ref()
                .is_some_and(|sound| sound.len() > 256)
            || self.actions.len() % 2 != 0
            || self.actions.len() > 16
        {
            return Err(unsupported());
        }
        let (tag, group) = identity.windows_parts();
        let identity = WindowsIdentity::new(&tag, &group).ok_or_else(unsupported)?;
        let mut toast = crate::windows::build_toast(&self);
        let mut actions = Vec::with_capacity(self.actions.len() / 2);
        for pair in self.actions.chunks_exact(2) {
            let key = &pair[0];
            if key.is_empty() || key.encode_utf16().take(65).count() > 64 || pair[1].len() > 4096 {
                return Err(unsupported());
            }
            toast = toast.add_button(&pair[1], key);
            actions.push(key.clone());
        }
        // Only the native template retains native text after it is built;
        // no Rust Notification clone lives through the response wait.
        drop(self);
        toast.run_owned(
            identity,
            mode == Mode::Response,
            || cancellation.is_cancelled(),
            handover,
            submitted,
            move |event| {
                let event = match event {
                    Response::Default => NotificationResponse::Default,
                    Response::Action(index) => {
                        let Some(key) = actions.get(index) else {
                            return;
                        };
                        NotificationResponse::Action(key.clone())
                    }
                    Response::Dismissed(reason) => NotificationResponse::Closed(reason.into()),
                };
                response(event);
            },
        )
    }
}

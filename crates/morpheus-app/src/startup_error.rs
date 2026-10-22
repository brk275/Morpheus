//! A closed startup state: no account actions or recovery mutations are wired.

use morpheus_matrix::MatrixCoreError;
use slint::ComponentHandle;

use crate::StartupErrorWindow;

pub(crate) fn message(error: &MatrixCoreError) -> &'static str {
    match error {
        MatrixCoreError::AppPasswordUnavailable => {
            "Account protection information could not be read securely. Restore a complete, trusted profile backup or restore access to your operating system's credential vault, then restart Morpheus. Your saved accounts will stay closed."
        }
        MatrixCoreError::AccountAccessBusy => {
            "Another Morpheus process may be using your saved accounts. Close other Morpheus instances, then restart the app."
        }
        _ => {
            "Morpheus could not open its local account data. Close the app and check access to its application data folder before trying again."
        }
    }
}

pub(crate) fn show(error: &MatrixCoreError) -> Result<(), slint::PlatformError> {
    let window = StartupErrorWindow::new()?;
    // Never show the underlying error: it can contain paths or server input.
    window.set_detail(message(error).into());
    window.on_exit_requested(|| {
        let _ = slint::quit_event_loop();
    });
    window.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protection_and_busy_states_have_actionable_fixed_messages() {
        let protection = message(&MatrixCoreError::AppPasswordUnavailable);
        assert!(protection.contains("credential vault"));
        assert!(protection.contains("trusted profile backup"));
        assert!(protection.contains("saved accounts will stay closed"));
        let busy = message(&MatrixCoreError::AccountAccessBusy);
        assert!(busy.contains("Close other Morpheus instances"));
        assert_ne!(protection, busy);
    }

    #[test]
    fn payload_bearing_errors_never_enter_the_startup_message() {
        let secret = "synthetic-private-account-token";
        let first = message(&MatrixCoreError::InvalidHomeserver(secret.into()));
        let second = message(&MatrixCoreError::AccountNotFound(secret.into()));
        assert_eq!(first, second);
        assert!(!first.contains(secret));
    }
}

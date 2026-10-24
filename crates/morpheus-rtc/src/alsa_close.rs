//! Linux-only checked ALSA close. Primary-handle consumption is not native cleanup success.
//! Only native zero plus real worker/TLS joins permits release; a negative status
//! remains terminal even though retrying the consumed pointer would be invalid.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(3);
static OWNER: Mutex<Option<CloseOwner>> = Mutex::new(None);
static POISONED: AtomicBool = AtomicBool::new(false);
static TERMINAL: Mutex<Option<AlsaBackendCloseError>> = Mutex::new(None);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlsaBackendCloseError {
    TimedOut,
    Pending,
    NativeClose,
    WorkerPanicked,
    SupervisorFailed,
}
impl std::fmt::Display for AlsaBackendCloseError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Pending => "ALSA native owner has not yet completed its close",
            Self::TimedOut => "ALSA native close is still owned",
            Self::NativeClose => "ALSA native cleanup failed and remains owned",
            Self::WorkerPanicked => "ALSA close worker or TLS panicked",
            Self::SupervisorFailed => "ALSA close ownership could not be confirmed",
        })
    }
}
impl std::error::Error for AlsaBackendCloseError {}
struct CloseOwner {
    supervisor: Option<JoinHandle<()>>,
    completed: Arc<Mutex<Option<Result<(), AlsaBackendCloseError>>>>,
    require_empty: bool,
}

/// Only from an already retained native opener/ring/attachment worker.
/// A failure keeps the affected actual CPAL PCM/worker/operation owner in its native registry.
pub fn close_retired_alsa_on_owned_worker() -> Result<(), AlsaBackendCloseError> {
    close_blocking(false)
}
fn close_blocking(require_empty: bool) -> Result<(), AlsaBackendCloseError> {
    if let Some(error) = *TERMINAL.lock().unwrap_or_else(|e| e.into_inner()) {
        return Err(error);
    }
    if POISONED.load(Ordering::Acquire) {
        return Err(AlsaBackendCloseError::WorkerPanicked);
    }
    loop {
        let result = if require_empty {
            cpal::drain_owned_alsa(Duration::from_secs(5))
        } else {
            cpal::drain_retired_owned_alsa(Duration::from_secs(5))
        };
        let result = result.map_err(|error| match error {
            cpal::AlsaCloseError::Pending => AlsaBackendCloseError::Pending,
            cpal::AlsaCloseError::Native { .. }
            | cpal::AlsaCloseError::Open { .. }
            | cpal::AlsaCloseError::Transport { .. } => AlsaBackendCloseError::NativeClose,
            cpal::AlsaCloseError::OwnerPanicked | cpal::AlsaCloseError::CallbackPanicked => {
                AlsaBackendCloseError::WorkerPanicked
            }
            cpal::AlsaCloseError::Reentrant
            | cpal::AlsaCloseError::Ownership
            | cpal::AlsaCloseError::Active => AlsaBackendCloseError::SupervisorFailed,
        });
        // Pending only waits for the SAME already retained native control owner/join.
        // All actual API failures are terminal; no native phase is reissued after error.
        if result != Err(AlsaBackendCloseError::Pending) {
            return result;
        }
    }
}

fn begin(owner: &mut Option<CloseOwner>, require_empty: bool) -> Result<(), AlsaBackendCloseError> {
    if let Some(error) = *TERMINAL.lock().unwrap_or_else(|e| e.into_inner()) {
        return Err(error);
    }
    if POISONED.load(Ordering::Acquire) {
        return Err(AlsaBackendCloseError::WorkerPanicked);
    }
    if owner.is_some() {
        return Ok(());
    }
    let completed = Arc::new(Mutex::new(None));
    let result = completed.clone();
    let (start, admitted) = std::sync::mpsc::sync_channel(1);
    let supervisor = std::thread::Builder::new()
        .name("morpheus-alsa-close-owner".into())
        .spawn(move || {
            // Until global publication succeeds, no actual native close thread exists.
            if admitted.recv().is_err() {
                return;
            }
            let actual = std::thread::Builder::new()
                .name("morpheus-alsa-close".into())
                .spawn(move || close_blocking(require_empty));
            let closed = match actual {
                Err(_) => Err(AlsaBackendCloseError::SupervisorFailed),
                Ok(actual) => match actual.join() {
                    Ok(result) => result,
                    Err(_) => {
                        POISONED.store(true, Ordering::Release);
                        Err(AlsaBackendCloseError::WorkerPanicked)
                    }
                },
            };
            *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(closed);
        })
        .map_err(|_| AlsaBackendCloseError::SupervisorFailed)?;
    *owner = Some(CloseOwner {
        supervisor: Some(supervisor),
        completed,
        require_empty,
    });
    start
        .send(())
        .map_err(|_| AlsaBackendCloseError::SupervisorFailed)
}
fn poll(owner: &mut Option<CloseOwner>) -> Option<Result<(), AlsaBackendCloseError>> {
    let active = owner.as_mut()?;
    let Some(supervisor) = active.supervisor.as_ref() else {
        POISONED.store(true, Ordering::Release);
        return Some(Err(AlsaBackendCloseError::SupervisorFailed));
    };
    if !supervisor.is_finished() {
        return None;
    }
    // Actual worker body AND TLS have already been joined on this supervisor.
    // Final supervisor epilogue has no hard OS scheduling guarantee.
    if active
        .supervisor
        .take()
        .expect("retained close supervisor")
        .join()
        .is_err()
    {
        POISONED.store(true, Ordering::Release);
        return Some(Err(AlsaBackendCloseError::SupervisorFailed));
    }
    let result = *active.completed.lock().unwrap_or_else(|e| e.into_inner());
    let Some(result) = result else {
        POISONED.store(true, Ordering::Release);
        return Some(Err(AlsaBackendCloseError::SupervisorFailed));
    };
    // Only Pending may later OBSERVE the same still-retained CPAL native owner.
    // A nonzero native API result has uncertain effects; no blind phase retry.
    if let Err(error) = result {
        if error != AlsaBackendCloseError::Pending {
            *TERMINAL.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
        }
    }
    *owner = None;
    Some(result)
}

/// Called after revocation and native values have been retired. Dropping this future
/// retains all actual handles. It never blocks a Tokio/UI thread on native close/join.
pub async fn wait_for_retired_alsa_release() -> Result<(), AlsaBackendCloseError> {
    wait(false).await
}
/// Final ordinary shutdown check after all call/ring/attachment owners were revoked and joined.
/// An unexpectedly still-active native stream is a failed ownership check, never auto-closed.
pub async fn wait_for_all_alsa_release() -> Result<(), AlsaBackendCloseError> {
    wait(true).await
}
async fn wait(require_empty: bool) -> Result<(), AlsaBackendCloseError> {
    {
        let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
        begin(&mut owner, require_empty)?;
    }
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        {
            let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
            // A concurrent observer may have consumed a completed receipt. Perform
            // our own checked empty/retired observation rather than timing out on None.
            if owner.is_none() {
                begin(&mut owner, require_empty)?;
            }
            let full = owner.as_ref().is_some_and(|owner| owner.require_empty);
            if let Some(result) = poll(&mut owner) {
                if result.is_err() || !require_empty || full {
                    return result;
                }
                begin(&mut owner, true)?;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(AlsaBackendCloseError::TimedOut);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_retired_observers_each_consume_a_real_close_worker_join() {
        // This synthetic case owns no native units: it exercises real coordinator
        // publication/join and simultaneous observers, not hardware cleanup.
        let (first, second) = tokio::join!(
            wait_for_retired_alsa_release(),
            wait_for_retired_alsa_release()
        );
        assert_eq!(first, Ok(()));
        assert_eq!(second, Ok(()));
        assert_eq!(wait_for_all_alsa_release().await, Ok(()));
    }
}

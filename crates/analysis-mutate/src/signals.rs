use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::CancellationToken;

static PROCESS_CANCELLATION: OnceLock<CancellationToken> = OnceLock::new();
static HANDLER_INSTALLATION: OnceLock<Result<(), SignalHandlerError>> = OnceLock::new();
static COOPERATIVE_CANCELLATION_ACTIVE: AtomicBool = AtomicBool::new(false);

const FORCED_CANCELLATION_EXIT: i32 = 130;

/// Failure to install the process-wide operating-system signal handler.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct SignalHandlerError {
    message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalDisposition {
    CancelCooperatively,
    ExitImmediately,
}

/// Return the process-wide cooperative cancellation token.
#[must_use]
pub fn process_cancellation_token() -> CancellationToken {
    PROCESS_CANCELLATION.get_or_init(CancellationToken::new).clone()
}

/// Install the process-wide cancellation handler once.
///
/// With the `ctrlc` termination feature this covers SIGINT and SIGTERM on
/// Unix, plus console Ctrl-C on Windows. During a supervised external command
/// or mutation execution, the first signal requests cooperative cancellation
/// so the child tree can be killed and source guards can restore; a repeated
/// signal forces exit. Outside those narrow windows, a signal exits immediately
/// instead of being swallowed by the process-wide handler.
///
/// # Errors
///
/// Returns an error if the platform handler cannot be installed.
pub fn install_signal_handlers() -> Result<(), SignalHandlerError> {
    HANDLER_INSTALLATION.get_or_init(install_process_handler).clone()
}

fn install_process_handler() -> Result<(), SignalHandlerError> {
    let cancellation = process_cancellation_token();
    ctrlc::set_handler(move || handle_termination_signal(&cancellation)).map_err(|error| SignalHandlerError {
        message: error.to_string(),
    })
}

fn handle_termination_signal(cancellation: &CancellationToken) {
    let active = COOPERATIVE_CANCELLATION_ACTIVE.load(Ordering::SeqCst);
    match signal_disposition(active, cancellation) {
        SignalDisposition::CancelCooperatively => cancellation.cancel(),
        SignalDisposition::ExitImmediately => std::process::exit(FORCED_CANCELLATION_EXIT),
    }
}

fn signal_disposition(active: bool, cancellation: &CancellationToken) -> SignalDisposition {
    if active && !cancellation.is_cancelled() {
        SignalDisposition::CancelCooperatively
    } else {
        SignalDisposition::ExitImmediately
    }
}

/// Guard marking the narrow interval in which a first termination signal
/// requests cooperative cleanup instead of immediately exiting the process.
#[derive(Debug)]
pub struct CancellationSignalGuard;

/// Enter a process-wide cooperative cancellation interval.
///
/// Cooperative scopes must not overlap. Dropping the returned guard restores
/// immediate signal termination behavior.
#[must_use]
pub fn cooperative_cancellation_scope() -> CancellationSignalGuard {
    let already_active = COOPERATIVE_CANCELLATION_ACTIVE.swap(true, Ordering::SeqCst);
    debug_assert!(
        !already_active,
        "cooperative cancellation scopes must not overlap"
    );
    CancellationSignalGuard
}

impl Drop for CancellationSignalGuard {
    fn drop(&mut self) {
        COOPERATIVE_CANCELLATION_ACTIVE.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_signal_state_contract_is_stable_and_scoped() {
        let first = process_cancellation_token();
        let second = process_cancellation_token();
        assert!(first.shares_state_with(&second));
        assert_eq!(install_signal_handlers(), Ok(()));
        assert_eq!(install_signal_handlers(), Ok(()));

        let cancellation = CancellationToken::new();
        assert_eq!(
            signal_disposition(false, &cancellation),
            SignalDisposition::ExitImmediately
        );
        assert_eq!(
            signal_disposition(true, &cancellation),
            SignalDisposition::CancelCooperatively
        );
        cancellation.cancel();
        assert_eq!(
            signal_disposition(true, &cancellation),
            SignalDisposition::ExitImmediately
        );

        assert!(!COOPERATIVE_CANCELLATION_ACTIVE.load(Ordering::SeqCst));
        {
            let guard = cooperative_cancellation_scope();
            assert!(COOPERATIVE_CANCELLATION_ACTIVE.load(Ordering::SeqCst));
            drop(guard);
        }
        assert!(!COOPERATIVE_CANCELLATION_ACTIVE.load(Ordering::SeqCst));
    }
}

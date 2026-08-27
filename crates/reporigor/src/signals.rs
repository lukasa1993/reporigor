use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use analysis_mutate::CancellationToken;
use anyhow::{anyhow, Result};

static PROCESS_CANCELLATION: OnceLock<CancellationToken> = OnceLock::new();
static HANDLER_INSTALLATION: OnceLock<Result<(), String>> = OnceLock::new();
static COOPERATIVE_CANCELLATION_ACTIVE: AtomicBool = AtomicBool::new(false);

const FORCED_CANCELLATION_EXIT: i32 = 130;

pub(crate) fn process_cancellation_token() -> CancellationToken {
    PROCESS_CANCELLATION.get_or_init(CancellationToken::new).clone()
}

/// Install the binary's process-wide cancellation handler once.
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
pub fn install_signal_handlers() -> Result<()> {
    let result = HANDLER_INSTALLATION.get_or_init(|| {
        let cancellation = process_cancellation_token();
        ctrlc::set_handler(move || {
            if !COOPERATIVE_CANCELLATION_ACTIVE.load(Ordering::SeqCst) || cancellation.is_cancelled() {
                std::process::exit(FORCED_CANCELLATION_EXIT);
            }
            cancellation.cancel();
        })
        .map_err(|error| error.to_string())
    });
    result.clone().map_err(|message| anyhow!(message))
}

#[derive(Debug)]
pub(crate) struct CancellationSignalGuard;

pub(crate) fn cooperative_cancellation_scope() -> CancellationSignalGuard {
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

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::MutationError;

/// A cooperatively shared cancellation request for mutation execution.
///
/// This type is deliberately independent of operating-system signal handling.
/// Library callers may cancel a run directly, while the `reporigor` binary
/// connects its process-wide signal handler to one shared token.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Request cancellation. Repeated requests are harmless.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn shares_state_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cancelled, &other.cancelled)
    }

    pub(crate) fn check(&self) -> Result<(), MutationError> {
        if self.is_cancelled() {
            Err(MutationError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[test]
fn clones_share_the_cancellation_request() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert!(!clone.is_cancelled());
    token.cancel();
    assert!(clone.is_cancelled());
    assert!(token.shares_state_with(&clone));
    assert!(matches!(clone.check(), Err(MutationError::Cancelled)));
}

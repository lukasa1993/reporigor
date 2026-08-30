//! Safe, language-neutral mutation execution.
//!
//! Language adapters discover [`reporigor_core::MutationCandidate`] values;
//! this crate owns the dangerous lifecycle around applying them. Every run is
//! serialized through an external persistent-state lock, journals an active
//! source replacement before it happens, restores through a conflict-checked
//! atomic same-directory rename, and kills a timed-out or cooperatively
//! cancelled command's process tree before restoration. Execute callers can
//! acquire and recover before source analysis; list mode remains state-free.
//! The executor never follows symbolic links, recreates missing targets,
//! writes control paths, or accepts paths that can escape the project root.

mod cancellation;
mod command;
mod error;
mod executor;
mod filesystem;
mod model;
mod selection;
mod signals;
#[cfg(test)]
mod test_support;

pub use cancellation::CancellationToken;
pub use command::{run_command, MUTANT_FINGERPRINT_ENV, MUTANT_ID_ENV};
pub use error::MutationError;
pub use executor::{recover_active, MutationExecutionSession, MutationExecutor, MutationReadSession};
pub use filesystem::{
    mutation_state_directory, pending_mutation_journal, PendingMutation, ACTIVE_JOURNAL, ACTIVE_RUN,
    MAX_MUTATION_SOURCE_BYTES, RUN_LOCK, STATE_DIRECTORY, STATE_DIRECTORY_ENV,
};
pub use model::{
    is_scoreable_status, mutation_score, BaselinePhase, BaselineReport, CommandOutcome, CommandSpec,
    MutationMode, MutationOptions, MutationRun, MutationSummary, RecoveryAction,
};
pub use selection::{select_candidates, MutationSelectionError};
pub use signals::{
    cooperative_cancellation_scope, install_signal_handlers, process_cancellation_token,
    CancellationSignalGuard, SignalHandlerError,
};

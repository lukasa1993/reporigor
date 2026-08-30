use std::path::PathBuf;

use crate::{BaselinePhase, CommandOutcome};

#[derive(Clone, Copy)]
pub(crate) enum PathMessageKind {
    Unsafe,
    InvalidJournal,
    UnsupportedMetadata,
}

#[derive(Debug, thiserror::Error)]
pub enum MutationError {
    #[error("mutation run cancelled")]
    Cancelled,

    #[error("invalid project root {}: {message}", path.display())]
    InvalidRoot { path: PathBuf, message: String },

    #[error("another mutation run already holds {}", path.display())]
    AlreadyRunning { path: PathBuf },

    #[error("invalid mutation option: {0}")]
    InvalidOptions(String),

    #[error("unsafe mutation path {rendered}: {message}", rendered = path.display())]
    UnsafePath {
        path: std::path::PathBuf,
        message: String,
    },

    #[error(
        "mutation recovery for {} is still pending while {} was requested; recover the original root first",
        active_root.display(),
        requested_root.display()
    )]
    PendingMutationRoot {
        active_root: PathBuf,
        requested_root: PathBuf,
    },

    #[error("invalid mutation journal {}: {message}", path.display())]
    InvalidJournal {
        path: PathBuf,
        message: std::string::String,
    },

    #[error(
        "mutation source {} is at least {actual_bytes} bytes, exceeding the executable mutation limit of {max_source_bytes} bytes",
        path.display()
    )]
    SourceTooLarge {
        path: PathBuf,
        actual_bytes: u64,
        max_source_bytes: u64,
    },

    #[error(
        "refusing to overwrite independently changed mutation source {}; the source was left unchanged and its recovery journal remains at {}. Preserve or revert the independent edits before retrying recovery",
        path.display(),
        journal.display()
    )]
    RecoveryConflict {
        path: PathBuf,
        journal: std::path::PathBuf,
    },

    #[error("cannot safely replace mutation source {}: {message}", path.display())]
    UnsupportedSourceMetadata {
        path: std::path::PathBuf,
        message: std::string::String,
    },

    #[error(
        "cannot recover {} because global pointer {} exists but journal {} is missing: {message}",
        path.display(),
        pointer.display(),
        journal.display()
    )]
    MissingRecoveryJournal {
        path: PathBuf,
        pointer: PathBuf,
        journal: PathBuf,
        message: String,
    },

    #[error("baseline {phase} failed: {reason}")]
    BaselineFailed {
        phase: BaselinePhase,
        reason: String,
        outcome: Box<CommandOutcome>,
    },

    #[error("{operation} failed for {}: {source}", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("cannot encode or decode mutation state: {0}")]
    State(String),

    #[error("mutation command failed: {0}")]
    Command(String),

    #[error("mutation process-tree supervision failed while {operation}: {message}")]
    ProcessTree {
        operation: &'static str,
        message: String,
    },
}

impl MutationError {
    pub(crate) fn io(
        operation: &'static str,
        path: impl Into<PathBuf>,
        source: impl Into<std::io::Error>,
    ) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source: source.into(),
        }
    }

    pub(crate) fn path_message(
        kind: PathMessageKind,
        path: impl Into<PathBuf>,
        message: impl Into<String>,
    ) -> Self {
        let path = path.into();
        let message = message.into();
        match kind {
            PathMessageKind::Unsafe => Self::UnsafePath { path, message },
            PathMessageKind::InvalidJournal => Self::InvalidJournal { path, message },
            PathMessageKind::UnsupportedMetadata => Self::UnsupportedSourceMetadata { path, message },
        }
    }

    pub(crate) fn recovery_conflict(path: impl Into<PathBuf>, journal: impl Into<PathBuf>) -> Self {
        Self::RecoveryConflict {
            path: path.into(),
            journal: journal.into(),
        }
    }
}

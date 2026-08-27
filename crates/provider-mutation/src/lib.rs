//! Safe boundaries for optional ecosystem mutation-testing providers.
//!
//! Static discovery only inspects project files and executable search paths.
//! It never starts a process, installs a package, or mutates the checkout.
//! Read-only version probes are a separate, explicit operation. External
//! mutation execution is deliberately not exposed until a provider can meet
//! reporigor's checkout-restoration contract; callers can still import an
//! existing Mutation Testing Elements v2 report.

mod command;
mod import;
mod inventory;
mod model;

pub use command::{CommandOutput, CommandRunner, SystemCommandRunner};
pub use import::{import_json, import_path};
pub use inventory::{
    discover, discover_with_options, preflight, preflight_with_options, preflight_with_runner,
};
pub use model::{
    BoundedCommand, CommandEffect, DetectionSource, ImportFormat, ImportedMutation, ImportedMutationReport,
    MutationProvider, MutationProviderOptions, MutationProviderStatus, ProviderInventory,
};

/// Errors returned by mutation-provider discovery, probing, and report import.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("project root is not a directory: {0}")]
    InvalidRoot(std::path::PathBuf),
    #[error("failed to inspect {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("provider command is not a read-only probe: {0}")]
    EffectfulCommand(String),
    #[error("failed to start {program}: {source}")]
    CommandStart {
        program: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("provider command {program} timed out after {seconds:.3} seconds")]
    CommandTimeout {
        program: std::path::PathBuf,
        seconds: f64,
    },
    #[error("failed to collect output from {program}: {message}")]
    CommandOutput {
        program: std::path::PathBuf,
        message: String,
    },
    #[error("provider probe failed for {provider}: {message}")]
    ProbeFailed {
        provider: MutationProvider,
        message: String,
    },
    #[error("failed to parse mutation report: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported report for {provider}: {message}")]
    UnsupportedReport {
        provider: MutationProvider,
        message: String,
    },
    #[error("invalid mutation report at {field}: {message}")]
    InvalidReport { field: String, message: String },
}

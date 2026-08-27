//! Stable contracts shared by the reporigor CLI, analyzers, and language adapters.

mod bounded_file;
mod config;
mod discovery;
mod dry_budget;
mod duration;
mod model;
mod source_budget;

pub use bounded_file::{
    read_bounded_utf8_file, read_bounded_utf8_file_within, read_optional_bounded_utf8_file_within,
    resolve_optional_regular_file_within, PROJECT_METADATA_MAX_BYTES,
};
pub use config::{CrapConfig, DryConfig, MutationConfig, RepoRigorConfig};
pub use discovery::{discover_project, discover_sources, DiscoveryOptions};
pub use dry_budget::{
    validate_dry_work_limit, DRY_DEFAULT_MAX_CANDIDATE_WORK, DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS,
    DRY_DEFAULT_MAX_TOTAL_WINDOWS, DRY_HARD_MAX_CANDIDATE_WORK, DRY_HARD_MAX_FINGERPRINT_BUCKETS,
    DRY_HARD_MAX_TOTAL_WINDOWS,
};
pub use duration::{checked_duration_from_secs_f64, InvalidDurationSeconds};
pub use model::{
    AnalysisRequest, AnalysisSnapshot, BackendCapabilities, BackendInfo, BackendPreference, Capability,
    Diagnostic, FileAnalysis, FunctionRecord, Language, MutationCandidate, MutationResult, MutationStatus,
    ProjectContext, ProjectKind, Severity, SourceFile, SourceLocation, TokenRecord,
};
pub use source_budget::{
    validate_max_source_bytes, SourceBudget, MAX_SELECTED_SOURCE_BYTES, MAX_SELECTED_SOURCE_FILES,
    MAX_SOURCE_BYTES_HARD_LIMIT,
};

use std::path::Path;

/// Contract implemented by syntax adapters. Project-aware providers may choose
/// the source set and then delegate individual files to one of these adapters.
pub trait SyntaxBackend: Send + Sync {
    fn info(&self) -> BackendInfo;

    fn supports(&self, language: Language) -> bool;

    /// Analyze one source file into the shared interchange model.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or analyzed under the
    /// requested parse policy.
    fn analyze_file(
        &self,
        root: &Path,
        source: &SourceFile,
        request: &AnalysisRequest,
    ) -> Result<FileAnalysis, CoreError>;
}

/// Contract implemented by ecosystem adapters such as Cargo, Clang, TypeScript,
/// and `SwiftPM`. Providers never silently hide degraded behavior: diagnostics and
/// the actual backend identity are included in the returned project context.
pub trait ProjectBackend: Send + Sync {
    fn info(&self) -> BackendInfo;

    fn supports(&self, project: ProjectKind) -> bool;

    /// Resolve project metadata and the authoritative source set.
    ///
    /// # Errors
    ///
    /// Returns an error when required project metadata is invalid or its
    /// provider cannot execute safely.
    fn resolve(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("invalid project root {path}: {message}")]
    InvalidRoot { path: String, message: String },
    #[error("failed to read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "source {path} is at least {actual_bytes} bytes, exceeding max_source_bytes ({max_source_bytes} bytes)"
    )]
    SourceTooLarge {
        path: String,
        actual_bytes: u64,
        max_source_bytes: u64,
    },
    #[error(
        "selected source budget exceeded at {path}: {selected_files} files and {selected_bytes} bytes (immutable limits: {max_files} files and {max_bytes} bytes)"
    )]
    SourceBudgetExceeded {
        path: String,
        selected_files: u64,
        max_files: u64,
        selected_bytes: u64,
        max_bytes: u64,
    },
    #[error("source {path} is not valid UTF-8 (first invalid byte at offset {valid_up_to})")]
    InvalidSourceEncoding { path: String, valid_up_to: usize },
    #[error("failed to parse {path}: {message}")]
    Parse { path: String, message: String },
    #[error("refusing to read {path}: file is at least {size} bytes, maximum is {max_bytes} bytes")]
    FileTooLarge { path: String, size: u64, max_bytes: u64 },
    #[error("backend {backend} is unavailable: {message}")]
    BackendUnavailable { backend: String, message: String },
    #[error("backend {backend} failed: {message}")]
    Backend { backend: String, message: String },
    #[error("unsafe path {path}: {message}")]
    UnsafePath { path: String, message: String },
    #[error("command failed: {0}")]
    Command(String),
}

impl CoreError {
    /// Construct the shared fail-closed error for a selected source that is
    /// larger than the request's configured byte limit.
    #[must_use]
    pub fn source_too_large(path: &Path, actual_bytes: u64, max_source_bytes: usize) -> Self {
        Self::SourceTooLarge {
            path: path.display().to_string(),
            actual_bytes,
            max_source_bytes: u64::try_from(max_source_bytes).unwrap_or(u64::MAX),
        }
    }
}

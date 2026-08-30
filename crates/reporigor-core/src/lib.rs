//! Stable contracts shared by the reporigor CLI, analyzers, and language adapters.

mod bounded_file;
mod config;
mod discovery;
mod dry_budget;
mod duration;
mod model;
mod path_io;
mod source_budget;
mod stable_id;

pub use bounded_file::{
    canonical_directory, read_bounded_utf8_file, read_bounded_utf8_file_within,
    read_optional_bounded_utf8_file_within, resolve_optional_regular_file_within, PROJECT_METADATA_MAX_BYTES,
};
pub use config::{
    ArchitectureConfig, BaselineConfig, CohesionConfig, CrapConfig, DryConfig, KissConfig, MutationConfig,
    MutationOperator, RepoRigorConfig, YagniConfig,
};
pub use discovery::{discover_project, discover_sources, DiscoveryOptions};
pub use dry_budget::{
    validate_dry_work_limit, DRY_DEFAULT_MAX_CANDIDATE_WORK, DRY_DEFAULT_MAX_FINGERPRINT_BUCKETS,
    DRY_DEFAULT_MAX_TOTAL_WINDOWS, DRY_HARD_MAX_CANDIDATE_WORK, DRY_HARD_MAX_FINGERPRINT_BUCKETS,
    DRY_HARD_MAX_TOTAL_WINDOWS,
};
pub use duration::{checked_duration_from_secs_f64, InvalidDurationSeconds};
pub use model::{
    canonicalize_rule_results, compare_function_records, validate_rule_results, AnalysisRequest,
    AnalysisSnapshot, BackendCapabilities, BackendInfo, BackendPreference, BaselineDisposition, Capability,
    CoverageSpan, DependencyRecord, DependencyScope, Diagnostic, FeatureRecord, FileAnalysis, FunctionRecord,
    IdentifierCountRecord, Language, ModuleRecord, MutationCandidate, MutationResult, MutationStatus,
    PackageRecord, ProjectContext, ProjectKind, RepositorySemantics, RuleComparison, RuleOutcome, RuleResult,
    RuleResultInput, RuleSummary, Severity, SourceFile, SourceLocation, SymbolVisibility, TestRecord,
    TokenRecord, TraitImplementationRecord, UnreachableRecord,
};
pub use path_io::is_executable_file;
pub use source_budget::{
    validate_max_source_bytes, SourceBudget, MAX_SELECTED_SOURCE_BYTES, MAX_SELECTED_SOURCE_FILES,
    MAX_SOURCE_BYTES_HARD_LIMIT,
};
pub use stable_id::{is_lowercase_sha256, normalize_repository_path, stable_id};

/// Convert a bounded in-memory count without truncating values representable by
/// the analysis model.
#[must_use]
pub fn count_as_f64(value: usize) -> f64 {
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

/// Construct one [`RuleResult`] without a high-arity function signature.
#[macro_export]
macro_rules! rule_result {
    (
        $rule_id:expr,
        $file:expr,
        $stable_symbol:expr,
        $measured:expr,
        $allowed:expr,
        $algorithm:expr,
        $comparison:expr,
        $structural_evidence:expr $(,)?
    ) => {
        $crate::RuleResult::new($crate::RuleResultInput {
            rule_id: ($rule_id).into(),
            file: ($file).into(),
            stable_symbol: ($stable_symbol).into(),
            measured: $measured,
            allowed: $allowed,
            algorithm: ($algorithm).into(),
            comparison: $comparison,
            structural_evidence: ($structural_evidence).into(),
        })
    };
}

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

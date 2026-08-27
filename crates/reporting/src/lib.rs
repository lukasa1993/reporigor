//! Stable output formats for the unified reporigor analyzers.
//!
//! The native JSON report is the lossless format. SARIF and Mutation Testing
//! Elements are projections intended for CI systems and ecosystem tooling.

mod human;
mod model;
mod mutation_elements;
mod sarif;

pub use analysis_crap::CoverageApplication;
pub use analysis_dry::{Duplicate, Location as DuplicateLocation};
pub use human::{escape_terminal_text, render_human};
pub use model::{
    CrapReport, CrapSummary, DryReport, DrySummary, MutationReport, MutationRunProvenance, MutationSummary,
    ReportCommand, ReportContext, ReportData, ReportEnvelope, ReportSummary, ToolInfo,
};
pub use mutation_elements::{mutation_elements_json, mutation_elements_value, MutationThresholds};
pub use sarif::{sarif_json, sarif_value};

use serde::Serialize;

/// Native report schema version. A breaking semantic change requires a new
/// number even if the Rust API remains source-compatible.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Mutation Testing Elements report schema emitted by this crate.
pub const MUTATION_ELEMENTS_SCHEMA_VERSION: &str = "2.0";

/// SARIF version emitted by this crate.
pub const SARIF_VERSION: &str = "2.1.0";

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("failed to serialize report: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the report does not contain {0} results")]
    MissingSection(&'static str),
    #[error("source text is required for mutated file {0}")]
    MissingMutationSource(String),
    #[error("mutation {id} in {file} has an invalid source span: {message}")]
    InvalidMutationSpan { id: u64, file: String, message: String },
    #[error("mutation thresholds must satisfy 0 <= low <= high <= 100 (low={low}, high={high})")]
    InvalidMutationThresholds { low: u8, high: u8 },
}

/// Serialize a report projection as stable, newline-terminated pretty JSON.
///
/// All maps in this crate are ordered and report constructors sort record
/// arrays, so repeated serialization of equivalent inputs is byte-for-byte
/// deterministic.
///
/// # Errors
///
/// Returns [`ReportError::Json`] if `value` cannot be represented as JSON.
pub fn pretty_json<T: Serialize>(value: &T) -> Result<String, ReportError> {
    let mut rendered = serde_json::to_string_pretty(value)?;
    rendered.push('\n');
    Ok(rendered)
}

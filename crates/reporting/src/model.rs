use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use analysis_crap::CoverageApplication;
use analysis_dry::{Duplicate, Location as DuplicateLocation};
use analysis_mutate::{BaselineReport, MutationMode, MutationRun, RecoveryAction};
use reporigor_core::{
    AnalysisSnapshot, BackendInfo, Diagnostic, FunctionRecord, MutationCandidate, MutationResult,
    MutationStatus, Severity,
};
use serde::{Deserialize, Serialize};

use crate::{pretty_json, MutationThresholds, ReportError, REPORT_SCHEMA_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReportCommand {
    Crap,
    Dry,
    Mutate,
    Check,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub version: String,
}

impl Default for ToolInfo {
    fn default() -> Self {
        Self {
            name: "reporigor".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// Non-result metadata used to construct any native report.
#[derive(Debug, Clone)]
pub struct ReportContext {
    pub root: PathBuf,
    pub files: usize,
    pub parse_errors: usize,
    pub backends: Vec<BackendInfo>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ReportContext {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            files: 0,
            parse_errors: 0,
            backends: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    #[must_use]
    pub fn from_snapshot(root: impl Into<PathBuf>, snapshot: &AnalysisSnapshot) -> Self {
        Self {
            root: root.into(),
            files: snapshot.files.len(),
            parse_errors: snapshot.parse_errors,
            backends: snapshot.backends.clone(),
            diagnostics: snapshot.diagnostics.clone(),
        }
    }

    fn normalize(&mut self) {
        self.backends.sort_by(|left, right| {
            (
                &left.id,
                &left.version,
                left.native,
                &left.capabilities.capabilities,
            )
                .cmp(&(
                    &right.id,
                    &right.version,
                    right.native,
                    &right.capabilities.capabilities,
                ))
        });
        self.diagnostics.sort_by(compare_diagnostics);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrapSummary {
    pub functions: usize,
    pub missing_coverage: usize,
    pub over_limit: usize,
    pub limit: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrapReport {
    pub summary: CrapSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CoverageApplication>,
    pub functions: Vec<FunctionRecord>,
}

impl CrapReport {
    #[must_use]
    pub fn new(mut functions: Vec<FunctionRecord>, limit: f64) -> Self {
        analysis_crap::sort_by_risk(&mut functions);
        let summary = CrapSummary {
            functions: functions.len(),
            missing_coverage: functions
                .iter()
                .filter(|function| function.coverage.is_none())
                .count(),
            over_limit: functions
                .iter()
                .filter(|function| function.crap.is_some_and(|score| score > limit))
                .count(),
            limit,
        };
        Self {
            summary,
            coverage: None,
            functions,
        }
    }

    /// Build a report section from the CRAP analyzer's native result.
    #[must_use]
    pub fn from_analysis(analysis: analysis_crap::CrapAnalysis, limit: f64) -> Self {
        let analysis_crap::CrapAnalysis { functions, coverage } = analysis;
        let mut report = Self::new(functions, limit);
        report.coverage = coverage;
        report
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrySummary {
    pub groups: usize,
    pub min_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DryReport {
    pub summary: DrySummary,
    pub duplicates: Vec<Duplicate>,
}

impl DryReport {
    #[must_use]
    pub fn new(mut duplicates: Vec<Duplicate>, min_tokens: usize) -> Self {
        for duplicate in &mut duplicates {
            duplicate.locations.sort_by(compare_duplicate_locations);
        }
        duplicates.sort_by(compare_duplicates);
        Self {
            summary: DrySummary {
                groups: duplicates.len(),
                min_tokens,
            },
            duplicates,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MutationSummary {
    pub total: usize,
    pub killed: usize,
    pub survived: usize,
    pub no_coverage: usize,
    pub compile_error: usize,
    pub runtime_error: usize,
    pub timeout: usize,
    pub invalid: usize,
    pub ignored: usize,
    pub pending: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutation_score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationRunProvenance {
    pub mode: MutationMode,
    pub recovery: RecoveryAction,
    pub baseline: BaselineReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationReport {
    pub summary: MutationSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<MutationRunProvenance>,
    pub mutants: Vec<MutationResult>,
}

impl MutationReport {
    #[must_use]
    pub fn new(mut mutants: Vec<MutationResult>) -> Self {
        mutants.sort_by(compare_mutations);
        let mut summary = MutationSummary {
            total: mutants.len(),
            ..MutationSummary::default()
        };
        for mutant in &mutants {
            match mutant.status {
                MutationStatus::Killed => summary.killed += 1,
                MutationStatus::Survived => summary.survived += 1,
                MutationStatus::NoCoverage => summary.no_coverage += 1,
                MutationStatus::CompileError => summary.compile_error += 1,
                MutationStatus::RuntimeError => summary.runtime_error += 1,
                MutationStatus::Timeout => summary.timeout += 1,
                MutationStatus::Invalid => summary.invalid += 1,
                MutationStatus::Ignored => summary.ignored += 1,
                MutationStatus::Pending => summary.pending += 1,
            }
        }
        // Mutation Testing Elements treats killed and timed-out mutants as
        // detected, and survived/no-coverage mutants as undetected. Compile
        // and runtime errors are invalid and therefore excluded from score.
        let detected = summary.killed + summary.timeout;
        let valid = detected + summary.survived + summary.no_coverage;
        if valid > 0 {
            summary.mutation_score = Some((count_as_f64(detected) / count_as_f64(valid)) * 100.0);
        }
        Self {
            summary,
            run: None,
            mutants,
        }
    }

    /// Build a report section from the mutation executor's native run result.
    #[must_use]
    pub fn from_run(run: MutationRun) -> Self {
        let MutationRun {
            mode,
            recovery,
            baseline,
            results,
            ..
        } = run;
        let mut report = Self::new(results);
        report.run = Some(MutationRunProvenance {
            mode,
            recovery,
            baseline,
        });
        report
    }

    /// Build a list-mode report without executing the candidate mutations.
    #[must_use]
    pub fn pending(candidates: Vec<MutationCandidate>) -> Self {
        Self::new(
            candidates
                .into_iter()
                .map(|mutation| MutationResult {
                    mutation,
                    status: MutationStatus::Pending,
                    exit_code: None,
                    duration_seconds: 0.0,
                    detail: None,
                })
                .collect(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReportData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub crap: Option<CrapReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry: Option<DryReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutate: Option<MutationReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReportSummary {
    pub files: usize,
    pub functions: usize,
    pub crap_over_limit: usize,
    pub duplicate_groups: usize,
    pub mutants: usize,
    pub killed: usize,
    pub survived: usize,
    pub no_coverage: usize,
    pub mutation_errors: usize,
    pub findings: usize,
    pub parse_errors: usize,
    pub diagnostics: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportEnvelope {
    pub schema_version: u32,
    pub tool: ToolInfo,
    pub command: ReportCommand,
    pub root: PathBuf,
    pub summary: ReportSummary,
    pub backends: Vec<BackendInfo>,
    pub diagnostics: Vec<Diagnostic>,
    pub results: ReportData,
}

impl ReportEnvelope {
    #[must_use]
    pub fn crap(context: ReportContext, report: CrapReport) -> Self {
        Self::from_parts(
            ReportCommand::Crap,
            context,
            ReportData {
                crap: Some(report),
                ..ReportData::default()
            },
        )
    }

    #[must_use]
    pub fn dry(context: ReportContext, report: DryReport) -> Self {
        Self::from_parts(
            ReportCommand::Dry,
            context,
            ReportData {
                dry: Some(report),
                ..ReportData::default()
            },
        )
    }

    #[must_use]
    pub fn mutate(context: ReportContext, report: MutationReport) -> Self {
        Self::from_parts(
            ReportCommand::Mutate,
            context,
            ReportData {
                mutate: Some(report),
                ..ReportData::default()
            },
        )
    }

    #[must_use]
    pub fn check(
        context: ReportContext,
        crap: Option<CrapReport>,
        dry: Option<DryReport>,
        mutate: Option<MutationReport>,
    ) -> Self {
        Self::from_parts(ReportCommand::Check, context, ReportData { crap, dry, mutate })
    }

    #[must_use]
    fn from_parts(command: ReportCommand, mut context: ReportContext, results: ReportData) -> Self {
        context.normalize();
        let summary = summarize(&context, &results);
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            tool: ToolInfo::default(),
            command,
            root: context.root,
            summary,
            backends: context.backends,
            diagnostics: context.diagnostics,
            results,
        }
    }

    /// Serialize the native schema-v1 report.
    ///
    /// # Errors
    ///
    /// Returns [`ReportError::Json`] if a value cannot be represented as JSON.
    pub fn to_pretty_json(&self) -> Result<String, ReportError> {
        pretty_json(self)
    }

    #[must_use]
    pub fn to_human(&self) -> String {
        crate::render_human(self)
    }

    /// Project CRAP and DRY findings into SARIF 2.1.0.
    ///
    /// # Errors
    ///
    /// Returns an error when neither supported section is present or when the
    /// projection cannot be serialized.
    pub fn to_sarif_json(&self) -> Result<String, ReportError> {
        crate::sarif_json(self)
    }

    /// Project mutation results into Mutation Testing Elements v2 JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when mutation results or required source text are
    /// missing, thresholds are invalid, or serialization fails.
    pub fn to_mutation_elements_json(
        &self,
        sources: &BTreeMap<String, String>,
        thresholds: MutationThresholds,
    ) -> Result<String, ReportError> {
        crate::mutation_elements_json(self, sources, thresholds)
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn summarize(context: &ReportContext, data: &ReportData) -> ReportSummary {
    let mut summary = ReportSummary {
        files: context.files,
        parse_errors: context.parse_errors,
        diagnostics: context.diagnostics.len(),
        ..ReportSummary::default()
    };
    if let Some(crap) = &data.crap {
        summary.functions = crap.summary.functions;
        summary.crap_over_limit = crap.summary.over_limit;
    }
    if let Some(dry) = &data.dry {
        summary.duplicate_groups = dry.summary.groups;
    }
    if let Some(mutation) = &data.mutate {
        summary.mutants = mutation.summary.total;
        summary.killed = mutation.summary.killed;
        summary.survived = mutation.summary.survived;
        summary.no_coverage = mutation.summary.no_coverage;
        summary.mutation_errors = mutation.summary.compile_error
            + mutation.summary.runtime_error
            + mutation.summary.timeout
            + mutation.summary.invalid;
    }
    summary.findings =
        summary.crap_over_limit + summary.duplicate_groups + summary.survived + summary.no_coverage;
    summary
}

fn compare_duplicates(left: &Duplicate, right: &Duplicate) -> Ordering {
    right
        .token_count
        .cmp(&left.token_count)
        .then_with(|| {
            left.locations
                .iter()
                .zip(&right.locations)
                .map(|(left_location, right_location)| {
                    compare_duplicate_locations(left_location, right_location)
                })
                .find(|ordering| *ordering != Ordering::Equal)
                .unwrap_or(Ordering::Equal)
        })
        .then_with(|| left.locations.len().cmp(&right.locations.len()))
}

fn compare_duplicate_locations(left: &DuplicateLocation, right: &DuplicateLocation) -> Ordering {
    (
        &left.file,
        left.start_line,
        left.end_line,
        left.start_token,
        left.end_token,
    )
        .cmp(&(
            &right.file,
            right.start_line,
            right.end_line,
            right.start_token,
            right.end_token,
        ))
}

fn compare_mutations(left: &MutationResult, right: &MutationResult) -> Ordering {
    (
        &left.mutation.file,
        left.mutation.line,
        left.mutation.column,
        left.mutation.id,
        &left.mutation.original,
        &left.mutation.replacement,
        mutation_status_rank(left.status),
        left.exit_code,
        &left.detail,
    )
        .cmp(&(
            &right.mutation.file,
            right.mutation.line,
            right.mutation.column,
            right.mutation.id,
            &right.mutation.original,
            &right.mutation.replacement,
            mutation_status_rank(right.status),
            right.exit_code,
            &right.detail,
        ))
        .then_with(|| left.duration_seconds.total_cmp(&right.duration_seconds))
}

fn compare_diagnostics(left: &Diagnostic, right: &Diagnostic) -> Ordering {
    (
        severity_rank(left.severity),
        left.location.as_ref().map(|location| location.file.as_str()),
        left.location.as_ref().map(|location| location.start_line),
        left.location.as_ref().map(|location| location.start_column),
        left.location.as_ref().map(|location| location.end_line),
        left.location.as_ref().map(|location| location.end_column),
        left.backend.as_str(),
        left.message.as_str(),
        left.fallback_used,
    )
        .cmp(&(
            severity_rank(right.severity),
            right.location.as_ref().map(|location| location.file.as_str()),
            right.location.as_ref().map(|location| location.start_line),
            right.location.as_ref().map(|location| location.start_column),
            right.location.as_ref().map(|location| location.end_line),
            right.location.as_ref().map(|location| location.end_column),
            right.backend.as_str(),
            right.message.as_str(),
            right.fallback_used,
        ))
}

fn count_as_f64(value: usize) -> f64 {
    // A report cannot hold more than u32::MAX records in practical memory.
    // Saturating here avoids a precision-losing architecture-sized cast.
    f64::from(u32::try_from(value).unwrap_or(u32::MAX))
}

const fn mutation_status_rank(status: MutationStatus) -> u8 {
    match status {
        MutationStatus::Killed => 0,
        MutationStatus::Survived => 1,
        MutationStatus::NoCoverage => 2,
        MutationStatus::CompileError => 3,
        MutationStatus::RuntimeError => 4,
        MutationStatus::Timeout => 5,
        MutationStatus::Invalid => 6,
        MutationStatus::Ignored => 7,
        MutationStatus::Pending => 8,
    }
}

const fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Error => 0,
        Severity::Warning => 1,
        Severity::Info => 2,
    }
}

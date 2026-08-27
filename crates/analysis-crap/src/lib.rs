//! Shared CRAP metric calculation and multi-format line-coverage ingestion.
//!
//! Language adapters provide [`FunctionRecord`] values. This crate deliberately
//! owns no parser: it maps normalized executable-line coverage to those
//! functions and applies the language-independent CRAP formula.

mod coverage;

use std::collections::BTreeMap;
use std::path::Path;

use reporigor_core::FunctionRecord;
use serde::{Deserialize, Serialize};

pub use coverage::{
    discover_coverage_report, load_coverage, load_coverage_as, normalize_path, parse_cobertura,
    parse_coverage, parse_coverage_py_json, parse_istanbul_json, parse_lcov, parse_llvm_json, CoverageError,
    CoverageFormat, CoverageReport, LineCoverage, MAX_COBERTURA_CLASSES, MAX_COBERTURA_CLASS_LINES,
    MAX_COBERTURA_RESOLUTION_CANDIDATES, MAX_COBERTURA_SOURCES, MAX_COBERTURA_XML_ATTRIBUTES,
    MAX_COBERTURA_XML_DEPTH, MAX_COBERTURA_XML_ENTITY_BYTES, MAX_COBERTURA_XML_ENTITY_DEPTH,
    MAX_COBERTURA_XML_MARKUP_BYTES, MAX_COBERTURA_XML_NAMESPACE_DECLARATIONS, MAX_COBERTURA_XML_NAME_BYTES,
    MAX_COBERTURA_XML_VALUE_BYTES, MAX_COVERAGE_DISCOVERY_BYTES, MAX_COVERAGE_DISCOVERY_CANDIDATES,
    MAX_COVERAGE_DISCOVERY_DIRECTORIES, MAX_COVERAGE_DISCOVERY_ENTRIES, MAX_COVERAGE_EXECUTABLE_LINES,
    MAX_COVERAGE_FILES, MAX_COVERAGE_LINES_PER_FILE, MAX_COVERAGE_PATH_BYTES, MAX_COVERAGE_RECORDS,
    MAX_COVERAGE_REPORT_BYTES, MAX_LLVM_EXPANDED_LINES, MAX_LLVM_REGION_LINES,
};

/// Result of mapping one coverage report onto a function collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CoverageApplication {
    pub total_functions: usize,
    pub matched_functions: usize,
    /// Functions whose source file cannot be resolved without ambiguity.
    pub unmatched_functions: usize,
    /// Functions whose source file matched but whose range has no executable
    /// lines in the report.
    pub empty_ranges: usize,
}

impl CoverageApplication {
    #[must_use]
    pub const fn missing_functions(self) -> usize {
        self.unmatched_functions + self.empty_ranges
    }
}

/// CLI-ready CRAP output. Functions are sorted by descending CRAP risk, with
/// missing coverage last and deterministic source ordering as a tie-breaker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrapAnalysis {
    pub functions: Vec<FunctionRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<CoverageApplication>,
}

impl CrapAnalysis {
    #[must_use]
    pub fn missing_coverage(&self) -> usize {
        self.functions
            .iter()
            .filter(|function| function.coverage.is_none())
            .count()
    }

    #[must_use]
    pub fn over_threshold(&self, threshold: f64) -> usize {
        self.functions
            .iter()
            .filter(|function| function.crap.is_some_and(|score| score > threshold))
            .count()
    }

    #[must_use]
    pub fn max_score(&self) -> Option<f64> {
        self.functions
            .iter()
            .filter_map(|function| function.crap)
            .max_by(f64::total_cmp)
    }
}

/// The exact CRAP metric: `C² × (1 - coverage/100)³ + C`.
#[must_use]
pub fn crap_score(complexity: u32, coverage_percent: f64) -> f64 {
    let complexity = f64::from(complexity);
    let uncovered = 1.0 - coverage_percent / 100.0;
    complexity * complexity * uncovered.powi(3) + complexity
}

/// Compatibility spelling for callers migrating from the individual tools.
#[must_use]
pub fn score(complexity: u32, coverage_percent: f64) -> f64 {
    crap_score(complexity, coverage_percent)
}

/// Apply executable-line coverage to inclusive function ranges in place.
/// Existing coverage/CRAP values are cleared first so repeated application
/// cannot accidentally retain data from an older report.
pub fn apply_coverage(
    root: &Path,
    functions: &mut [FunctionRecord],
    coverage: &CoverageReport,
) -> CoverageApplication {
    let mut application = CoverageApplication {
        total_functions: functions.len(),
        ..CoverageApplication::default()
    };
    let mut resolved_files = BTreeMap::new();

    for function in functions {
        function.coverage = None;
        function.crap = None;
        let lines = *resolved_files
            .entry(function.file.clone())
            .or_insert_with(|| coverage.lines_for_file(root, &function.file));
        let Some(lines) = lines else {
            application.unmatched_functions += 1;
            continue;
        };
        if function.start_line == 0 || function.end_line < function.start_line {
            application.empty_ranges += 1;
            continue;
        }
        let (covered, executable) = lines.range(function.start_line..=function.end_line).fold(
            (0_usize, 0_usize),
            |(covered, executable), (_, hits)| {
                (
                    covered.saturating_add(usize::from(*hits > 0)),
                    executable.saturating_add(1),
                )
            },
        );
        if executable == 0 {
            application.empty_ranges += 1;
            continue;
        }
        let covered = u32::try_from(covered).unwrap_or(u32::MAX);
        let executable = u32::try_from(executable).unwrap_or(u32::MAX);
        let percent = 100.0 * f64::from(covered) / f64::from(executable);
        function.coverage = Some(percent);
        function.crap = Some(crap_score(function.complexity, percent));
        application.matched_functions += 1;
    }

    application
}

/// Sort functions consistently for terminal, JSON, and SARIF reporters.
pub fn sort_by_risk(functions: &mut [FunctionRecord]) {
    functions.sort_by(|left, right| match (left.crap, right.crap) {
        (Some(left_score), Some(right_score)) => right_score
            .total_cmp(&left_score)
            .then_with(|| left.file.cmp(&right.file))
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.name.cmp(&right.name)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left
            .file
            .cmp(&right.file)
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.name.cmp(&right.name)),
    });
}

/// Analyze adapter-produced functions against an already-loaded report.
#[must_use]
pub fn analyze(
    root: &Path,
    mut functions: Vec<FunctionRecord>,
    coverage: Option<&CoverageReport>,
) -> CrapAnalysis {
    for function in &mut functions {
        function.coverage = None;
        function.crap = None;
    }
    let application = coverage.map(|report| apply_coverage(root, &mut functions, report));
    sort_by_risk(&mut functions);
    CrapAnalysis {
        functions,
        coverage: application,
    }
}

/// Convenience entry point for the CLI when a report path was supplied.
///
/// # Errors
///
/// Returns an error when coverage report discovery, reading, detection, or
/// parsing fails, or when the report contains no executable lines.
pub fn analyze_path(
    root: &Path,
    functions: Vec<FunctionRecord>,
    coverage_path: &Path,
) -> Result<CrapAnalysis, CoverageError> {
    let coverage = load_coverage(coverage_path)?;
    Ok(analyze(root, functions, Some(&coverage)))
}

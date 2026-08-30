//! Shared CRAP metric calculation and multi-format line-coverage ingestion.
//!
//! Language adapters provide [`FunctionRecord`] values. This crate deliberately
//! owns no parser: it maps normalized executable-line coverage to those
//! functions and applies the language-independent CRAP formula.

mod coverage;

use std::collections::{BTreeMap, BTreeSet};
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
    /// Functions where line-only coverage cannot separate an outer statement
    /// from a nested executable body on the same boundary line.
    #[serde(default)]
    pub ambiguous_functions: usize,
}

impl CoverageApplication {
    #[must_use]
    pub const fn missing_functions(self) -> usize {
        self.unmatched_functions + self.empty_ranges + self.ambiguous_functions
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
    apply_coverage_with_policy(root, functions, coverage, false)
}

/// Apply coverage with an explicit fail-closed policy for functions absent
/// from a region-aware report. When enabled, absence means 0% coverage; it
/// never means covered.
pub fn apply_coverage_with_policy(
    root: &Path,
    functions: &mut [FunctionRecord],
    coverage: &CoverageReport,
    unreported_as_zero: bool,
) -> CoverageApplication {
    CoverageMapper::new(root, functions, coverage, unreported_as_zero).apply_all(functions)
}

struct CoverageMapper<'a> {
    root: &'a Path,
    coverage: &'a CoverageReport,
    unreported_as_zero: bool,
    function_coverage: bool,
    regional: Option<BTreeMap<usize, Vec<&'a coverage::FunctionCoverage>>>,
    peer_ambiguous: BTreeSet<usize>,
    resolved_files: BTreeMap<String, Option<&'a LineCoverage>>,
    application: CoverageApplication,
}

impl<'a> CoverageMapper<'a> {
    fn new(
        root: &'a Path,
        functions: &[FunctionRecord],
        coverage: &'a CoverageReport,
        unreported_as_zero: bool,
    ) -> Self {
        let function_coverage = coverage.has_function_coverage();
        Self {
            root,
            coverage,
            unreported_as_zero,
            function_coverage,
            regional: function_coverage.then(|| assign_function_coverage(root, functions, coverage)),
            peer_ambiguous: peer_ambiguous_functions(root, functions, coverage),
            resolved_files: BTreeMap::new(),
            application: CoverageApplication {
                total_functions: functions.len(),
                ..CoverageApplication::default()
            },
        }
    }

    fn apply_all(mut self, functions: &mut [FunctionRecord]) -> CoverageApplication {
        for (index, function) in functions.iter_mut().enumerate() {
            self.apply_function(index, function);
        }
        self.application
    }

    fn apply_function(&mut self, index: usize, function: &mut FunctionRecord) {
        function.coverage = None;
        function.crap = None;
        let lines = *self
            .resolved_files
            .entry(function.file.clone())
            .or_insert_with(|| self.coverage.lines_for_file(self.root, &function.file));
        let Some(lines) = lines else {
            self.apply_missing_file(function);
            return;
        };
        self.apply_resolved_function(index, function, lines);
    }

    fn apply_missing_file(&mut self, function: &mut FunctionRecord) {
        if self.function_coverage && self.unreported_as_zero {
            self.apply_zero_coverage(function);
        } else {
            self.application.unmatched_functions += 1;
        }
    }

    fn apply_resolved_function(&mut self, index: usize, function: &mut FunctionRecord, lines: &LineCoverage) {
        if self.apply_regional(index, function) {
            return;
        }
        if self.apply_unreported_region(function) {
            return;
        }
        if self.apply_invalid_range(function) {
            return;
        }
        if self.apply_ambiguous_range(index, function, lines) {
            return;
        }
        self.apply_line_coverage(function, lines);
    }

    fn apply_regional(&mut self, index: usize, function: &mut FunctionRecord) -> bool {
        if let Some(regional) = self.regional.as_ref().and_then(|regional| regional.get(&index)) {
            apply_regional_function(function, regional);
            self.application.matched_functions += 1;
            true
        } else {
            false
        }
    }

    fn apply_unreported_region(&mut self, function: &mut FunctionRecord) -> bool {
        if !self.function_coverage
            || self
                .coverage
                .functions_for_file(self.root, &function.file)
                .is_none()
        {
            return false;
        }
        if self.unreported_as_zero {
            self.apply_zero_coverage(function);
        } else {
            self.application.empty_ranges += 1;
        }
        true
    }

    fn apply_zero_coverage(&mut self, function: &mut FunctionRecord) {
        function.coverage = Some(0.0);
        function.crap = Some(crap_score(function.complexity, 0.0));
        self.application.matched_functions += 1;
    }

    fn apply_invalid_range(&mut self, function: &FunctionRecord) -> bool {
        if function.start_line == 0 || function.end_line < function.start_line {
            self.application.empty_ranges += 1;
            true
        } else {
            false
        }
    }

    fn apply_ambiguous_range(
        &mut self,
        index: usize,
        function: &FunctionRecord,
        lines: &LineCoverage,
    ) -> bool {
        if boundary_is_executable(function, lines) || self.peer_ambiguous.contains(&index) {
            self.application.ambiguous_functions += 1;
            true
        } else {
            false
        }
    }

    fn apply_line_coverage(&mut self, function: &mut FunctionRecord, lines: &LineCoverage) {
        let (covered, executable) = owned_line_counts(function, lines);
        if executable == 0 {
            self.application.empty_ranges += 1;
            return;
        }
        let percent = coverage_percent(covered, executable);
        function.coverage = Some(percent);
        function.crap = Some(crap_score(function.complexity, percent));
        self.application.matched_functions += 1;
    }
}

fn boundary_is_executable(function: &FunctionRecord, lines: &LineCoverage) -> bool {
    function.coverage_excluded_ranges.iter().any(|(start, end)| {
        [*start, *end].into_iter().any(|boundary| {
            function.start_line <= boundary && boundary <= function.end_line && lines.contains_key(&boundary)
        })
    })
}

fn owned_line_counts(function: &FunctionRecord, lines: &LineCoverage) -> (usize, usize) {
    lines.range(function.start_line..=function.end_line).fold(
        (0_usize, 0_usize),
        |(covered, executable), (line, hits)| {
            if function
                .coverage_excluded_ranges
                .iter()
                .any(|(start, end)| *start < *line && *line < *end)
            {
                return (covered, executable);
            }
            (
                covered.saturating_add(usize::from(*hits > 0)),
                executable.saturating_add(1),
            )
        },
    )
}

fn assign_function_coverage<'a>(
    root: &Path,
    functions: &[FunctionRecord],
    coverage: &'a CoverageReport,
) -> BTreeMap<usize, Vec<&'a coverage::FunctionCoverage>> {
    let by_file = function_indices_by_file(functions);

    let mut assigned: BTreeMap<usize, Vec<&coverage::FunctionCoverage>> = BTreeMap::new();
    for (file, indices) in by_file {
        let Some(regions) = coverage.functions_for_file(root, file) else {
            continue;
        };
        for region in regions {
            let candidate = indices
                .iter()
                .copied()
                .filter(|index| function_contains_region(&functions[*index], region))
                .min_by(|left, right| {
                    function_span_key(&functions[*left]).cmp(&function_span_key(&functions[*right]))
                });
            if let Some(index) = candidate {
                assigned.entry(index).or_default().push(region);
            }
        }
    }
    assigned
}

fn function_contains_region(function: &FunctionRecord, region: &coverage::FunctionCoverage) -> bool {
    if has_precise_coverage_span(function) {
        precise_region_belongs_to_function(function, region)
    } else {
        line_region_belongs_to_function(function, region)
    }
}

fn has_precise_coverage_span(function: &FunctionRecord) -> bool {
    let span = function.coverage_span;
    span.start_line > 0 && span.end_line >= span.start_line
}

fn precise_region_belongs_to_function(
    function: &FunctionRecord,
    region: &coverage::FunctionCoverage,
) -> bool {
    region_spans_fit(region, function.coverage_span)
        && region_avoids_excluded_spans(region, &function.coverage_excluded_spans)
}

fn region_spans_fit(region: &coverage::FunctionCoverage, outer: reporigor_core::CoverageSpan) -> bool {
    region.spans.iter().all(|span| span_within(*span, outer))
}

fn region_avoids_excluded_spans(
    region: &coverage::FunctionCoverage,
    excluded_spans: &[reporigor_core::CoverageSpan],
) -> bool {
    !region.spans.iter().any(|region_span| {
        excluded_spans
            .iter()
            .any(|excluded| spans_intersect(*region_span, *excluded))
    })
}

fn line_region_belongs_to_function(function: &FunctionRecord, region: &coverage::FunctionCoverage) -> bool {
    region_spans_fit_lines(region, function.start_line, function.end_line)
        && region_avoids_excluded_ranges(region, &function.coverage_excluded_ranges)
}

fn region_spans_fit_lines(region: &coverage::FunctionCoverage, start: u32, end: u32) -> bool {
    region
        .spans
        .iter()
        .all(|span| start <= span.start_line && span.end_line <= end)
}

fn region_avoids_excluded_ranges(
    region: &coverage::FunctionCoverage,
    excluded_ranges: &[(u32, u32)],
) -> bool {
    !region.spans.iter().any(|span| {
        excluded_ranges
            .iter()
            .any(|range| span_intersects_line_range(*span, *range))
    })
}

fn span_intersects_line_range(span: reporigor_core::CoverageSpan, range: (u32, u32)) -> bool {
    span.start_line <= range.1 && range.0 <= span.end_line
}

fn span_within(inner: reporigor_core::CoverageSpan, outer: reporigor_core::CoverageSpan) -> bool {
    let inner_start = (inner.start_line, inner.start_column);
    let inner_end = (inner.end_line, inner.end_column);
    let outer_start = (outer.start_line, outer.start_column);
    let outer_end = (outer.end_line, outer.end_column);
    outer_start <= inner_start && inner_end <= outer_end
}

fn spans_intersect(left: reporigor_core::CoverageSpan, right: reporigor_core::CoverageSpan) -> bool {
    let left_start = (left.start_line, left.start_column);
    let left_end = (left.end_line, left.end_column);
    let right_start = (right.start_line, right.start_column);
    let right_end = (right.end_line, right.end_column);
    left_start < right_end && right_start < left_end
}

fn function_span_key(function: &FunctionRecord) -> (u32, u32, &str, &str) {
    let lines = function.end_line.saturating_sub(function.start_line);
    let columns = if lines == 0 {
        function
            .coverage_span
            .end_column
            .saturating_sub(function.coverage_span.start_column)
    } else {
        u32::MAX
    };
    (
        lines,
        columns,
        function.stable_symbol.as_str(),
        function.name.as_str(),
    )
}

fn apply_regional_function(function: &mut FunctionRecord, regions: &[&coverage::FunctionCoverage]) {
    let (lines, any_hits) = regional_line_hits(function, regions);
    let percent = regional_coverage_percent(&lines, any_hits);
    function.coverage = Some(percent);
    function.crap = Some(crap_score(function.complexity, percent));
}

fn regional_line_hits(
    function: &FunctionRecord,
    regions: &[&coverage::FunctionCoverage],
) -> (BTreeMap<u32, u64>, bool) {
    let mut lines: BTreeMap<u32, u64> = BTreeMap::new();
    let mut any_hits = false;
    for region in regions {
        merge_region_lines(function, region, &mut lines, &mut any_hits);
    }
    (lines, any_hits)
}

fn merge_region_lines(
    function: &FunctionRecord,
    region: &coverage::FunctionCoverage,
    lines: &mut BTreeMap<u32, u64>,
    any_hits: &mut bool,
) {
    for (&line, &hits) in &region.lines {
        merge_regional_line(function, lines, any_hits, line, hits);
    }
}

fn merge_regional_line(
    function: &FunctionRecord,
    lines: &mut BTreeMap<u32, u64>,
    any_hits: &mut bool,
    line: u32,
    hits: u64,
) {
    if line_is_excluded(function, line) {
        return;
    }
    let merged = lines.entry(line).or_default();
    *merged = (*merged).max(hits);
    *any_hits |= hits > 0;
}

fn line_is_excluded(function: &FunctionRecord, line: u32) -> bool {
    function
        .coverage_excluded_ranges
        .iter()
        .any(|(start, end)| *start <= line && line <= *end)
}

fn regional_coverage_percent(lines: &BTreeMap<u32, u64>, any_hits: bool) -> f64 {
    let executable = lines.len();
    let covered = lines.values().filter(|hits| **hits > 0).count();
    if executable == 0 {
        empty_regional_coverage_percent(any_hits)
    } else {
        coverage_percent(covered, executable)
    }
}

fn empty_regional_coverage_percent(any_hits: bool) -> f64 {
    if any_hits {
        100.0
    } else {
        0.0
    }
}

fn coverage_percent(covered: usize, executable: usize) -> f64 {
    let covered = u32::try_from(covered).unwrap_or(u32::MAX);
    let executable = u32::try_from(executable).unwrap_or(u32::MAX);
    100.0 * f64::from(covered) / f64::from(executable)
}

fn peer_ambiguous_functions(
    root: &Path,
    functions: &[FunctionRecord],
    coverage: &CoverageReport,
) -> BTreeSet<usize> {
    let by_file = function_indices_by_file(functions);

    let mut ambiguous = BTreeSet::new();
    for (file, indices) in by_file {
        if let Some(lines) = coverage.lines_for_file(root, file) {
            ambiguous.extend(ambiguous_functions_for_file(functions, indices, lines));
        }
    }
    ambiguous
}

fn function_indices_by_file(functions: &[FunctionRecord]) -> BTreeMap<&str, Vec<usize>> {
    let mut by_file = BTreeMap::new();
    for (index, function) in functions.iter().enumerate() {
        by_file
            .entry(function.file.as_str())
            .or_insert_with(Vec::new)
            .push(index);
    }
    by_file
}

fn ambiguous_functions_for_file(
    functions: &[FunctionRecord],
    indices: Vec<usize>,
    lines: &LineCoverage,
) -> BTreeSet<usize> {
    let mut segments = coverage_segments(functions, indices);
    segments.sort_unstable();
    mark_ambiguous_segments(lines, &segments)
}

fn coverage_segments(functions: &[FunctionRecord], indices: Vec<usize>) -> Vec<(u32, u32, usize)> {
    indices
        .into_iter()
        .flat_map(|index| {
            coverage_owned_segments(&functions[index])
                .into_iter()
                .map(move |(start, end)| (start, end, index))
        })
        .collect()
}

fn mark_ambiguous_segments(lines: &LineCoverage, segments: &[(u32, u32, usize)]) -> BTreeSet<usize> {
    let mut endings = segments.to_vec();
    endings.sort_unstable_by_key(|(start, end, index)| (*end, *start, *index));
    let mut ambiguous = BTreeSet::new();
    let mut active = BTreeSet::new();
    let mut active_unmarked = BTreeSet::new();
    let mut next_start = 0;
    let mut next_end = 0;
    for line in lines.keys().copied() {
        expire_segments(line, &endings, &mut next_end, &mut active, &mut active_unmarked);
        activate_segments(
            line,
            segments,
            &mut next_start,
            &ambiguous,
            &mut active,
            &mut active_unmarked,
        );
        mark_active_ambiguities(&active, &mut active_unmarked, &mut ambiguous);
    }
    ambiguous
}

fn expire_segments(
    line: u32,
    endings: &[(u32, u32, usize)],
    next_end: &mut usize,
    active: &mut BTreeSet<usize>,
    active_unmarked: &mut BTreeSet<usize>,
) {
    while endings.get(*next_end).is_some_and(|(_, end, _)| *end < line) {
        let (_, _, index) = endings[*next_end];
        active.remove(&index);
        active_unmarked.remove(&index);
        *next_end += 1;
    }
}

fn activate_segments(
    line: u32,
    segments: &[(u32, u32, usize)],
    next_start: &mut usize,
    ambiguous: &BTreeSet<usize>,
    active: &mut BTreeSet<usize>,
    active_unmarked: &mut BTreeSet<usize>,
) {
    while segments
        .get(*next_start)
        .is_some_and(|(start, _, _)| *start <= line)
    {
        let (_, end, index) = segments[*next_start];
        if end >= line {
            active.insert(index);
            if !ambiguous.contains(&index) {
                active_unmarked.insert(index);
            }
        }
        *next_start += 1;
    }
}

fn mark_active_ambiguities(
    active: &BTreeSet<usize>,
    active_unmarked: &mut BTreeSet<usize>,
    ambiguous: &mut BTreeSet<usize>,
) {
    if active.len() > 1 && !active_unmarked.is_empty() {
        ambiguous.extend(active_unmarked.iter().copied());
        active_unmarked.clear();
    }
}

fn coverage_owned_segments(function: &FunctionRecord) -> Vec<(u32, u32)> {
    if function.start_line == 0 || function.end_line < function.start_line {
        return Vec::new();
    }
    let mut excluded_interiors = excluded_coverage_interiors(function);
    excluded_interiors.sort_unstable();
    segments_outside_exclusions(function, excluded_interiors)
}

fn excluded_coverage_interiors(function: &FunctionRecord) -> Vec<(u32, u32)> {
    function
        .coverage_excluded_ranges
        .iter()
        .filter_map(|(start, end)| {
            let start = start.saturating_add(1).max(function.start_line);
            let end = end.saturating_sub(1).min(function.end_line);
            (start <= end).then_some((start, end))
        })
        .collect()
}

fn segments_outside_exclusions(
    function: &FunctionRecord,
    excluded_interiors: Vec<(u32, u32)>,
) -> Vec<(u32, u32)> {
    let mut segments = Vec::new();
    let mut cursor = function.start_line;
    for (start, end) in excluded_interiors {
        if end < cursor {
            continue;
        }
        if cursor < start {
            segments.push((cursor, start - 1));
        }
        cursor = cursor.max(end.saturating_add(1));
        if cursor > function.end_line {
            break;
        }
    }
    append_remaining_segment(function.end_line, cursor, &mut segments);
    segments
}

fn append_remaining_segment(end_line: u32, cursor: u32, segments: &mut Vec<(u32, u32)>) {
    if cursor <= end_line {
        segments.push((cursor, end_line));
    }
}

/// Sort functions consistently for terminal, JSON, and SARIF reporters.
pub fn sort_by_risk(functions: &mut [FunctionRecord]) {
    functions.sort_by(|left, right| match (left.crap, right.crap) {
        (Some(left_score), Some(right_score)) => right_score
            .total_cmp(&left_score)
            .then_with(|| compare_source_identity(left, right)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => compare_source_identity(left, right),
    });
}

fn compare_source_identity(left: &FunctionRecord, right: &FunctionRecord) -> std::cmp::Ordering {
    (&left.file, left.start_line, &left.name).cmp(&(&right.file, right.start_line, &right.name))
}

/// Analyze adapter-produced functions against an already-loaded report.
#[must_use]
pub fn analyze(
    root: &Path,
    functions: Vec<FunctionRecord>,
    coverage: Option<&CoverageReport>,
) -> CrapAnalysis {
    analyze_with_policy(root, functions, coverage, false)
}

/// Analyze functions with an explicit unreported-function policy.
#[must_use]
pub fn analyze_with_policy(
    root: &Path,
    mut functions: Vec<FunctionRecord>,
    coverage: Option<&CoverageReport>,
    unreported_as_zero: bool,
) -> CrapAnalysis {
    for function in &mut functions {
        function.coverage = None;
        function.crap = None;
    }
    let application =
        coverage.map(|report| apply_coverage_with_policy(root, &mut functions, report, unreported_as_zero));
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
    analyze_path_with_policy(root, functions, coverage_path, false)
}

/// Load a report and analyze with an explicit unreported-function policy.
///
/// # Errors
///
/// Returns the same errors as [`analyze_path`].
pub fn analyze_path_with_policy(
    root: &Path,
    functions: Vec<FunctionRecord>,
    coverage_path: &Path,
    unreported_as_zero: bool,
) -> Result<CrapAnalysis, CoverageError> {
    let coverage = load_coverage(coverage_path)?;
    Ok(analyze_with_policy(
        root,
        functions,
        Some(&coverage),
        unreported_as_zero,
    ))
}

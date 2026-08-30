use std::collections::BTreeMap;

use reporigor_core::{Language, MutationResult, MutationStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{pretty_json, ReportEnvelope, ReportError, MUTATION_ELEMENTS_SCHEMA_VERSION};

/// Mutation score thresholds used by Mutation Testing Elements consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationThresholds {
    pub high: u8,
    pub low: u8,
}

impl MutationThresholds {
    /// Create validated score thresholds.
    ///
    /// # Errors
    ///
    /// Returns an error unless `low <= high` and both values are at most 100.
    pub const fn new(low: u8, high: u8) -> Result<Self, ReportError> {
        if low > high || high > 100 {
            return Err(ReportError::InvalidMutationThresholds { low, high });
        }
        Ok(Self { high, low })
    }
}

impl Default for MutationThresholds {
    fn default() -> Self {
        Self { high: 80, low: 60 }
    }
}

/// Project mutation results into the Mutation Testing Elements v2 report
/// schema. Source text is mandatory in that schema and must be supplied for
/// every file containing a mutant.
///
/// # Errors
///
/// Returns an error when the report has no mutation section, required source
/// text is missing, a candidate span disagrees with that source, thresholds
/// are invalid, or serialization fails.
pub fn mutation_elements_value(
    report: &ReportEnvelope,
    sources: &BTreeMap<String, String>,
    thresholds: MutationThresholds,
) -> Result<Value, ReportError> {
    validate_thresholds(thresholds)?;
    let mutation = report
        .results
        .mutate
        .as_ref()
        .ok_or(ReportError::MissingSection("mutation"))?;
    let grouped = group_mutants(&mutation.mutants);
    let files = project_files(grouped, sources)?;
    let output = MutationElementsReport {
        schema_version: MUTATION_ELEMENTS_SCHEMA_VERSION,
        thresholds,
        project_root: report.root.to_string_lossy().into_owned(),
        files,
        framework: MutationFramework {
            name: report.tool.name.clone(),
            version: report.tool.version.clone(),
        },
    };
    Ok(serde_json::to_value(output)?)
}

fn group_mutants(mutants: &[MutationResult]) -> BTreeMap<&str, (Language, Vec<&MutationResult>)> {
    let mut grouped = BTreeMap::new();
    for mutant in mutants {
        let entry = grouped
            .entry(mutant.mutation.file.as_str())
            .or_insert_with(|| (mutant.mutation.language, Vec::new()));
        entry.1.push(mutant);
    }
    grouped
}

fn project_files(
    grouped: BTreeMap<&str, (Language, Vec<&MutationResult>)>,
    sources: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, MutationElementsFile>, ReportError> {
    grouped
        .into_iter()
        .map(|(file, (language, mutants))| {
            project_file(file, language, mutants, sources).map(|projected| (file.to_owned(), projected))
        })
        .collect()
}

fn project_file(
    file: &str,
    language: Language,
    mut mutants: Vec<&MutationResult>,
    sources: &BTreeMap<String, String>,
) -> Result<MutationElementsFile, ReportError> {
    let source = sources
        .get(file)
        .ok_or_else(|| ReportError::MissingMutationSource(file.to_owned()))?;
    sort_mutants(&mut mutants);
    validate_mutants(&mutants, source)?;
    let positions = source_position_index(source, &mutants);
    let mutants = mutants
        .into_iter()
        .map(|mutant| project_mutant(mutant, &positions))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(MutationElementsFile {
        language: mutation_language(language),
        source: source.clone(),
        mutants,
    })
}

fn sort_mutants(mutants: &mut [&MutationResult]) {
    mutants.sort_by(|left, right| {
        (
            left.mutation.line,
            left.mutation.column,
            left.mutation.id,
            &left.mutation.replacement,
        )
            .cmp(&(
                right.mutation.line,
                right.mutation.column,
                right.mutation.id,
                &right.mutation.replacement,
            ))
    });
}

fn validate_mutants(mutants: &[&MutationResult], source: &str) -> Result<(), ReportError> {
    for mutant in mutants {
        validate_mutant_span(mutant, source)?;
    }
    Ok(())
}

/// Serialize the Mutation Testing Elements projection as deterministic pretty
/// JSON.
///
/// # Errors
///
/// Returns the same projection and serialization errors as
/// [`mutation_elements_value`].
pub fn mutation_elements_json(
    report: &ReportEnvelope,
    sources: &BTreeMap<String, String>,
    thresholds: MutationThresholds,
) -> Result<String, ReportError> {
    pretty_json(&mutation_elements_value(report, sources, thresholds)?)
}

const fn validate_thresholds(thresholds: MutationThresholds) -> Result<(), ReportError> {
    if thresholds.low > thresholds.high || thresholds.high > 100 {
        return Err(ReportError::InvalidMutationThresholds {
            low: thresholds.low,
            high: thresholds.high,
        });
    }
    Ok(())
}

fn validate_mutant_span(result: &MutationResult, source: &str) -> Result<(), ReportError> {
    let mutation = &result.mutation;
    validate_span_order(result)?;
    validate_span_bounds(result, source)?;
    validate_span_boundaries(result, source)?;
    validate_original_text(result, source, mutation.start_byte..mutation.end_byte)
}

fn validate_span_order(result: &MutationResult) -> Result<(), ReportError> {
    let mutation = &result.mutation;
    if mutation.start_byte > mutation.end_byte {
        return Err(invalid_span(result, "the byte range is reversed"));
    }
    Ok(())
}

fn validate_span_bounds(result: &MutationResult, source: &str) -> Result<(), ReportError> {
    let mutation = &result.mutation;
    if mutation.end_byte > source.len() {
        return Err(invalid_span(
            result,
            format!(
                "byte range {}..{} is outside the {}-byte source",
                mutation.start_byte,
                mutation.end_byte,
                source.len()
            ),
        ));
    }
    Ok(())
}

fn validate_span_boundaries(result: &MutationResult, source: &str) -> Result<(), ReportError> {
    let endpoints = [result.mutation.start_byte, result.mutation.end_byte];
    if !endpoints
        .into_iter()
        .all(|offset| source.is_char_boundary(offset))
    {
        return Err(invalid_span(result, "the byte range splits a UTF-8 scalar value"));
    }
    Ok(())
}

fn validate_original_text(
    result: &MutationResult,
    source: &str,
    range: std::ops::Range<usize>,
) -> Result<(), ReportError> {
    if source.get(range) != Some(result.mutation.original.as_str()) {
        return Err(invalid_span(
            result,
            "the original text does not match the supplied source at the byte range",
        ));
    }
    Ok(())
}

fn project_mutant(
    result: &MutationResult,
    positions: &SourcePositionIndex,
) -> Result<MutationElementsMutant, ReportError> {
    let mutation = &result.mutation;
    let start = positions.position(result, mutation.start_byte, "start")?;
    let end = positions.position(result, mutation.end_byte, "end")?;
    if mutation.line != start.line || mutation.column != start.column {
        return Err(invalid_span(
            result,
            format!(
                "recorded start {}:{} does not match byte offset {} ({}:{})",
                mutation.line, mutation.column, mutation.start_byte, start.line, start.column
            ),
        ));
    }
    let (status, invalid_reason) = mutation_status(result.status);
    let status_reason = result
        .detail
        .clone()
        .or_else(|| invalid_reason.map(str::to_owned));
    let duration = result
        .duration_seconds
        .is_finite()
        .then(|| result.duration_seconds.max(0.0) * 1_000.0);

    Ok(MutationElementsMutant {
        id: result.mutation.id.to_string(),
        mutator_name: mutator_name(&result.mutation.original, &result.mutation.replacement),
        description: format!(
            "Replace `{}` with `{}`",
            result.mutation.original, result.mutation.replacement
        ),
        replacement: result.mutation.replacement.clone(),
        location: MutationLocation { start, end },
        status,
        status_reason,
        duration,
    })
}

#[derive(Debug)]
struct SourcePositionIndex {
    offsets: Vec<usize>,
    positions: Vec<(usize, usize)>,
}

impl SourcePositionIndex {
    fn position(
        &self,
        result: &MutationResult,
        offset: usize,
        endpoint: &str,
    ) -> Result<MutationPosition, ReportError> {
        let index = self.offsets.binary_search(&offset).map_err(|_| {
            invalid_span(
                result,
                format!("the {endpoint} byte offset is missing from the source coordinate index"),
            )
        })?;
        let (line, column) = self.positions.get(index).copied().ok_or_else(|| {
            invalid_span(
                result,
                format!("the {endpoint} byte offset is missing from the source coordinate index"),
            )
        })?;
        Ok(MutationPosition {
            line: u32::try_from(line).map_err(|_| {
                invalid_span(
                    result,
                    format!("the {endpoint} position exceeds the report coordinate range"),
                )
            })?,
            column: u32::try_from(column).map_err(|_| {
                invalid_span(
                    result,
                    format!("the {endpoint} position exceeds the report coordinate range"),
                )
            })?,
        })
    }
}

fn source_position_index(source: &str, mutants: &[&MutationResult]) -> SourcePositionIndex {
    let offsets = requested_offsets(mutants);
    let positions = scan_positions(source, &offsets);
    debug_assert_eq!(positions.len(), offsets.len());
    SourcePositionIndex { offsets, positions }
}

fn requested_offsets(mutants: &[&MutationResult]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(mutants.len().saturating_mul(2));
    for mutant in mutants {
        offsets.push(mutant.mutation.start_byte);
        offsets.push(mutant.mutation.end_byte);
    }
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

fn scan_positions(source: &str, offsets: &[usize]) -> Vec<(usize, usize)> {
    let mut positions = Vec::with_capacity(offsets.len());
    let mut requested = 0;
    let mut line = 1_usize;
    let mut column = 1_usize;
    for (byte_offset, character) in source.char_indices() {
        while offsets.get(requested) == Some(&byte_offset) {
            positions.push((line, column));
            requested += 1;
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    while offsets.get(requested) == Some(&source.len()) {
        positions.push((line, column));
        requested += 1;
    }
    debug_assert_eq!(requested, offsets.len());
    positions
}

fn invalid_span(result: &MutationResult, message: impl Into<String>) -> ReportError {
    ReportError::InvalidMutationSpan {
        id: result.mutation.id,
        file: result.mutation.file.clone(),
        message: message.into(),
    }
}

fn mutation_status(status: MutationStatus) -> (String, Option<&'static str>) {
    if status == MutationStatus::Invalid {
        return (
            "Ignored".to_owned(),
            Some("The mutation candidate was rejected before execution."),
        );
    }
    (format!("{status:?}"), None)
}

fn mutation_language(language: Language) -> &'static str {
    crate::indexed_name(
        "bash|c|cpp|objectivec|python|rust|swift|typescript",
        language as usize,
        "rust",
    )
}

fn mutator_name(original: &str, replacement: &str) -> &'static str {
    binary_mutator_name(original, replacement)
        .or_else(|| boolean_mutator_name(original))
        .unwrap_or("GenericMutation")
}

fn binary_mutator_name(original: &str, replacement: &str) -> Option<&'static str> {
    const CLASSES: &str =
        "==,!=:EqualityOperator|>,>=,<,<=:RelationalOperator|&&,||,and,or:LogicalOperator|+,-,*,/:ArithmeticOperator";
    CLASSES.split('|').find_map(|class| {
        let (operators, name) = class.split_once(':')?;
        same_operator_class(operators, original, replacement).then_some(name)
    })
}

fn same_operator_class(operators: &str, original: &str, replacement: &str) -> bool {
    let contains = |candidate| operators.split(',').any(|operator| operator == candidate);
    contains(original) && contains(replacement)
}

fn boolean_mutator_name(original: &str) -> Option<&'static str> {
    ["true", "false", "True", "False", "YES", "NO"]
        .contains(&original)
        .then_some("BooleanLiteral")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationElementsReport {
    schema_version: &'static str,
    thresholds: MutationThresholds,
    project_root: String,
    files: BTreeMap<String, MutationElementsFile>,
    framework: MutationFramework,
}

#[derive(Debug, Serialize)]
struct MutationFramework {
    name: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct MutationElementsFile {
    language: &'static str,
    source: String,
    mutants: Vec<MutationElementsMutant>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationElementsMutant {
    id: String,
    mutator_name: &'static str,
    description: String,
    replacement: String,
    location: MutationLocation,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct MutationLocation {
    start: MutationPosition,
    end: MutationPosition,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct MutationPosition {
    line: u32,
    column: u32,
}

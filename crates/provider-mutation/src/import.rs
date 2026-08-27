use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use reporigor_core::{
    read_bounded_utf8_file_within, CoreError, Language, MutationCandidate, MutationResult, MutationStatus,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{ImportFormat, ImportedMutation, ImportedMutationReport, MutationProvider, ProviderError};

const MAX_REPORT_BYTES: usize = 32 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 1_024;
const MAX_MUTANTS: usize = 100_000;
const MAX_RETAINED_BYTES_PER_MUTANT: usize = 1024 * 1024;
const MAX_TOTAL_RETAINED_BYTES: usize = 32 * 1024 * 1024;
const MAX_RETAINED_SOURCE_AND_INDEX_BYTES: usize = 64 * 1024 * 1024;
const COORDINATE_CHECKPOINT_STRIDE_BYTES: usize = 256;
const MAX_MUTER_FILE_REPORTS: usize = 1_024;
const MAX_MUTER_TRAVERSAL_ENTRIES: usize = 100_000;
const MAX_MUTER_TRAVERSAL_DEPTH: usize = 64;

#[derive(Debug, Default)]
struct ImportBudget {
    source_files: usize,
    mutants: usize,
    retained_bytes: usize,
    retained_source_and_index_bytes: usize,
}

impl ImportBudget {
    fn add_source_file(&mut self, source_bytes: usize) -> Result<(), ProviderError> {
        if source_bytes > MAX_SOURCE_BYTES {
            return Err(limit_error("sourceBytes", source_bytes, MAX_SOURCE_BYTES));
        }
        self.source_files = self
            .source_files
            .checked_add(1)
            .ok_or_else(|| limit_error("sourceFiles", usize::MAX, MAX_SOURCE_FILES))?;
        if self.source_files > MAX_SOURCE_FILES {
            return Err(limit_error("sourceFiles", self.source_files, MAX_SOURCE_FILES));
        }
        self.add_source_or_index_bytes(source_bytes)
    }

    fn add_coordinate_index(&mut self, index_bytes: usize) -> Result<(), ProviderError> {
        self.add_source_or_index_bytes(index_bytes)
    }

    fn add_source_or_index_bytes(&mut self, bytes: usize) -> Result<(), ProviderError> {
        let retained = self
            .retained_source_and_index_bytes
            .checked_add(bytes)
            .ok_or_else(|| {
                limit_error(
                    "retainedSourceAndIndexBytes",
                    usize::MAX,
                    MAX_RETAINED_SOURCE_AND_INDEX_BYTES,
                )
            })?;
        if retained > MAX_RETAINED_SOURCE_AND_INDEX_BYTES {
            return Err(limit_error(
                "retainedSourceAndIndexBytes",
                retained,
                MAX_RETAINED_SOURCE_AND_INDEX_BYTES,
            ));
        }
        self.retained_source_and_index_bytes = retained;
        Ok(())
    }

    fn add_mutant(&mut self, original_bytes: usize, replacement_bytes: usize) -> Result<(), ProviderError> {
        if original_bytes > MAX_RETAINED_BYTES_PER_MUTANT {
            return Err(limit_error(
                "originalBytesPerMutant",
                original_bytes,
                MAX_RETAINED_BYTES_PER_MUTANT,
            ));
        }
        if replacement_bytes > MAX_RETAINED_BYTES_PER_MUTANT {
            return Err(limit_error(
                "replacementBytesPerMutant",
                replacement_bytes,
                MAX_RETAINED_BYTES_PER_MUTANT,
            ));
        }
        self.mutants = self
            .mutants
            .checked_add(1)
            .ok_or_else(|| limit_error("mutants", usize::MAX, MAX_MUTANTS))?;
        if self.mutants > MAX_MUTANTS {
            return Err(limit_error("mutants", self.mutants, MAX_MUTANTS));
        }
        let retained = original_bytes
            .checked_add(replacement_bytes)
            .and_then(|bytes| self.retained_bytes.checked_add(bytes))
            .ok_or_else(|| limit_error("totalRetainedMutationBytes", usize::MAX, MAX_TOTAL_RETAINED_BYTES))?;
        if retained > MAX_TOTAL_RETAINED_BYTES {
            return Err(limit_error(
                "totalRetainedMutationBytes",
                retained,
                MAX_TOTAL_RETAINED_BYTES,
            ));
        }
        self.retained_bytes = retained;
        Ok(())
    }
}

fn limit_error(name: &str, observed: usize, maximum: usize) -> ProviderError {
    invalid(
        format!("limits.{name}"),
        format!("observed {observed} bytes/items, maximum is {maximum}"),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EffectiveCandidate {
    file: String,
    start: usize,
    end: usize,
    replacement: String,
}

#[derive(Debug, Default)]
struct CandidateRegistry {
    external_ids: BTreeSet<String>,
    effective: BTreeSet<EffectiveCandidate>,
}

impl CandidateRegistry {
    fn add_external_id(&mut self, id: &str, field: &str) -> Result<(), ProviderError> {
        if !self.external_ids.insert(id.to_owned()) {
            return Err(invalid(field, format!("duplicate upstream mutation id {id:?}")));
        }
        Ok(())
    }

    fn add_effective(
        &mut self,
        file: &str,
        span: &std::ops::Range<usize>,
        replacement: &str,
        field: &str,
    ) -> Result<(), ProviderError> {
        let candidate = EffectiveCandidate {
            file: file.to_owned(),
            start: span.start,
            end: span.end,
            replacement: replacement.to_owned(),
        };
        if !self.effective.insert(candidate) {
            return Err(invalid(field, "duplicate effective mutation candidate"));
        }
        Ok(())
    }
}

/// Import an existing JSON report without running a mutation engine.
///
/// Mutation Testing Elements v2 is accepted for every provider and is the
/// native format emitted by Stryker's JSON reporter. cargo-mutants'
/// `mutants.out/outcomes.json` is also recognized, with explicit schema checks
/// because its upstream documentation marks the format as changeable.
///
/// # Errors
///
/// Returns an error for malformed JSON, unsupported schemas, unsafe paths, or
/// invalid mutation coordinates/statuses.
pub fn import_json(
    root: &Path,
    provider: MutationProvider,
    input: &str,
) -> Result<ImportedMutationReport, ProviderError> {
    let root = canonical_root(root)?;
    import_json_from_root(&root, provider, input)
}

fn import_json_from_root(
    root: &Path,
    provider: MutationProvider,
    input: &str,
) -> Result<ImportedMutationReport, ProviderError> {
    if input.len() > MAX_REPORT_BYTES {
        return Err(limit_error("reportBytes", input.len(), MAX_REPORT_BYTES));
    }
    let value: Value = serde_json::from_str(input)?;
    if looks_like_mte(&value) {
        return import_mte(root, provider, value);
    }
    if provider == MutationProvider::CargoMutants && value.get("outcomes").is_some() {
        return import_cargo_mutants(root, &value);
    }
    if provider == MutationProvider::Muter && value.get("fileReports").is_some() {
        return import_muter(root, &value);
    }
    Err(ProviderError::UnsupportedReport {
        provider,
        message: "expected Mutation Testing Elements schemaVersion 2.x".to_owned()
            + if provider == MutationProvider::CargoMutants {
                " or cargo-mutants outcomes.json"
            } else {
                ""
            },
    })
}

/// Read and import a provider report from disk.
///
/// # Errors
///
/// Returns filesystem and report validation errors.
pub fn import_path(
    root: &Path,
    provider: MutationProvider,
    report: &Path,
) -> Result<ImportedMutationReport, ProviderError> {
    let root = canonical_root(root)?;
    let input = read_contained_file(&root, report, "report", MAX_REPORT_BYTES)?;
    import_json_from_root(&root, provider, &input)
}

fn looks_like_mte(value: &Value) -> bool {
    value.get("files").is_some()
        && (value.get("schemaVersion").is_some() || value.get("schema_version").is_some())
}

fn mte_schema_major(version: &str) -> Option<u8> {
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    for part in &parts {
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || part.len() > 1 && part.starts_with('0')
        {
            return None;
        }
    }
    match parts[0] {
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MteReport {
    #[serde(alias = "schema_version")]
    schema_version: String,
    thresholds: MteThresholds,
    files: BTreeMap<String, MteFile>,
    #[serde(default)]
    framework: Option<MteFramework>,
}

#[derive(Debug, Deserialize)]
struct MteThresholds {
    high: u64,
    low: u64,
}

#[derive(Debug, Deserialize)]
struct MteFramework {
    name: String,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MteFile {
    language: String,
    source: String,
    mutants: Vec<MteMutant>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MteMutant {
    id: String,
    mutator_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    replacement: String,
    location: ReportLocation,
    status: String,
    #[serde(default)]
    status_reason: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct ReportLocation {
    start: ReportPosition,
    end: ReportPosition,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct ReportPosition {
    line: u32,
    column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoordinateMode {
    UnicodeScalar,
    JavascriptUtf16,
}

#[derive(Debug)]
struct CoordinateTable {
    lines: Vec<CoordinateLine>,
    checkpoints: Vec<CoordinateCheckpoint>,
    mode: CoordinateMode,
}

#[derive(Debug)]
struct CachedSource {
    text: String,
    coordinates: CoordinateTable,
}

#[derive(Debug)]
struct CoordinateLine {
    start: u32,
    end: u32,
    checkpoint_start: u32,
    checkpoint_count: u32,
}

#[derive(Debug, Clone, Copy)]
struct CoordinateCheckpoint {
    byte_offset: u32,
    scalar_index: u32,
    coordinate_index: u32,
}

#[derive(Debug, Clone, Copy)]
struct CoordinateIndexShape {
    lines: usize,
    checkpoints: usize,
    retained_bytes: usize,
}

impl CoordinateTable {
    fn new(source: &str, mode: CoordinateMode, budget: &mut ImportBudget) -> Result<Self, ProviderError> {
        let shape = coordinate_index_shape(source, mode)?;
        budget.add_coordinate_index(shape.retained_bytes)?;

        let mut lines = Vec::new();
        lines.try_reserve_exact(shape.lines).map_err(|_| {
            invalid(
                "limits.coordinateIndex",
                "unable to reserve coordinate line index",
            )
        })?;
        let mut checkpoints = Vec::new();
        checkpoints.try_reserve_exact(shape.checkpoints).map_err(|_| {
            invalid(
                "limits.coordinateIndex",
                "unable to reserve coordinate checkpoints",
            )
        })?;

        let mut line_start = 0;
        let mut checkpoint_start = 0;
        let mut checkpoint_count = 0;
        let mut last_checkpoint = 0;
        let mut scalar_index = 0_u32;
        let mut coordinate_index = 0_u32;
        for (index, character) in source.char_indices() {
            if index < line_start {
                continue;
            }
            if let Some(terminator_bytes) = line_terminator_bytes(source, index, character, mode) {
                push_coordinate_line(&mut lines, line_start, index, checkpoint_start, checkpoint_count)?;
                line_start = index + terminator_bytes;
                checkpoint_start = checkpoints.len();
                checkpoint_count = 0;
                last_checkpoint = line_start;
                scalar_index = 0;
                coordinate_index = 0;
                continue;
            }

            scalar_index = scalar_index
                .checked_add(1)
                .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
            coordinate_index = coordinate_index
                .checked_add(coordinate_width(character, mode))
                .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
            let after = index + character.len_utf8();
            if after - last_checkpoint >= COORDINATE_CHECKPOINT_STRIDE_BYTES {
                checkpoints.push(CoordinateCheckpoint {
                    byte_offset: u32::try_from(after)
                        .map_err(|_| invalid("limits.sourceBytes", "source offset does not fit in u32"))?,
                    scalar_index,
                    coordinate_index,
                });
                checkpoint_count = checkpoint_count
                    .checked_add(1)
                    .ok_or_else(|| invalid("limits.coordinateIndex", "too many coordinate checkpoints"))?;
                last_checkpoint = after;
            }
        }
        push_coordinate_line(
            &mut lines,
            line_start,
            source.len(),
            checkpoint_start,
            checkpoint_count,
        )?;
        debug_assert_eq!(lines.len(), shape.lines);
        debug_assert!(checkpoints.len() <= shape.checkpoints);
        Ok(Self {
            lines,
            checkpoints,
            mode,
        })
    }

    fn offset(&self, source: &str, position: ReportPosition) -> Result<usize, ProviderError> {
        if position.line == 0 || position.column == 0 {
            return Err(invalid("location", "line and column must both be at least 1"));
        }
        let line_index = usize::try_from(position.line - 1)
            .map_err(|_| invalid("location.line", "line does not fit this platform"))?;
        let line = self.lines.get(line_index).ok_or_else(|| {
            invalid(
                "location.line",
                format!("line {} is outside source", position.line),
            )
        })?;
        let target = position.column - 1;
        let checkpoints = self.line_checkpoints(line)?;
        let checkpoint_index =
            checkpoints.partition_point(|checkpoint| checkpoint.coordinate_index <= target);
        let (mut byte_offset, mut coordinate_index) = if let Some(index) = checkpoint_index.checked_sub(1) {
            let checkpoint = checkpoints
                .get(index)
                .ok_or_else(|| invalid("limits.coordinateIndex", "coordinate checkpoint disappeared"))?;
            (checkpoint.byte_offset, checkpoint.coordinate_index)
        } else {
            (line.start, 0)
        };
        if coordinate_index == target {
            return usize::try_from(byte_offset)
                .map_err(|_| invalid("location", "source offset does not fit this platform"));
        }

        let line_end = usize::try_from(line.end)
            .map_err(|_| invalid("location", "source offset does not fit this platform"))?;
        let scan_start = usize::try_from(byte_offset)
            .map_err(|_| invalid("location", "source offset does not fit this platform"))?;
        for (relative, character) in source[scan_start..line_end].char_indices() {
            coordinate_index = coordinate_index
                .checked_add(coordinate_width(character, self.mode))
                .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
            byte_offset = u32::try_from(scan_start + relative + character.len_utf8())
                .map_err(|_| invalid("location", "source offset does not fit in u32"))?;
            if coordinate_index == target {
                return usize::try_from(byte_offset)
                    .map_err(|_| invalid("location", "source offset does not fit this platform"));
            }
            if coordinate_index > target {
                return Err(invalid(
                    "location.column",
                    format!(
                        "column {} splits a UTF-16 surrogate pair on line {}",
                        position.column, position.line
                    ),
                ));
            }
        }
        Err(invalid(
            "location.column",
            format!("column {} is outside line {}", position.column, position.line),
        ))
    }

    fn scalar_position(&self, source: &str, offset: usize) -> Result<(u32, u32), ProviderError> {
        let offset =
            u32::try_from(offset).map_err(|_| invalid("location", "source offset does not fit in u32"))?;
        let line_index = self.lines.partition_point(|line| line.start <= offset);
        let line_index = line_index
            .checked_sub(1)
            .ok_or_else(|| invalid("location", "offset precedes source"))?;
        let line = &self.lines[line_index];
        if offset > line.end {
            return Err(invalid("location", "offset points into a line terminator"));
        }
        let checkpoints = self.line_checkpoints(line)?;
        let checkpoint_index = checkpoints.partition_point(|checkpoint| checkpoint.byte_offset <= offset);
        let (byte_offset, scalar_index) = if let Some(index) = checkpoint_index.checked_sub(1) {
            let checkpoint = checkpoints
                .get(index)
                .ok_or_else(|| invalid("limits.coordinateIndex", "coordinate checkpoint disappeared"))?;
            (checkpoint.byte_offset, checkpoint.scalar_index)
        } else {
            (line.start, 0)
        };
        let byte_offset = usize::try_from(byte_offset)
            .map_err(|_| invalid("location", "source offset does not fit this platform"))?;
        let offset = usize::try_from(offset)
            .map_err(|_| invalid("location", "source offset does not fit this platform"))?;
        let remainder = source
            .get(byte_offset..offset)
            .ok_or_else(|| invalid("location", "offset splits a Unicode scalar value"))?;
        let scalar_index = scalar_index
            .checked_add(
                u32::try_from(remainder.chars().count())
                    .map_err(|_| invalid("location.column", "source column does not fit in u32"))?,
            )
            .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
        let display_line = u32::try_from(line_index + 1)
            .map_err(|_| invalid("location.line", "source line does not fit in u32"))?;
        let display_column = scalar_index
            .checked_add(1)
            .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
        Ok((display_line, display_column))
    }

    fn utf8_position(&self, source: &str, offset: usize) -> Result<(u32, u32, u32), ProviderError> {
        let (line, scalar_column) = self.scalar_position(source, offset)?;
        let line_index = usize::try_from(line - 1)
            .map_err(|_| invalid("location.line", "source line does not fit this platform"))?;
        let line_start = usize::try_from(self.lines[line_index].start)
            .map_err(|_| invalid("location", "source offset does not fit this platform"))?;
        let utf8_column = u32::try_from(offset - line_start + 1)
            .map_err(|_| invalid("location.column", "source column does not fit in u32"))?;
        Ok((line, utf8_column, scalar_column))
    }

    fn line_checkpoints(&self, line: &CoordinateLine) -> Result<&[CoordinateCheckpoint], ProviderError> {
        let start = usize::try_from(line.checkpoint_start).map_err(|_| {
            invalid(
                "limits.coordinateIndex",
                "checkpoint offset does not fit this platform",
            )
        })?;
        let count = usize::try_from(line.checkpoint_count).map_err(|_| {
            invalid(
                "limits.coordinateIndex",
                "checkpoint count does not fit this platform",
            )
        })?;
        let end = start
            .checked_add(count)
            .ok_or_else(|| invalid("limits.coordinateIndex", "checkpoint range overflowed"))?;
        self.checkpoints
            .get(start..end)
            .ok_or_else(|| invalid("limits.coordinateIndex", "checkpoint range is invalid"))
    }
}

fn coordinate_index_shape(source: &str, mode: CoordinateMode) -> Result<CoordinateIndexShape, ProviderError> {
    let mut lines = 1_usize;
    let mut line_start = 0;
    for (index, character) in source.char_indices() {
        if index < line_start {
            continue;
        }
        if let Some(terminator_bytes) = line_terminator_bytes(source, index, character, mode) {
            lines = lines
                .checked_add(1)
                .ok_or_else(|| limit_error("coordinateLines", usize::MAX, usize::MAX))?;
            line_start = index + terminator_bytes;
        }
    }
    let checkpoints = source.len() / COORDINATE_CHECKPOINT_STRIDE_BYTES;
    let retained_bytes = lines
        .checked_mul(std::mem::size_of::<CoordinateLine>())
        .and_then(|bytes| {
            checkpoints
                .checked_mul(std::mem::size_of::<CoordinateCheckpoint>())
                .and_then(|checkpoint_bytes| bytes.checked_add(checkpoint_bytes))
        })
        .ok_or_else(|| {
            limit_error(
                "retainedSourceAndIndexBytes",
                usize::MAX,
                MAX_RETAINED_SOURCE_AND_INDEX_BYTES,
            )
        })?;
    Ok(CoordinateIndexShape {
        lines,
        checkpoints,
        retained_bytes,
    })
}

fn line_terminator_bytes(source: &str, index: usize, character: char, mode: CoordinateMode) -> Option<usize> {
    match (mode, character) {
        (CoordinateMode::UnicodeScalar, '\n') => Some(1),
        (CoordinateMode::JavascriptUtf16, '\r') => {
            Some(if source.as_bytes().get(index + 1) == Some(&b'\n') {
                2
            } else {
                1
            })
        }
        (CoordinateMode::JavascriptUtf16, '\n' | '\u{2028}' | '\u{2029}') => Some(character.len_utf8()),
        _ => None,
    }
}

const fn coordinate_width(character: char, mode: CoordinateMode) -> u32 {
    match mode {
        CoordinateMode::UnicodeScalar => 1,
        CoordinateMode::JavascriptUtf16 => {
            if character as u32 >= 0x1_0000 {
                2
            } else {
                1
            }
        }
    }
}

fn push_coordinate_line(
    lines: &mut Vec<CoordinateLine>,
    start: usize,
    end: usize,
    checkpoint_start: usize,
    checkpoint_count: usize,
) -> Result<(), ProviderError> {
    lines.push(CoordinateLine {
        start: u32::try_from(start)
            .map_err(|_| invalid("limits.sourceBytes", "source offset does not fit in u32"))?,
        end: u32::try_from(end)
            .map_err(|_| invalid("limits.sourceBytes", "source offset does not fit in u32"))?,
        checkpoint_start: u32::try_from(checkpoint_start)
            .map_err(|_| invalid("limits.coordinateIndex", "checkpoint offset does not fit in u32"))?,
        checkpoint_count: u32::try_from(checkpoint_count)
            .map_err(|_| invalid("limits.coordinateIndex", "checkpoint count does not fit in u32"))?,
    });
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn import_mte(
    root: &Path,
    provider: MutationProvider,
    value: Value,
) -> Result<ImportedMutationReport, ProviderError> {
    let document: MteReport = serde_json::from_value(value)?;
    let mte_v2 = validate_mte_header(&document, provider)?;
    let stryker_coordinates = document
        .framework
        .as_ref()
        .is_some_and(|framework| framework.name.eq_ignore_ascii_case("StrykerJS"))
        || (document.framework.is_none() && provider == MutationProvider::Stryker);
    let coordinate_mode = if stryker_coordinates {
        CoordinateMode::JavascriptUtf16
    } else {
        CoordinateMode::UnicodeScalar
    };

    let mut results = Vec::new();
    let mut used_ids = BTreeSet::new();
    let mut budget = ImportBudget::default();
    let mut registry = CandidateRegistry::default();
    let mut normalized_files = BTreeSet::new();
    for (file, entry) in document.files {
        budget.add_source_file(entry.source.len())?;
        let file = normalized_relative(root, &file)?;
        if !normalized_files.insert(file.clone()) {
            return Err(invalid(
                "files",
                format!("duplicate normalized source file {file:?}"),
            ));
        }
        let language = report_language(&entry.language)?;
        let coordinates = CoordinateTable::new(&entry.source, coordinate_mode, &mut budget)?;
        for mutant in entry.mutants {
            if mutant.mutator_name.trim().is_empty() {
                return Err(invalid(
                    "files.*.mutants[].mutatorName",
                    "expected a non-empty string",
                ));
            }
            let status = mte_status(&mutant.status)?;
            let span = source_span(&entry.source, &coordinates, mutant.location)?;
            budget.add_mutant(span.len(), mutant.replacement.len())?;
            registry.add_external_id(&mutant.id, "files.*.mutants[].id")?;
            registry.add_effective(&file, &span, &mutant.replacement, "files.*.mutants[]")?;
            let (line, column) = coordinates.scalar_position(&entry.source, span.start)?;
            let duration_seconds = match mutant.duration {
                Some(duration) if duration.is_finite() && duration >= 0.0 => duration / 1_000.0,
                Some(_) => {
                    return Err(invalid(
                        "files.*.mutants[].duration",
                        "expected a finite non-negative millisecond duration",
                    ));
                }
                None => 0.0,
            };
            let mut id = stable_id(provider, &file, &mutant.id);
            while !used_ids.insert(id) {
                id = id.wrapping_add(1);
            }
            let detail = mutant.status_reason.or_else(|| {
                let mut detail = Some(mutant.mutator_name);
                if let Some(description) = mutant.description {
                    match &mut detail {
                        Some(prefix) if !description.is_empty() => {
                            prefix.push_str(": ");
                            prefix.push_str(&description);
                        }
                        None if !description.is_empty() => detail = Some(description),
                        _ => {}
                    }
                }
                detail
            });
            results.push(ImportedMutation {
                external_id: mutant.id,
                result: MutationResult {
                    mutation: MutationCandidate {
                        id,
                        language,
                        file: file.clone(),
                        line,
                        column,
                        original: entry.source[span.start..span.end].to_owned(),
                        replacement: mutant.replacement,
                        start_byte: span.start,
                        end_byte: span.end,
                    },
                    status,
                    exit_code: None,
                    duration_seconds,
                    detail,
                },
            });
        }
    }
    sort_results(&mut results);
    Ok(ImportedMutationReport {
        provider,
        format: if mte_v2 {
            ImportFormat::MutationTestingElementsV2
        } else {
            ImportFormat::MutationTestingElementsV1
        },
        framework_name: document
            .framework
            .as_ref()
            .map(|framework| framework.name.clone()),
        framework_version: document.framework.and_then(|framework| framework.version),
        results,
        warnings: Vec::new(),
    })
}

fn validate_mte_header(document: &MteReport, provider: MutationProvider) -> Result<bool, ProviderError> {
    let Some(schema_major) = mte_schema_major(&document.schema_version) else {
        return Err(invalid(
            "schemaVersion",
            format!(
                "invalid Mutation Testing Elements version {:?}",
                document.schema_version
            ),
        ));
    };
    let mte_v2 = schema_major == 2;
    if !(mte_v2 || provider == MutationProvider::Mull && schema_major == 1) {
        return Err(invalid(
            "schemaVersion",
            format!("expected 2.x, found {}", document.schema_version),
        ));
    }
    if document.thresholds.high > 100
        || document.thresholds.low > 100
        || document.thresholds.low > document.thresholds.high
    {
        return Err(invalid(
            "thresholds",
            "expected integer thresholds satisfying 0 <= low <= high <= 100",
        ));
    }
    if document
        .framework
        .as_ref()
        .is_some_and(|framework| framework.name.trim().is_empty())
    {
        return Err(invalid("framework.name", "expected a non-empty string"));
    }
    Ok(mte_v2)
}

#[allow(clippy::too_many_lines)]
fn import_cargo_mutants(root: &Path, value: &Value) -> Result<ImportedMutationReport, ProviderError> {
    let version = required_string(value, "cargo_mutants_version", "report")?.to_owned();
    required_string(value, "end_time", "report")?;
    let outcomes = value
        .get("outcomes")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("outcomes", "expected an array"))?;
    let mut sources = BTreeMap::<String, CachedSource>::new();
    let mut results = Vec::new();
    let mut used_ids = BTreeSet::new();
    let mut budget = ImportBudget::default();
    let mut registry = CandidateRegistry::default();

    for (index, outcome) in outcomes.iter().enumerate() {
        let field = format!("outcomes[{index}]");
        let scenario = outcome
            .get("scenario")
            .ok_or_else(|| invalid(format!("{field}.scenario"), "missing scenario"))?;
        if scenario.as_str() == Some("Baseline") {
            required_string(outcome, "summary", &field)?;
            continue;
        }
        let mutant = scenario
            .get("Mutant")
            .filter(|mutant| mutant.is_object())
            .ok_or_else(|| {
                invalid(
                    format!("{field}.scenario"),
                    "expected \"Baseline\" or an object containing Mutant",
                )
            })?;
        let file_value = required_string(mutant, "file", &field)?;
        let file = normalized_relative(root, file_value)?;
        if !sources.contains_key(&file) {
            let source = read_contained_file(
                root,
                Path::new(&file),
                &format!("{field}.scenario.Mutant.file"),
                MAX_SOURCE_BYTES,
            )?;
            budget.add_source_file(source.len())?;
            let coordinates = CoordinateTable::new(&source, CoordinateMode::UnicodeScalar, &mut budget)?;
            sources.insert(
                file.clone(),
                CachedSource {
                    text: source,
                    coordinates,
                },
            );
        }
        let source = sources
            .get(&file)
            .ok_or_else(|| invalid(&field, "source cache entry disappeared"))?;
        let location: ReportLocation = serde_json::from_value(
            mutant
                .get("span")
                .cloned()
                .ok_or_else(|| invalid(format!("{field}.scenario.Mutant.span"), "missing span"))?,
        )?;
        let span = source_span(&source.text, &source.coordinates, location)?;
        let external_id = required_string(mutant, "name", &format!("{field}.scenario.Mutant"))?.to_owned();
        if external_id.trim().is_empty() {
            return Err(invalid(
                format!("{field}.scenario.Mutant.name"),
                "expected a non-empty string",
            ));
        }
        let replacement = required_string(mutant, "replacement", &format!("{field}.scenario.Mutant"))?;
        budget.add_mutant(span.len(), replacement.len())?;
        registry.add_external_id(&external_id, &format!("{field}.scenario.Mutant.name"))?;
        registry.add_effective(&file, &span, replacement, &field)?;
        let mut id = stable_id(MutationProvider::CargoMutants, &file, &external_id);
        while !used_ids.insert(id) {
            id = id.wrapping_add(1);
        }
        let summary = required_string(outcome, "summary", &field)?;
        let status = cargo_mutants_status(summary)?;
        let duration_seconds = outcome
            .get("phase_results")
            .and_then(Value::as_array)
            .map_or(0.0, |phases| {
                phases
                    .iter()
                    .filter_map(|phase| phase.get("duration").and_then(Value::as_f64))
                    .filter(|duration| duration.is_finite() && *duration >= 0.0)
                    .sum()
            });
        results.push(ImportedMutation {
            external_id,
            result: MutationResult {
                mutation: MutationCandidate {
                    id,
                    language: Language::Rust,
                    file,
                    line: location.start.line,
                    column: location.start.column,
                    original: source.text[span.start..span.end].to_owned(),
                    replacement: replacement.to_owned(),
                    start_byte: span.start,
                    end_byte: span.end,
                },
                status,
                exit_code: None,
                duration_seconds,
                detail: mutant.get("name").and_then(Value::as_str).map(str::to_owned),
            },
        });
    }
    sort_results(&mut results);
    Ok(ImportedMutationReport {
        provider: MutationProvider::CargoMutants,
        format: ImportFormat::CargoMutantsOutcomes,
        framework_name: Some("cargo-mutants".to_owned()),
        framework_version: Some(version),
        results,
        warnings: vec![
            "cargo-mutants documents outcomes.json as subject to change; this importer validates the recognized fields and rejects incompatible shapes".to_owned(),
        ],
    })
}

#[allow(clippy::too_many_lines)]
fn import_muter(root: &Path, value: &Value) -> Result<ImportedMutationReport, ProviderError> {
    let file_reports = value
        .get("fileReports")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("fileReports", "expected an array"))?;
    if file_reports.len() > MAX_MUTER_FILE_REPORTS {
        return Err(limit_error(
            "fileReports",
            file_reports.len(),
            MAX_MUTER_FILE_REPORTS,
        ));
    }
    let mut requested_basenames = BTreeSet::new();
    for (file_index, file_report) in file_reports.iter().enumerate() {
        let field = format!("fileReports[{file_index}]");
        let file_name = required_string(file_report, "fileName", &field)?;
        validate_muter_basename(file_name, &format!("{field}.fileName"))?;
        requested_basenames.insert(file_name.to_owned());
    }
    let basename_index = build_muter_basename_index(root, &requested_basenames)?;
    let mut results = Vec::new();
    let mut used_ids = BTreeSet::new();
    let mut sources = BTreeMap::<String, CachedSource>::new();
    let mut budget = ImportBudget::default();
    let mut registry = CandidateRegistry::default();
    for (file_index, file_report) in file_reports.iter().enumerate() {
        let field = format!("fileReports[{file_index}]");
        let file_name = required_string(file_report, "fileName", &field)?;
        let file = resolve_indexed_basename(&basename_index, file_name, &format!("{field}.fileName"))?;
        if !sources.contains_key(&file) {
            let source = read_contained_file(
                root,
                Path::new(&file),
                &format!("{field}.fileName"),
                MAX_SOURCE_BYTES,
            )?;
            budget.add_source_file(source.len())?;
            let coordinates = CoordinateTable::new(&source, CoordinateMode::UnicodeScalar, &mut budget)?;
            sources.insert(
                file.clone(),
                CachedSource {
                    text: source,
                    coordinates,
                },
            );
        }
        let source = sources
            .get(&file)
            .ok_or_else(|| invalid(&field, "source cache entry disappeared"))?;
        let operators = file_report
            .get("appliedOperators")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid(format!("{field}.appliedOperators"), "expected an array"))?;
        for (operator_index, operator) in operators.iter().enumerate() {
            let operator_field = format!("{field}.appliedOperators[{operator_index}]");
            let status_value = required_string(operator, "testSuiteOutcome", &operator_field)?;
            // Muter emits one synthetic null operator for an entirely
            // uncovered file. It is file-level coverage metadata, not a
            // mutation candidate (all coordinates and snapshots are null).
            if status_value == "noCoverage" {
                if operator.get("mutationPoint").is_some_and(Value::is_null)
                    && operator.get("mutationSnapshot").is_some_and(Value::is_null)
                {
                    continue;
                }
                return Err(invalid(
                    format!("{operator_field}.testSuiteOutcome"),
                    "noCoverage is only valid for Muter's null file-level sentinel",
                ));
            }
            let status = muter_status(status_value)?;
            let point = operator
                .get("mutationPoint")
                .filter(|point| point.is_object())
                .ok_or_else(|| invalid(format!("{operator_field}.mutationPoint"), "expected an object"))?;
            let snapshot = operator
                .get("mutationSnapshot")
                .filter(|snapshot| snapshot.is_object())
                .ok_or_else(|| invalid(format!("{operator_field}.mutationSnapshot"), "expected an object"))?;
            let operator_name = required_string(point, "mutationOperatorId", &operator_field)?;
            if operator_name.trim().is_empty() {
                return Err(invalid(
                    format!("{operator_field}.mutationPoint.mutationOperatorId"),
                    "expected a non-empty string",
                ));
            }
            let original =
                required_string(snapshot, "before", &format!("{operator_field}.mutationSnapshot"))?;
            let replacement =
                required_string(snapshot, "after", &format!("{operator_field}.mutationSnapshot"))?;
            let description = required_string(
                snapshot,
                "description",
                &format!("{operator_field}.mutationSnapshot"),
            )?;
            let position = point
                .get("position")
                .filter(|position| position.is_object())
                .ok_or_else(|| {
                    invalid(
                        format!("{operator_field}.mutationPoint.position"),
                        "expected an object",
                    )
                })?;
            let position = validated_muter_position(
                &source.text,
                &source.coordinates,
                position,
                &format!("{operator_field}.mutationPoint.position"),
            )?;
            let start = position.utf8_offset;
            let end = if original.is_empty() {
                start
            } else {
                let end = start.checked_add(original.len()).ok_or_else(|| {
                    invalid(
                        format!("{operator_field}.mutationSnapshot.before"),
                        "snapshot length overflows the source offset",
                    )
                })?;
                if source.text.get(start..end) == Some(original) {
                    end
                } else {
                    return Err(invalid(
                        format!("{operator_field}.mutationSnapshot.before"),
                        "snapshot text does not match source at the reported position",
                    ));
                }
            };
            budget.add_mutant(end - start, replacement.len())?;
            registry.add_effective(&file, &(start..end), replacement, &operator_field)?;
            let external_id = format!(
                "{file}:{}:{}:{operator_name}:{}",
                position.line,
                position.utf8_column,
                operator_index.saturating_add(1)
            );
            registry.add_external_id(&external_id, &format!("{operator_field}.externalId"))?;
            let mut id = stable_id(MutationProvider::Muter, &file, &external_id);
            while !used_ids.insert(id) {
                id = id.wrapping_add(1);
            }
            results.push(ImportedMutation {
                external_id,
                result: MutationResult {
                    mutation: MutationCandidate {
                        id,
                        language: Language::Swift,
                        file: file.clone(),
                        line: position.line,
                        column: position.scalar_column,
                        original: original.to_owned(),
                        replacement: replacement.to_owned(),
                        start_byte: start,
                        end_byte: end,
                    },
                    status,
                    exit_code: None,
                    duration_seconds: 0.0,
                    detail: Some(if description.is_empty() {
                        operator_name.to_owned()
                    } else {
                        description.to_owned()
                    }),
                },
            });
        }
    }
    sort_results(&mut results);
    Ok(ImportedMutationReport {
        provider: MutationProvider::Muter,
        format: ImportFormat::MuterJson,
        framework_name: Some("Muter".to_owned()),
        framework_version: None,
        results,
        warnings: vec![
            "Muter JSON is unversioned and omits source paths; reporigor accepts it only when every file basename resolves uniquely under the project root".to_owned(),
        ],
    })
}

fn muter_status(value: &str) -> Result<MutationStatus, ProviderError> {
    match value {
        "passed" => Ok(MutationStatus::Survived),
        "failed" | "runtimeError" => Ok(MutationStatus::Killed),
        "buildError" => Ok(MutationStatus::CompileError),
        "timeout" => Ok(MutationStatus::Timeout),
        _ => Err(invalid(
            "fileReports[].appliedOperators[].testSuiteOutcome",
            format!("unsupported Muter outcome {value:?}"),
        )),
    }
}

#[derive(Debug, Clone, Copy)]
struct ValidatedMuterPosition {
    utf8_offset: usize,
    line: u32,
    utf8_column: u32,
    scalar_column: u32,
}

fn validated_muter_position(
    source: &str,
    coordinates: &CoordinateTable,
    position: &Value,
    field: &str,
) -> Result<ValidatedMuterPosition, ProviderError> {
    let line = required_u32(position, "line", field)?;
    let utf8_column = required_u32(position, "column", field)?;
    if line == 0 || utf8_column == 0 {
        return Err(invalid(field, "line and column must both be at least 1"));
    }
    let utf8_offset = required_usize(position, "utf8Offset", field)?;
    if utf8_offset > source.len() || !source.is_char_boundary(utf8_offset) {
        return Err(invalid(
            format!("{field}.utf8Offset"),
            "offset is outside the UTF-8 source or splits a Unicode scalar value",
        ));
    }
    let (actual_line, actual_utf8_column, scalar_column) = coordinates.utf8_position(source, utf8_offset)?;
    if line != actual_line || utf8_column != actual_utf8_column {
        return Err(invalid(
            field,
            format!(
                "line/column {line}:{utf8_column} does not match utf8Offset {utf8_offset} ({actual_line}:{actual_utf8_column})"
            ),
        ));
    }
    Ok(ValidatedMuterPosition {
        utf8_offset,
        line,
        utf8_column,
        scalar_column,
    })
}

fn validate_muter_basename(file_name: &str, field: &str) -> Result<(), ProviderError> {
    if Path::new(file_name).file_name().and_then(|name| name.to_str()) != Some(file_name) {
        return Err(invalid(field, "expected a basename"));
    }
    if file_name.is_empty() || matches!(file_name, "." | "..") {
        return Err(invalid(field, "expected a non-empty file basename"));
    }
    Ok(())
}

fn resolve_indexed_basename(
    index: &BTreeMap<String, Vec<String>>,
    file_name: &str,
    field: &str,
) -> Result<String, ProviderError> {
    match index.get(file_name).map(Vec::as_slice).unwrap_or_default() {
        [path] => Ok(path.clone()),
        [] => Err(invalid(
            field,
            format!("could not resolve {file_name:?} under project root"),
        )),
        _ => Err(invalid(
            field,
            format!("basename {file_name:?} is ambiguous under project root"),
        )),
    }
}

fn build_muter_basename_index(
    root: &Path,
    requested: &BTreeSet<String>,
) -> Result<BTreeMap<String, Vec<String>>, ProviderError> {
    let mut index = BTreeMap::new();
    if requested.is_empty() {
        return Ok(index);
    }
    let mut traversed_entries = 0;
    collect_muter_basenames(root, root, 0, requested, &mut index, &mut traversed_entries)?;
    Ok(index)
}

fn collect_muter_basenames(
    root: &Path,
    directory: &Path,
    depth: usize,
    requested: &BTreeSet<String>,
    index: &mut BTreeMap<String, Vec<String>>,
    traversed_entries: &mut usize,
) -> Result<(), ProviderError> {
    if depth > MAX_MUTER_TRAVERSAL_DEPTH {
        return Err(limit_error(
            "muterTraversalDepth",
            depth,
            MAX_MUTER_TRAVERSAL_DEPTH,
        ));
    }
    let entries = fs::read_dir(directory).map_err(|source| ProviderError::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ProviderError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        *traversed_entries = traversed_entries
            .checked_add(1)
            .ok_or_else(|| limit_error("muterTraversalEntries", usize::MAX, MAX_MUTER_TRAVERSAL_ENTRIES))?;
        if *traversed_entries > MAX_MUTER_TRAVERSAL_ENTRIES {
            return Err(limit_error(
                "muterTraversalEntries",
                *traversed_entries,
                MAX_MUTER_TRAVERSAL_ENTRIES,
            ));
        }
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| ProviderError::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            let name = entry.file_name();
            if !matches!(
                name.to_str(),
                Some(".git" | ".build" | "build" | "target" | "node_modules" | ".venv" | "venv")
            ) {
                collect_muter_basenames(
                    root,
                    &path,
                    depth.saturating_add(1),
                    requested,
                    index,
                    traversed_entries,
                )?;
            }
        } else if file_type.is_file() {
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !requested.contains(&file_name) {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| invalid("fileReports[].fileName", "resolved file escaped project root"))?;
            let normalized = normalized_relative(root, &relative.to_string_lossy())?;
            let matches = index.entry(file_name).or_default();
            // One path is resolvable and two are enough to prove ambiguity;
            // retaining every same-basename file would make a tiny Muter
            // report consume memory proportional to the whole checkout.
            if matches.len() < 2 {
                matches.push(normalized);
            }
        }
    }
    Ok(())
}

fn required_string<'a>(object: &'a Value, name: &str, parent: &str) -> Result<&'a str, ProviderError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{parent}.{name}"), "expected a string"))
}

fn required_u32(object: &Value, name: &str, parent: &str) -> Result<u32, ProviderError> {
    let value = object
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(format!("{parent}.{name}"), "expected an unsigned integer"))?;
    u32::try_from(value).map_err(|_| invalid(format!("{parent}.{name}"), "value does not fit in u32"))
}

fn required_usize(object: &Value, name: &str, parent: &str) -> Result<usize, ProviderError> {
    let value = object
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(format!("{parent}.{name}"), "expected an unsigned integer"))?;
    usize::try_from(value)
        .map_err(|_| invalid(format!("{parent}.{name}"), "value does not fit on this platform"))
}

fn report_language(value: &str) -> Result<Language, ProviderError> {
    let normalized = value.to_ascii_lowercase().replace(['-', '_'], "");
    let language = match normalized.as_str() {
        "bash" | "shell" => Some(Language::Bash),
        "c" => Some(Language::C),
        "cpp" | "c++" | "cxx" => Some(Language::Cpp),
        "objectivec" | "objc" => Some(Language::ObjectiveC),
        "python" | "py" => Some(Language::Python),
        "rust" | "rs" => Some(Language::Rust),
        "swift" => Some(Language::Swift),
        "typescript" | "ts" | "tsx" => Some(Language::TypeScript),
        _ => None,
    };
    language.ok_or_else(|| invalid("files.*.language", format!("unsupported {value:?}")))
}

fn mte_status(value: &str) -> Result<MutationStatus, ProviderError> {
    match value {
        "Killed" => Ok(MutationStatus::Killed),
        "Survived" => Ok(MutationStatus::Survived),
        "NoCoverage" => Ok(MutationStatus::NoCoverage),
        "CompileError" => Ok(MutationStatus::CompileError),
        "RuntimeError" => Ok(MutationStatus::RuntimeError),
        "Timeout" => Ok(MutationStatus::Timeout),
        "Ignored" => Ok(MutationStatus::Ignored),
        "Pending" => Ok(MutationStatus::Pending),
        _ => Err(invalid(
            "files.*.mutants[].status",
            format!("unsupported status {value:?}"),
        )),
    }
}

fn cargo_mutants_status(value: &str) -> Result<MutationStatus, ProviderError> {
    match value {
        "CaughtMutant" => Ok(MutationStatus::Killed),
        "MissedMutant" | "Success" => Ok(MutationStatus::Survived),
        "Unviable" => Ok(MutationStatus::CompileError),
        "Failure" => Ok(MutationStatus::RuntimeError),
        "Timeout" => Ok(MutationStatus::Timeout),
        _ => Err(invalid(
            "outcomes[].summary",
            format!("unsupported cargo-mutants summary {value:?}"),
        )),
    }
}

fn source_span(
    source: &str,
    coordinates: &CoordinateTable,
    location: ReportLocation,
) -> Result<std::ops::Range<usize>, ProviderError> {
    let start = coordinates.offset(source, location.start)?;
    let end = coordinates.offset(source, location.end)?;
    if end < start {
        return Err(invalid("location", "end precedes start"));
    }
    Ok(start..end)
}

fn normalized_relative(root: &Path, value: &str) -> Result<String, ProviderError> {
    let relative = validated_relative_path(root, Path::new(value), "files")?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                unreachable!("validated_relative_path returns only normal relative components")
            }
        }
    }
    Ok(parts.join("/"))
}

fn canonical_root(root: &Path) -> Result<PathBuf, ProviderError> {
    if !root.is_dir() {
        return Err(ProviderError::InvalidRoot(root.to_path_buf()));
    }
    fs::canonicalize(root).map_err(|source| ProviderError::Io {
        path: root.to_path_buf(),
        source,
    })
}

fn validated_relative_path(root: &Path, input: &Path, field: &str) -> Result<PathBuf, ProviderError> {
    let relative = if input.is_absolute() {
        input.strip_prefix(root).map_err(|_| {
            invalid(
                field,
                format!(
                    "absolute path {:?} is outside project root",
                    input.to_string_lossy()
                ),
            )
        })?
    } else {
        input
    };
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(invalid(
                    field,
                    format!("path {:?} escapes project root", input.to_string_lossy()),
                ));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(invalid(field, "empty path"));
    }
    reject_escaping_symlink_prefixes(root, &normalized, field)?;
    Ok(normalized)
}

fn reject_escaping_symlink_prefixes(root: &Path, relative: &Path, field: &str) -> Result<(), ProviderError> {
    let mut candidate = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            unreachable!("relative path was normalized before symlink validation");
        };
        candidate.push(part);
        match fs::symlink_metadata(&candidate) {
            Ok(_) => {
                let canonical = fs::canonicalize(&candidate).map_err(|source| ProviderError::Io {
                    path: candidate.clone(),
                    source,
                })?;
                if !canonical.starts_with(root) {
                    return Err(invalid(
                        field,
                        format!(
                            "path {:?} resolves outside project root",
                            relative.to_string_lossy()
                        ),
                    ));
                }
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(ProviderError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn read_contained_file(
    root: &Path,
    input: &Path,
    field: &str,
    max_bytes: usize,
) -> Result<String, ProviderError> {
    let relative = validated_relative_path(root, input, field)?;
    let requested = root.join(&relative);
    let max_bytes_u64 = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    read_bounded_utf8_file_within(root, &requested, max_bytes_u64).map_err(|error| match error {
        CoreError::FileTooLarge { size, .. } => {
            limit_error(field, usize::try_from(size).unwrap_or(usize::MAX), max_bytes)
        }
        error => invalid(field, error.to_string()),
    })
}

fn stable_id(provider: MutationProvider, file: &str, external_id: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in provider
        .as_str()
        .bytes()
        .chain([0])
        .chain(file.bytes())
        .chain([0])
        .chain(external_id.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn sort_results(results: &mut [ImportedMutation]) {
    results.sort_by(|left, right| {
        (
            &left.result.mutation.file,
            left.result.mutation.line,
            left.result.mutation.column,
            &left.external_id,
        )
            .cmp(&(
                &right.result.mutation.file,
                right.result.mutation.line,
                right.result.mutation.column,
                &right.external_id,
            ))
    });
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> ProviderError {
    ProviderError::InvalidReport {
        field: field.into(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_budget_caps_mutant_count_and_aggregate_retained_bytes() {
        let mut count_budget = ImportBudget::default();
        for _ in 0..MAX_MUTANTS {
            count_budget
                .add_mutant(0, 0)
                .unwrap_or_else(|error| panic!("within count budget: {error}"));
        }
        assert!(matches!(
            count_budget.add_mutant(0, 0),
            Err(ProviderError::InvalidReport { ref field, .. }) if field == "limits.mutants"
        ));

        let mut byte_budget = ImportBudget::default();
        for _ in 0..(MAX_TOTAL_RETAINED_BYTES / MAX_RETAINED_BYTES_PER_MUTANT) {
            byte_budget
                .add_mutant(MAX_RETAINED_BYTES_PER_MUTANT, 0)
                .unwrap_or_else(|error| panic!("within retained-byte budget: {error}"));
        }
        assert!(matches!(
            byte_budget.add_mutant(1, 0),
            Err(ProviderError::InvalidReport { ref field, .. })
                if field == "limits.totalRetainedMutationBytes"
        ));
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use reporigor_core::{
    normalize_repository_path, read_bounded_utf8_file_within, CoreError, Language, MutationCandidate,
    MutationResult, MutationStatus,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    canonical_root, ImportFormat, ImportedMutation, ImportedMutationReport, MutationProvider, ProviderError,
};

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
        validate_mutant_bytes("originalBytesPerMutant", original_bytes)?;
        validate_mutant_bytes("replacementBytesPerMutant", replacement_bytes)?;
        self.increment_mutants()?;
        self.retained_bytes =
            retained_mutation_bytes(self.retained_bytes, original_bytes, replacement_bytes)?;
        Ok(())
    }

    fn increment_mutants(&mut self) -> Result<(), ProviderError> {
        self.mutants = self
            .mutants
            .checked_add(1)
            .ok_or_else(|| limit_error("mutants", usize::MAX, MAX_MUTANTS))?;
        if self.mutants > MAX_MUTANTS {
            return Err(limit_error("mutants", self.mutants, MAX_MUTANTS));
        }
        Ok(())
    }
}

fn validate_mutant_bytes(field: &str, bytes: usize) -> Result<(), ProviderError> {
    if bytes <= MAX_RETAINED_BYTES_PER_MUTANT {
        Ok(())
    } else {
        Err(limit_error(field, bytes, MAX_RETAINED_BYTES_PER_MUTANT))
    }
}

fn retained_mutation_bytes(
    current: usize,
    original: usize,
    replacement: usize,
) -> Result<usize, ProviderError> {
    let retained = original
        .checked_add(replacement)
        .and_then(|bytes| current.checked_add(bytes))
        .ok_or_else(|| limit_error("totalRetainedMutationBytes", usize::MAX, MAX_TOTAL_RETAINED_BYTES))?;
    if retained <= MAX_TOTAL_RETAINED_BYTES {
        Ok(retained)
    } else {
        Err(limit_error(
            "totalRetainedMutationBytes",
            retained,
            MAX_TOTAL_RETAINED_BYTES,
        ))
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
    match recognized_report(provider, &value) {
        Some(RecognizedReport::Mte) => import_mte(root, provider, value),
        Some(RecognizedReport::CargoMutants) => import_cargo_mutants(root, &value),
        Some(RecognizedReport::Muter) => import_muter(root, &value),
        None => Err(unsupported_report(provider)),
    }
}

#[derive(Clone, Copy)]
enum RecognizedReport {
    Mte,
    CargoMutants,
    Muter,
}

fn recognized_report(provider: MutationProvider, value: &Value) -> Option<RecognizedReport> {
    if looks_like_mte(value) {
        return Some(RecognizedReport::Mte);
    }
    match provider {
        MutationProvider::CargoMutants if value.get("outcomes").is_some() => {
            Some(RecognizedReport::CargoMutants)
        }
        MutationProvider::Muter if value.get("fileReports").is_some() => Some(RecognizedReport::Muter),
        _ => None,
    }
}

fn unsupported_report(provider: MutationProvider) -> ProviderError {
    let cargo_mutants_suffix = if provider == MutationProvider::CargoMutants {
        " or cargo-mutants outcomes.json"
    } else {
        ""
    };
    ProviderError::UnsupportedReport {
        provider,
        message: "expected Mutation Testing Elements schemaVersion 2.x".to_owned() + cargo_mutants_suffix,
    }
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
    if !(1..=3).contains(&parts.len()) || !parts.iter().all(|part| valid_schema_part(part)) {
        return None;
    }
    parts[0].parse().ok().filter(|major| matches!(major, 1 | 2))
}

fn valid_schema_part(part: &str) -> bool {
    !part.is_empty()
        && part.bytes().all(|byte| byte.is_ascii_digit())
        && (part.len() == 1 || !part.starts_with('0'))
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

fn ensure_cached_source(
    sources: &mut BTreeMap<String, CachedSource>,
    budget: &mut ImportBudget,
    root: &Path,
    file: &str,
    field: &str,
) -> Result<(), ProviderError> {
    if sources.contains_key(file) {
        return Ok(());
    }
    let source = read_contained_file(root, Path::new(file), field, MAX_SOURCE_BYTES)?;
    budget.add_source_file(source.len())?;
    let coordinates = CoordinateTable::new(&source, CoordinateMode::UnicodeScalar, budget)?;
    sources.insert(
        file.to_owned(),
        CachedSource {
            text: source,
            coordinates,
        },
    );
    Ok(())
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

struct CoordinateBuilder {
    lines: Vec<CoordinateLine>,
    checkpoints: Vec<CoordinateCheckpoint>,
    line_start: usize,
    checkpoint_start: usize,
    checkpoint_count: usize,
    last_checkpoint: usize,
    scalar_index: u32,
    coordinate_index: u32,
    mode: CoordinateMode,
}

impl CoordinateTable {
    fn new(source: &str, mode: CoordinateMode, budget: &mut ImportBudget) -> Result<Self, ProviderError> {
        let shape = coordinate_index_shape(source, mode)?;
        budget.add_coordinate_index(shape.retained_bytes)?;
        let mut builder = CoordinateBuilder::new(shape, mode)?;
        for (index, character) in source.char_indices() {
            builder.push_character(source, index, character)?;
        }
        builder.finish(source.len(), shape)
    }

    fn offset(&self, source: &str, position: ReportPosition) -> Result<usize, ProviderError> {
        validate_nonzero_position(position.line, position.column, "location")?;
        let line = self.report_line(position.line)?;
        let target = position.column - 1;
        let cursor = self.coordinate_cursor(line, target)?;
        if cursor.coordinate_index == target {
            return platform_offset(cursor.byte_offset);
        }
        self.scan_coordinate(source, line, cursor, target, position)
    }

    fn scalar_position(&self, source: &str, offset: usize) -> Result<(u32, u32), ProviderError> {
        let located = self.locate_scalar_offset(offset)?;
        let cursor = self.scalar_cursor(located.line, located.offset)?;
        display_scalar_position(source, located, cursor)
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

    fn report_line(&self, line: u32) -> Result<&CoordinateLine, ProviderError> {
        let index = usize::try_from(line - 1)
            .map_err(|_| invalid("location.line", "line does not fit this platform"))?;
        self.lines
            .get(index)
            .ok_or_else(|| invalid("location.line", format!("line {line} is outside source")))
    }

    fn coordinate_cursor(
        &self,
        line: &CoordinateLine,
        target: u32,
    ) -> Result<CoordinateCursor, ProviderError> {
        let checkpoints = self.line_checkpoints(line)?;
        let checkpoint = preceding_checkpoint(checkpoints, |entry| entry.coordinate_index <= target)?;
        Ok(match checkpoint {
            Some(entry) => CoordinateCursor {
                byte_offset: entry.byte_offset,
                coordinate_index: entry.coordinate_index,
            },
            None => CoordinateCursor {
                byte_offset: line.start,
                coordinate_index: 0,
            },
        })
    }

    fn scan_coordinate(
        &self,
        source: &str,
        line: &CoordinateLine,
        cursor: CoordinateCursor,
        target: u32,
        position: ReportPosition,
    ) -> Result<usize, ProviderError> {
        let (scan_start, line_end) = platform_range(cursor.byte_offset, line.end)?;
        let mut coordinate_index = cursor.coordinate_index;
        for (relative, character) in source[scan_start..line_end].char_indices() {
            coordinate_index = advance_coordinate(coordinate_index, character, self.mode)?;
            let byte_offset = scan_start + relative + character.len_utf8();
            if let Some(result) = matched_coordinate(coordinate_index, target, byte_offset, position) {
                return result;
            }
        }
        Err(outside_line_error(position))
    }

    fn locate_scalar_offset(&self, offset: usize) -> Result<LocatedScalar<'_>, ProviderError> {
        let offset =
            u32::try_from(offset).map_err(|_| invalid("location", "source offset does not fit in u32"))?;
        let line_index = self
            .lines
            .partition_point(|line| line.start <= offset)
            .checked_sub(1)
            .ok_or_else(|| invalid("location", "offset precedes source"))?;
        let line = &self.lines[line_index];
        if offset > line.end {
            return Err(invalid("location", "offset points into a line terminator"));
        }
        Ok(LocatedScalar {
            line_index,
            line,
            offset,
        })
    }

    fn scalar_cursor(&self, line: &CoordinateLine, offset: u32) -> Result<ScalarCursor, ProviderError> {
        let checkpoints = self.line_checkpoints(line)?;
        let checkpoint = preceding_checkpoint(checkpoints, |entry| entry.byte_offset <= offset)?;
        Ok(match checkpoint {
            Some(entry) => ScalarCursor {
                byte_offset: entry.byte_offset,
                scalar_index: entry.scalar_index,
            },
            None => ScalarCursor {
                byte_offset: line.start,
                scalar_index: 0,
            },
        })
    }
}

#[derive(Clone, Copy)]
struct CoordinateCursor {
    byte_offset: u32,
    coordinate_index: u32,
}

#[derive(Clone, Copy)]
struct ScalarCursor {
    byte_offset: u32,
    scalar_index: u32,
}

#[derive(Clone, Copy)]
struct LocatedScalar<'a> {
    line_index: usize,
    line: &'a CoordinateLine,
    offset: u32,
}

fn checkpoint_at(
    checkpoints: &[CoordinateCheckpoint],
    index: usize,
) -> Result<CoordinateCheckpoint, ProviderError> {
    checkpoints
        .get(index)
        .copied()
        .ok_or_else(|| invalid("limits.coordinateIndex", "coordinate checkpoint disappeared"))
}

fn preceding_checkpoint(
    checkpoints: &[CoordinateCheckpoint],
    predicate: impl FnMut(&CoordinateCheckpoint) -> bool,
) -> Result<Option<CoordinateCheckpoint>, ProviderError> {
    checkpoints
        .partition_point(predicate)
        .checked_sub(1)
        .map(|index| checkpoint_at(checkpoints, index))
        .transpose()
}

fn platform_offset(offset: u32) -> Result<usize, ProviderError> {
    usize::try_from(offset).map_err(|_| invalid("location", "source offset does not fit this platform"))
}

fn platform_range(start: u32, end: u32) -> Result<(usize, usize), ProviderError> {
    Ok((platform_offset(start)?, platform_offset(end)?))
}

fn advance_coordinate(current: u32, character: char, mode: CoordinateMode) -> Result<u32, ProviderError> {
    current
        .checked_add(coordinate_width(character, mode))
        .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))
}

fn matched_coordinate(
    coordinate: u32,
    target: u32,
    byte_offset: usize,
    position: ReportPosition,
) -> Option<Result<usize, ProviderError>> {
    match coordinate.cmp(&target) {
        std::cmp::Ordering::Equal => Some(Ok(byte_offset)),
        std::cmp::Ordering::Greater => Some(Err(split_surrogate_error(position))),
        std::cmp::Ordering::Less => None,
    }
}

fn split_surrogate_error(position: ReportPosition) -> ProviderError {
    column_error(format!(
        "column {} splits a UTF-16 surrogate pair on line {}",
        position.column, position.line
    ))
}

fn outside_line_error(position: ReportPosition) -> ProviderError {
    column_error(format!(
        "column {} is outside line {}",
        position.column, position.line
    ))
}

fn column_error(message: String) -> ProviderError {
    invalid("location.column", message)
}

fn display_scalar_position(
    source: &str,
    located: LocatedScalar<'_>,
    cursor: ScalarCursor,
) -> Result<(u32, u32), ProviderError> {
    let (byte_offset, offset) = platform_range(cursor.byte_offset, located.offset)?;
    let remainder = source
        .get(byte_offset..offset)
        .ok_or_else(|| invalid("location", "offset splits a Unicode scalar value"))?;
    let scalar_index = add_scalar_count(cursor.scalar_index, remainder)?;
    display_coordinates(located.line_index, scalar_index)
}

fn add_scalar_count(current: u32, source: &str) -> Result<u32, ProviderError> {
    let count = u32::try_from(source.chars().count())
        .map_err(|_| invalid("location.column", "source column does not fit in u32"))?;
    current
        .checked_add(count)
        .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))
}

fn display_coordinates(line_index: usize, scalar_index: u32) -> Result<(u32, u32), ProviderError> {
    let line = u32::try_from(line_index + 1)
        .map_err(|_| invalid("location.line", "source line does not fit in u32"))?;
    let column = scalar_index
        .checked_add(1)
        .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
    Ok((line, column))
}

impl CoordinateBuilder {
    fn new(shape: CoordinateIndexShape, mode: CoordinateMode) -> Result<Self, ProviderError> {
        let (lines, checkpoints) = reserve_coordinate_storage(shape)?;
        Ok(Self {
            lines,
            checkpoints,
            line_start: 0,
            checkpoint_start: 0,
            checkpoint_count: 0,
            last_checkpoint: 0,
            scalar_index: 0,
            coordinate_index: 0,
            mode,
        })
    }

    fn push_character(&mut self, source: &str, index: usize, character: char) -> Result<(), ProviderError> {
        if index < self.line_start {
            return Ok(());
        }
        if let Some(terminator_bytes) = line_terminator_bytes(source, index, character, self.mode) {
            return self.push_terminator(index, terminator_bytes);
        }
        self.push_source_character(index, character)
    }

    fn push_terminator(&mut self, index: usize, terminator_bytes: usize) -> Result<(), ProviderError> {
        self.finish_line(index)?;
        self.start_line(index + terminator_bytes);
        Ok(())
    }

    fn push_source_character(&mut self, index: usize, character: char) -> Result<(), ProviderError> {
        self.advance_indices(character)?;
        let after = index + character.len_utf8();
        if after - self.last_checkpoint >= COORDINATE_CHECKPOINT_STRIDE_BYTES {
            self.push_checkpoint(after)?;
        }
        Ok(())
    }

    fn advance_indices(&mut self, character: char) -> Result<(), ProviderError> {
        self.scalar_index = self
            .scalar_index
            .checked_add(1)
            .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
        self.coordinate_index = self
            .coordinate_index
            .checked_add(coordinate_width(character, self.mode))
            .ok_or_else(|| invalid("location.column", "source column does not fit in u32"))?;
        Ok(())
    }

    fn push_checkpoint(&mut self, after: usize) -> Result<(), ProviderError> {
        self.checkpoints.push(CoordinateCheckpoint {
            byte_offset: u32::try_from(after)
                .map_err(|_| invalid("limits.sourceBytes", "source offset does not fit in u32"))?,
            scalar_index: self.scalar_index,
            coordinate_index: self.coordinate_index,
        });
        self.checkpoint_count = self
            .checkpoint_count
            .checked_add(1)
            .ok_or_else(|| invalid("limits.coordinateIndex", "too many coordinate checkpoints"))?;
        self.last_checkpoint = after;
        Ok(())
    }

    fn finish_line(&mut self, end: usize) -> Result<(), ProviderError> {
        push_coordinate_line(
            &mut self.lines,
            self.line_start,
            end,
            self.checkpoint_start,
            self.checkpoint_count,
        )
    }

    fn start_line(&mut self, start: usize) {
        self.line_start = start;
        self.checkpoint_start = self.checkpoints.len();
        self.checkpoint_count = 0;
        self.last_checkpoint = start;
        self.scalar_index = 0;
        self.coordinate_index = 0;
    }

    fn finish(
        mut self,
        source_len: usize,
        shape: CoordinateIndexShape,
    ) -> Result<CoordinateTable, ProviderError> {
        self.finish_line(source_len)?;
        debug_assert_eq!(self.lines.len(), shape.lines);
        debug_assert!(self.checkpoints.len() <= shape.checkpoints);
        Ok(CoordinateTable {
            lines: self.lines,
            checkpoints: self.checkpoints,
            mode: self.mode,
        })
    }
}

fn reserve_coordinate_storage(
    shape: CoordinateIndexShape,
) -> Result<(Vec<CoordinateLine>, Vec<CoordinateCheckpoint>), ProviderError> {
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
    Ok((lines, checkpoints))
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

fn import_mte(
    root: &Path,
    provider: MutationProvider,
    value: Value,
) -> Result<ImportedMutationReport, ProviderError> {
    let document: MteReport = serde_json::from_value(value)?;
    let mte_v2 = validate_mte_header(&document, provider)?;
    let coordinate_mode = mte_coordinate_mode(&document, provider);
    let mut context = MteImportContext::new(provider, coordinate_mode);
    for (file, entry) in document.files {
        context.import_file(root, &file, entry)?;
    }
    sort_results(&mut context.results);
    Ok(imported_report(
        provider,
        mte_import_format(mte_v2),
        document
            .framework
            .as_ref()
            .map(|framework| framework.name.clone()),
        document.framework.and_then(|framework| framework.version),
        context.results,
        Vec::new(),
    ))
}

fn imported_report(
    provider: MutationProvider,
    format: ImportFormat,
    framework_name: Option<String>,
    framework_version: Option<String>,
    results: Vec<ImportedMutation>,
    warnings: Vec<String>,
) -> ImportedMutationReport {
    ImportedMutationReport {
        provider,
        format,
        framework_name,
        framework_version,
        results,
        warnings,
    }
}

struct MteImportContext {
    provider: MutationProvider,
    coordinate_mode: CoordinateMode,
    results: Vec<ImportedMutation>,
    used_ids: BTreeSet<u64>,
    budget: ImportBudget,
    registry: CandidateRegistry,
    normalized_files: BTreeSet<String>,
}

impl MteImportContext {
    fn new(provider: MutationProvider, coordinate_mode: CoordinateMode) -> Self {
        Self {
            provider,
            coordinate_mode,
            results: Vec::new(),
            used_ids: BTreeSet::new(),
            budget: ImportBudget::default(),
            registry: CandidateRegistry::default(),
            normalized_files: BTreeSet::new(),
        }
    }

    fn import_file(&mut self, root: &Path, reported_file: &str, entry: MteFile) -> Result<(), ProviderError> {
        let MteFile {
            language: reported_language,
            source,
            mutants,
        } = entry;
        self.budget.add_source_file(source.len())?;
        let file = self.register_file(root, reported_file)?;
        let language = report_language(&reported_language)?;
        let coordinates = CoordinateTable::new(&source, self.coordinate_mode, &mut self.budget)?;
        self.import_mutants(&file, language, &source, &coordinates, mutants)
    }

    fn import_mutants(
        &mut self,
        file: &str,
        language: Language,
        source: &str,
        coordinates: &CoordinateTable,
        mutants: Vec<MteMutant>,
    ) -> Result<(), ProviderError> {
        for mutant in mutants {
            self.import_mutant(file, language, source, coordinates, mutant)?;
        }
        Ok(())
    }

    fn register_file(&mut self, root: &Path, reported: &str) -> Result<String, ProviderError> {
        let file = normalized_relative(root, reported)?;
        if self.normalized_files.insert(file.clone()) {
            Ok(file)
        } else {
            Err(invalid(
                "files",
                format!("duplicate normalized source file {file:?}"),
            ))
        }
    }

    fn import_mutant(
        &mut self,
        file: &str,
        language: Language,
        source: &str,
        coordinates: &CoordinateTable,
        mutant: MteMutant,
    ) -> Result<(), ProviderError> {
        validate_mte_mutator_name(&mutant.mutator_name)?;
        let prepared = prepare_mte_mutant(source, coordinates, &mutant)?;
        self.register_mutant(file, &mutant, &prepared.span)?;
        let id = reserve_stable_id(&mut self.used_ids, self.provider, file, &mutant.id);
        self.results
            .push(build_mte_mutation(file, language, id, mutant, prepared));
        Ok(())
    }

    fn register_mutant(
        &mut self,
        file: &str,
        mutant: &MteMutant,
        span: &std::ops::Range<usize>,
    ) -> Result<(), ProviderError> {
        self.budget.add_mutant(span.len(), mutant.replacement.len())?;
        self.registry
            .add_external_id(&mutant.id, "files.*.mutants[].id")?;
        self.registry
            .add_effective(file, span, &mutant.replacement, "files.*.mutants[]")
    }
}

struct PreparedMteMutant {
    span: std::ops::Range<usize>,
    original: String,
    line: u32,
    column: u32,
    status: MutationStatus,
    duration_seconds: f64,
}

fn prepare_mte_mutant(
    source: &str,
    coordinates: &CoordinateTable,
    mutant: &MteMutant,
) -> Result<PreparedMteMutant, ProviderError> {
    let status = mte_status(&mutant.status)?;
    let span = source_span(source, coordinates, mutant.location)?;
    let (line, column) = coordinates.scalar_position(source, span.start)?;
    let duration_seconds = mte_duration(mutant.duration)?;
    let original = source[span.clone()].to_owned();
    Ok(PreparedMteMutant {
        span,
        original,
        line,
        column,
        status,
        duration_seconds,
    })
}

fn validate_mte_mutator_name(name: &str) -> Result<(), ProviderError> {
    if name.trim().is_empty() {
        Err(invalid(
            "files.*.mutants[].mutatorName",
            "expected a non-empty string",
        ))
    } else {
        Ok(())
    }
}

fn mte_duration(duration: Option<f64>) -> Result<f64, ProviderError> {
    match duration {
        Some(duration) => validated_mte_duration(duration),
        None => Ok(0.0),
    }
}

fn validated_mte_duration(duration: f64) -> Result<f64, ProviderError> {
    if valid_mte_duration(duration) {
        Ok(duration / 1_000.0)
    } else {
        Err(invalid(
            "files.*.mutants[].duration",
            "expected a finite non-negative millisecond duration",
        ))
    }
}

fn valid_mte_duration(duration: f64) -> bool {
    duration.is_finite() && duration >= 0.0
}

fn build_mte_mutation(
    file: &str,
    language: Language,
    id: u64,
    mutant: MteMutant,
    prepared: PreparedMteMutant,
) -> ImportedMutation {
    let detail = mte_detail(mutant.status_reason, mutant.mutator_name, mutant.description);
    imported_mutation(
        ImportedMutationFields {
            external_id: mutant.id,
            id,
            language,
            file: file.to_owned(),
            operator: String::new(),
            line: prepared.line,
            column: prepared.column,
            original: prepared.original,
            replacement: mutant.replacement,
            span: prepared.span,
        },
        prepared.status,
        prepared.duration_seconds,
        detail,
    )
}

struct ImportedMutationFields {
    external_id: String,
    id: u64,
    language: Language,
    file: String,
    operator: String,
    line: u32,
    column: u32,
    original: String,
    replacement: String,
    span: std::ops::Range<usize>,
}

fn imported_mutation(
    fields: ImportedMutationFields,
    status: MutationStatus,
    duration_seconds: f64,
    detail: Option<String>,
) -> ImportedMutation {
    ImportedMutation {
        external_id: fields.external_id,
        result: MutationResult {
            mutation: MutationCandidate {
                id: fields.id,
                language: fields.language,
                file: fields.file,
                stable_symbol: String::new(),
                operator: fields.operator,
                fingerprint: String::new(),
                line: fields.line,
                column: fields.column,
                original: fields.original,
                replacement: fields.replacement,
                start_byte: fields.span.start,
                end_byte: fields.span.end,
            },
            status,
            exit_code: None,
            duration_seconds,
            detail,
        },
    }
}

fn mte_detail(reason: Option<String>, mutator: String, description: Option<String>) -> Option<String> {
    reason.or_else(|| match description {
        Some(description) if !description.is_empty() => Some(format!("{mutator}: {description}")),
        _ => Some(mutator),
    })
}

fn mte_coordinate_mode(document: &MteReport, provider: MutationProvider) -> CoordinateMode {
    let stryker_framework = document
        .framework
        .as_ref()
        .is_some_and(|framework| framework.name.eq_ignore_ascii_case("StrykerJS"));
    if stryker_framework || (document.framework.is_none() && provider == MutationProvider::Stryker) {
        CoordinateMode::JavascriptUtf16
    } else {
        CoordinateMode::UnicodeScalar
    }
}

const fn mte_import_format(v2: bool) -> ImportFormat {
    if v2 {
        ImportFormat::MutationTestingElementsV2
    } else {
        ImportFormat::MutationTestingElementsV1
    }
}

fn validate_mte_header(document: &MteReport, provider: MutationProvider) -> Result<bool, ProviderError> {
    let schema_major = validate_mte_schema(&document.schema_version, provider)?;
    validate_mte_thresholds(&document.thresholds)?;
    validate_mte_framework(document.framework.as_ref())?;
    Ok(schema_major == 2)
}

fn validate_mte_schema(version: &str, provider: MutationProvider) -> Result<u8, ProviderError> {
    let Some(schema_major) = mte_schema_major(version) else {
        return Err(invalid(
            "schemaVersion",
            format!("invalid Mutation Testing Elements version {version:?}"),
        ));
    };
    if schema_major != 2 && !(provider == MutationProvider::Mull && schema_major == 1) {
        return Err(invalid("schemaVersion", format!("expected 2.x, found {version}")));
    }
    Ok(schema_major)
}

fn validate_mte_thresholds(thresholds: &MteThresholds) -> Result<(), ProviderError> {
    if thresholds.high > 100 || thresholds.low > 100 || thresholds.low > thresholds.high {
        return Err(invalid(
            "thresholds",
            "expected integer thresholds satisfying 0 <= low <= high <= 100",
        ));
    }
    Ok(())
}

fn validate_mte_framework(framework: Option<&MteFramework>) -> Result<(), ProviderError> {
    if framework.is_some_and(|framework| framework.name.trim().is_empty()) {
        return Err(invalid("framework.name", "expected a non-empty string"));
    }
    Ok(())
}

fn import_cargo_mutants(root: &Path, value: &Value) -> Result<ImportedMutationReport, ProviderError> {
    let version = required_string(value, "cargo_mutants_version", "report")?.to_owned();
    required_string(value, "end_time", "report")?;
    let outcomes = required_array(value, "outcomes", "outcomes")?;
    let mut context = MutationImportContext::default();
    for (index, outcome) in outcomes.iter().enumerate() {
        context.import_outcome(root, index, outcome)?;
    }
    sort_results(&mut context.results);
    Ok(imported_report(
        MutationProvider::CargoMutants,
        ImportFormat::CargoMutantsOutcomes,
        Some("cargo-mutants".to_owned()),
        Some(version),
        context.results,
        vec![
            "cargo-mutants documents outcomes.json as subject to change; this importer validates the recognized fields and rejects incompatible shapes".to_owned(),
        ],
    ))
}

#[derive(Default)]
struct MutationImportContext {
    sources: BTreeMap<String, CachedSource>,
    results: Vec<ImportedMutation>,
    used_ids: BTreeSet<u64>,
    budget: ImportBudget,
    registry: CandidateRegistry,
}

impl MutationImportContext {
    fn import_outcome(&mut self, root: &Path, index: usize, outcome: &Value) -> Result<(), ProviderError> {
        let field = format!("outcomes[{index}]");
        let Some(mutant) = cargo_mutant_scenario(outcome, &field)? else {
            return Ok(());
        };
        self.import_mutant(root, mutant, outcome, &field)
    }

    fn import_mutant(
        &mut self,
        root: &Path,
        mutant: &Value,
        outcome: &Value,
        field: &str,
    ) -> Result<(), ProviderError> {
        let file = cargo_mutant_file(root, mutant, field)?;
        let source = self.cached_source(root, &file, &format!("{field}.scenario.Mutant.file"), field)?;
        let prepared = prepare_cargo_mutant(mutant, outcome, field, source)?;
        self.register_candidate(CandidateRegistration {
            file: &file,
            span: &prepared.span,
            replacement: &prepared.replacement,
            external_id: &prepared.external_id,
            effective_field: field,
            external_field: &format!("{field}.scenario.Mutant.name"),
        })?;
        let id = reserve_stable_id(
            &mut self.used_ids,
            MutationProvider::CargoMutants,
            &file,
            &prepared.external_id,
        );
        self.results.push(build_cargo_mutation(file, id, prepared));
        Ok(())
    }

    fn cached_source(
        &mut self,
        root: &Path,
        file: &str,
        source_field: &str,
        field: &str,
    ) -> Result<&CachedSource, ProviderError> {
        ensure_cached_source(&mut self.sources, &mut self.budget, root, file, source_field)?;
        self.sources
            .get(file)
            .ok_or_else(|| invalid(field, "source cache entry disappeared"))
    }

    fn register_candidate(&mut self, registration: CandidateRegistration<'_>) -> Result<(), ProviderError> {
        self.budget
            .add_mutant(registration.span.len(), registration.replacement.len())?;
        self.registry.add_effective(
            registration.file,
            registration.span,
            registration.replacement,
            registration.effective_field,
        )?;
        self.registry
            .add_external_id(registration.external_id, registration.external_field)
    }
}

#[derive(Clone, Copy)]
struct CandidateRegistration<'a> {
    file: &'a str,
    span: &'a std::ops::Range<usize>,
    replacement: &'a str,
    external_id: &'a str,
    effective_field: &'a str,
    external_field: &'a str,
}

struct PreparedCargoMutant {
    external_id: String,
    replacement: String,
    original: String,
    span: std::ops::Range<usize>,
    location: ReportLocation,
    status: MutationStatus,
    duration_seconds: f64,
    detail: Option<String>,
}

fn cargo_mutant_scenario<'a>(outcome: &'a Value, field: &str) -> Result<Option<&'a Value>, ProviderError> {
    let scenario = outcome
        .get("scenario")
        .ok_or_else(|| invalid(format!("{field}.scenario"), "missing scenario"))?;
    if scenario.as_str() == Some("Baseline") {
        required_string(outcome, "summary", field)?;
        return Ok(None);
    }
    scenario
        .get("Mutant")
        .filter(|mutant| mutant.is_object())
        .map(Some)
        .ok_or_else(|| {
            invalid(
                format!("{field}.scenario"),
                "expected \"Baseline\" or an object containing Mutant",
            )
        })
}

fn cargo_mutant_file(root: &Path, mutant: &Value, field: &str) -> Result<String, ProviderError> {
    let file = required_string(mutant, "file", field)?;
    normalized_relative(root, file)
}

fn prepare_cargo_mutant(
    mutant: &Value,
    outcome: &Value,
    field: &str,
    source: &CachedSource,
) -> Result<PreparedCargoMutant, ProviderError> {
    let (external_id, replacement, location, span) = cargo_mutant_identity(mutant, field, source)?;
    let status = cargo_mutants_status(required_string(outcome, "summary", field)?)?;
    Ok(PreparedCargoMutant {
        detail: mutant.get("name").and_then(Value::as_str).map(str::to_owned),
        original: source.text[span.clone()].to_owned(),
        duration_seconds: cargo_phase_duration(outcome),
        external_id,
        replacement,
        location,
        span,
        status,
    })
}

fn cargo_mutant_identity(
    mutant: &Value,
    field: &str,
    source: &CachedSource,
) -> Result<(String, String, ReportLocation, std::ops::Range<usize>), ProviderError> {
    let (location, span) = cargo_mutant_span(mutant, field, source)?;
    let external_id = required_string(mutant, "name", &format!("{field}.scenario.Mutant"))?.to_owned();
    validate_nonempty(&external_id, format!("{field}.scenario.Mutant.name"))?;
    let replacement = required_string(mutant, "replacement", &format!("{field}.scenario.Mutant"))?.to_owned();
    Ok((external_id, replacement, location, span))
}

fn cargo_mutant_span(
    mutant: &Value,
    field: &str,
    source: &CachedSource,
) -> Result<(ReportLocation, std::ops::Range<usize>), ProviderError> {
    let location: ReportLocation = serde_json::from_value(
        mutant
            .get("span")
            .cloned()
            .ok_or_else(|| invalid(format!("{field}.scenario.Mutant.span"), "missing span"))?,
    )?;
    let span = source_span(&source.text, &source.coordinates, location)?;
    Ok((location, span))
}

fn cargo_phase_duration(outcome: &Value) -> f64 {
    outcome
        .get("phase_results")
        .and_then(Value::as_array)
        .map_or(0.0, |phases| {
            phases
                .iter()
                .filter_map(|phase| phase.get("duration").and_then(Value::as_f64))
                .filter(|duration| duration.is_finite() && *duration >= 0.0)
                .sum()
        })
}

fn build_cargo_mutation(file: String, id: u64, prepared: PreparedCargoMutant) -> ImportedMutation {
    imported_mutation(
        ImportedMutationFields {
            external_id: prepared.external_id,
            id,
            language: Language::Rust,
            file,
            operator: String::new(),
            line: prepared.location.start.line,
            column: prepared.location.start.column,
            original: prepared.original,
            replacement: prepared.replacement,
            span: prepared.span,
        },
        prepared.status,
        prepared.duration_seconds,
        prepared.detail,
    )
}

fn import_muter(root: &Path, value: &Value) -> Result<ImportedMutationReport, ProviderError> {
    let file_reports = required_array(value, "fileReports", "fileReports")?;
    validate_muter_file_report_count(file_reports.len())?;
    let requested_basenames = requested_muter_basenames(file_reports)?;
    let basename_index = build_muter_basename_index(root, &requested_basenames)?;
    let mut context = MutationImportContext::default();
    context.import_files(root, &basename_index, file_reports)?;
    sort_results(&mut context.results);
    Ok(imported_report(
        MutationProvider::Muter,
        ImportFormat::MuterJson,
        Some("Muter".to_owned()),
        None,
        context.results,
        vec![
            "Muter JSON is unversioned and omits source paths; reporigor accepts it only when every file basename resolves uniquely under the project root".to_owned(),
        ],
    ))
}

impl MutationImportContext {
    fn import_files(
        &mut self,
        root: &Path,
        basename_index: &BTreeMap<String, Vec<String>>,
        file_reports: &[Value],
    ) -> Result<(), ProviderError> {
        for (file_index, file_report) in file_reports.iter().enumerate() {
            self.import_file(root, basename_index, file_index, file_report)?;
        }
        Ok(())
    }

    fn import_file(
        &mut self,
        root: &Path,
        basename_index: &BTreeMap<String, Vec<String>>,
        file_index: usize,
        file_report: &Value,
    ) -> Result<(), ProviderError> {
        let field = format!("fileReports[{file_index}]");
        let file = resolve_muter_file(basename_index, file_report, &field)?;
        self.cached_source(root, &file, &format!("{field}.fileName"), &field)?;
        let operators = muter_operators(file_report, &field)?;
        self.import_operators(&file, &field, operators)
    }

    fn import_operators(
        &mut self,
        file: &str,
        field: &str,
        operators: &[Value],
    ) -> Result<(), ProviderError> {
        for (operator_index, operator) in operators.iter().enumerate() {
            self.import_operator(file, field, operator_index, operator)?;
        }
        Ok(())
    }

    fn import_operator(
        &mut self,
        file: &str,
        field: &str,
        operator_index: usize,
        operator: &Value,
    ) -> Result<(), ProviderError> {
        let operator_field = format!("{field}.appliedOperators[{operator_index}]");
        let prepared = {
            let source = self
                .sources
                .get(file)
                .ok_or_else(|| invalid(field, "source cache entry disappeared"))?;
            prepare_muter_operator(source, operator, &operator_field, file, operator_index)?
        };
        let Some(prepared) = prepared else {
            return Ok(());
        };
        self.register_candidate(CandidateRegistration {
            file,
            span: &prepared.span,
            replacement: &prepared.replacement,
            external_id: &prepared.external_id,
            effective_field: &operator_field,
            external_field: &format!("{operator_field}.externalId"),
        })?;
        let id = reserve_stable_id(
            &mut self.used_ids,
            MutationProvider::Muter,
            file,
            &prepared.external_id,
        );
        self.results.push(build_muter_mutation(file, id, prepared));
        Ok(())
    }
}

struct PreparedMuterMutation {
    external_id: String,
    operator_name: String,
    original: String,
    replacement: String,
    detail: String,
    position: ValidatedMuterPosition,
    span: std::ops::Range<usize>,
    status: MutationStatus,
}

struct RawMuterMutation {
    operator_name: String,
    original: String,
    replacement: String,
    description: String,
    position: ValidatedMuterPosition,
}

fn validate_muter_file_report_count(count: usize) -> Result<(), ProviderError> {
    if count <= MAX_MUTER_FILE_REPORTS {
        Ok(())
    } else {
        Err(limit_error("fileReports", count, MAX_MUTER_FILE_REPORTS))
    }
}

fn requested_muter_basenames(file_reports: &[Value]) -> Result<BTreeSet<String>, ProviderError> {
    let mut requested = BTreeSet::new();
    for (index, report) in file_reports.iter().enumerate() {
        let field = format!("fileReports[{index}].fileName");
        let file_name = required_string(report, "fileName", &format!("fileReports[{index}]"))?;
        validate_muter_basename(file_name, &field)?;
        requested.insert(file_name.to_owned());
    }
    Ok(requested)
}

fn resolve_muter_file(
    index: &BTreeMap<String, Vec<String>>,
    report: &Value,
    field: &str,
) -> Result<String, ProviderError> {
    let file_name = required_string(report, "fileName", field)?;
    resolve_indexed_basename(index, file_name, &format!("{field}.fileName"))
}

fn muter_operators<'a>(report: &'a Value, field: &str) -> Result<&'a [Value], ProviderError> {
    required_array(report, "appliedOperators", &format!("{field}.appliedOperators"))
}

fn prepare_muter_operator(
    source: &CachedSource,
    operator: &Value,
    field: &str,
    file: &str,
    operator_index: usize,
) -> Result<Option<PreparedMuterMutation>, ProviderError> {
    let Some(status) = muter_operator_status(operator, field)? else {
        return Ok(None);
    };
    let raw = raw_muter_mutation(source, operator, field)?;
    let span = validated_muter_snapshot_span(&source.text, raw.position.utf8_offset, &raw.original, field)?;
    let external_id = format!(
        "{file}:{}:{}:{}:{}",
        raw.position.line,
        raw.position.utf8_column,
        raw.operator_name,
        operator_index.saturating_add(1)
    );
    let detail = muter_detail(&raw.operator_name, raw.description);
    Ok(Some(PreparedMuterMutation {
        external_id,
        operator_name: raw.operator_name,
        original: raw.original,
        replacement: raw.replacement,
        detail,
        position: raw.position,
        span,
        status,
    }))
}

fn muter_detail(operator_name: &str, description: String) -> String {
    if description.is_empty() {
        operator_name.to_owned()
    } else {
        description
    }
}

fn muter_operator_status(operator: &Value, field: &str) -> Result<Option<MutationStatus>, ProviderError> {
    let value = required_string(operator, "testSuiteOutcome", field)?;
    if value != "noCoverage" {
        return muter_status(value).map(Some);
    }
    if valid_no_coverage_sentinel(operator) {
        Ok(None)
    } else {
        Err(invalid(
            format!("{field}.testSuiteOutcome"),
            "noCoverage is only valid for Muter's null file-level sentinel",
        ))
    }
}

fn valid_no_coverage_sentinel(operator: &Value) -> bool {
    operator.get("mutationPoint").is_some_and(Value::is_null)
        && operator.get("mutationSnapshot").is_some_and(Value::is_null)
}

fn raw_muter_mutation(
    source: &CachedSource,
    operator: &Value,
    field: &str,
) -> Result<RawMuterMutation, ProviderError> {
    let (operator_name, position) = muter_point_details(source, operator, field)?;
    let (original, replacement, description) = muter_snapshot_details(operator, field)?;
    Ok(RawMuterMutation {
        operator_name,
        original,
        replacement,
        description,
        position,
    })
}

fn muter_point_details(
    source: &CachedSource,
    operator: &Value,
    field: &str,
) -> Result<(String, ValidatedMuterPosition), ProviderError> {
    let point = required_object(operator, "mutationPoint", field)?;
    let operator_name = muter_operator_name(point, field)?;
    let position = muter_operator_position(source, point, field)?;
    Ok((operator_name, position))
}

fn muter_operator_name(point: &Value, field: &str) -> Result<String, ProviderError> {
    let operator_name = required_string(point, "mutationOperatorId", field)?.to_owned();
    validate_nonempty(
        &operator_name,
        format!("{field}.mutationPoint.mutationOperatorId"),
    )?;
    Ok(operator_name)
}

fn muter_operator_position(
    source: &CachedSource,
    point: &Value,
    field: &str,
) -> Result<ValidatedMuterPosition, ProviderError> {
    let position = required_object(point, "position", &format!("{field}.mutationPoint"))?;
    validated_muter_position(
        &source.text,
        &source.coordinates,
        position,
        &format!("{field}.mutationPoint.position"),
    )
}

fn muter_snapshot_details(operator: &Value, field: &str) -> Result<(String, String, String), ProviderError> {
    let snapshot = required_object(operator, "mutationSnapshot", field)?;
    let snapshot_field = format!("{field}.mutationSnapshot");
    Ok((
        required_string(snapshot, "before", &snapshot_field)?.to_owned(),
        required_string(snapshot, "after", &snapshot_field)?.to_owned(),
        required_string(snapshot, "description", &snapshot_field)?.to_owned(),
    ))
}

fn required_object<'a>(value: &'a Value, name: &str, field: &str) -> Result<&'a Value, ProviderError> {
    value
        .get(name)
        .filter(|member| member.is_object())
        .ok_or_else(|| invalid(format!("{field}.{name}"), "expected an object"))
}

fn validate_nonempty(name: &str, field: String) -> Result<(), ProviderError> {
    if name.trim().is_empty() {
        Err(invalid(field, "expected a non-empty string"))
    } else {
        Ok(())
    }
}

fn validated_muter_snapshot_span(
    source: &str,
    start: usize,
    original: &str,
    field: &str,
) -> Result<std::ops::Range<usize>, ProviderError> {
    if original.is_empty() {
        return Ok(start..start);
    }
    let end = start.checked_add(original.len()).ok_or_else(|| {
        invalid(
            format!("{field}.mutationSnapshot.before"),
            "snapshot length overflows the source offset",
        )
    })?;
    if source.get(start..end) == Some(original) {
        Ok(start..end)
    } else {
        Err(invalid(
            format!("{field}.mutationSnapshot.before"),
            "snapshot text does not match source at the reported position",
        ))
    }
}

fn build_muter_mutation(file: &str, id: u64, prepared: PreparedMuterMutation) -> ImportedMutation {
    imported_mutation(
        ImportedMutationFields {
            external_id: prepared.external_id,
            id,
            language: Language::Swift,
            file: file.to_owned(),
            operator: prepared.operator_name,
            line: prepared.position.line,
            column: prepared.position.scalar_column,
            original: prepared.original,
            replacement: prepared.replacement,
            span: prepared.span,
        },
        prepared.status,
        0.0,
        Some(prepared.detail),
    )
}

fn muter_status(value: &str) -> Result<MutationStatus, ProviderError> {
    imported_status(StatusDialect::Muter, value)
}

#[derive(Debug, Clone, Copy)]
struct ValidatedMuterPosition {
    utf8_offset: usize,
    line: u32,
    utf8_column: u32,
    scalar_column: u32,
}

#[derive(Debug, Clone, Copy)]
struct ReportedMuterPosition {
    line: u32,
    utf8_column: u32,
    utf8_offset: usize,
}

fn validated_muter_position(
    source: &str,
    coordinates: &CoordinateTable,
    position: &Value,
    field: &str,
) -> Result<ValidatedMuterPosition, ProviderError> {
    let reported = reported_muter_position(position, field)?;
    validate_muter_source_position(source, reported, field)?;
    let (actual_line, actual_utf8_column, scalar_column) =
        coordinates.utf8_position(source, reported.utf8_offset)?;
    validate_reported_position(
        reported.line,
        reported.utf8_column,
        reported.utf8_offset,
        actual_line,
        actual_utf8_column,
        field,
    )?;
    Ok(ValidatedMuterPosition {
        utf8_offset: reported.utf8_offset,
        line: reported.line,
        utf8_column: reported.utf8_column,
        scalar_column,
    })
}

fn reported_muter_position(position: &Value, field: &str) -> Result<ReportedMuterPosition, ProviderError> {
    Ok(ReportedMuterPosition {
        line: required_u32(position, "line", field)?,
        utf8_column: required_u32(position, "column", field)?,
        utf8_offset: required_usize(position, "utf8Offset", field)?,
    })
}

fn validate_muter_source_position(
    source: &str,
    position: ReportedMuterPosition,
    field: &str,
) -> Result<(), ProviderError> {
    validate_nonzero_position(position.line, position.utf8_column, field)?;
    validate_utf8_offset(source, position.utf8_offset, field)
}

fn validate_nonzero_position(line: u32, column: u32, field: &str) -> Result<(), ProviderError> {
    if line > 0 && column > 0 {
        Ok(())
    } else {
        Err(invalid(field, "line and column must both be at least 1"))
    }
}

fn validate_utf8_offset(source: &str, offset: usize, field: &str) -> Result<(), ProviderError> {
    if offset <= source.len() && source.is_char_boundary(offset) {
        Ok(())
    } else {
        Err(invalid(
            format!("{field}.utf8Offset"),
            "offset is outside the UTF-8 source or splits a Unicode scalar value",
        ))
    }
}

fn validate_reported_position(
    line: u32,
    column: u32,
    offset: usize,
    actual_line: u32,
    actual_column: u32,
    field: &str,
) -> Result<(), ProviderError> {
    if line == actual_line && column == actual_column {
        Ok(())
    } else {
        Err(invalid(
            field,
            format!(
                "line/column {line}:{column} does not match utf8Offset {offset} ({actual_line}:{actual_column})"
            ),
        ))
    }
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
    let mut traversal = MuterTraversal {
        root,
        requested,
        index: &mut index,
        traversed_entries: &mut traversed_entries,
    };
    traversal.collect(root, 0)?;
    Ok(index)
}

struct MuterTraversal<'a> {
    root: &'a Path,
    requested: &'a BTreeSet<String>,
    index: &'a mut BTreeMap<String, Vec<String>>,
    traversed_entries: &'a mut usize,
}

impl MuterTraversal<'_> {
    fn collect(&mut self, directory: &Path, depth: usize) -> Result<(), ProviderError> {
        Self::validate_depth(depth)?;
        let entries = fs::read_dir(directory).map_err(|source| ProviderError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        self.collect_entries(directory, depth, entries)
    }

    fn collect_entries(
        &mut self,
        directory: &Path,
        depth: usize,
        entries: fs::ReadDir,
    ) -> Result<(), ProviderError> {
        for entry in entries {
            let entry = entry.map_err(|source| ProviderError::Io {
                path: directory.to_path_buf(),
                source,
            })?;
            self.increment()?;
            self.collect_entry(depth, &entry)?;
        }
        Ok(())
    }

    fn validate_depth(depth: usize) -> Result<(), ProviderError> {
        if depth <= MAX_MUTER_TRAVERSAL_DEPTH {
            Ok(())
        } else {
            Err(limit_error(
                "muterTraversalDepth",
                depth,
                MAX_MUTER_TRAVERSAL_DEPTH,
            ))
        }
    }

    fn increment(&mut self) -> Result<(), ProviderError> {
        *self.traversed_entries = self
            .traversed_entries
            .checked_add(1)
            .ok_or_else(|| limit_error("muterTraversalEntries", usize::MAX, MAX_MUTER_TRAVERSAL_ENTRIES))?;
        if *self.traversed_entries <= MAX_MUTER_TRAVERSAL_ENTRIES {
            Ok(())
        } else {
            Err(limit_error(
                "muterTraversalEntries",
                *self.traversed_entries,
                MAX_MUTER_TRAVERSAL_ENTRIES,
            ))
        }
    }

    fn collect_entry(&mut self, depth: usize, entry: &fs::DirEntry) -> Result<(), ProviderError> {
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| ProviderError::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            return Ok(());
        }
        self.collect_regular_entry(depth, entry, &path, file_type)
    }

    fn collect_regular_entry(
        &mut self,
        depth: usize,
        entry: &fs::DirEntry,
        path: &Path,
        file_type: fs::FileType,
    ) -> Result<(), ProviderError> {
        if file_type.is_dir() {
            return self.collect_directory(depth, entry, path);
        }
        if file_type.is_file() {
            self.collect_file(entry, path)?;
        }
        Ok(())
    }

    fn collect_directory(
        &mut self,
        depth: usize,
        entry: &fs::DirEntry,
        path: &Path,
    ) -> Result<(), ProviderError> {
        if excluded_muter_directory(entry) {
            return Ok(());
        }
        self.collect(path, depth.saturating_add(1))
    }

    fn collect_file(&mut self, entry: &fs::DirEntry, path: &Path) -> Result<(), ProviderError> {
        let Some(file_name) = self.requested_file_name(entry) else {
            return Ok(());
        };
        self.record_file(file_name, path)
    }

    fn requested_file_name(&self, entry: &fs::DirEntry) -> Option<String> {
        let file_name = entry.file_name();
        file_name
            .to_str()
            .filter(|file_name| self.requested.contains(*file_name))
            .map(str::to_owned)
    }

    fn record_file(&mut self, file_name: String, path: &Path) -> Result<(), ProviderError> {
        let relative = path
            .strip_prefix(self.root)
            .map_err(|_| invalid("fileReports[].fileName", "resolved file escaped project root"))?;
        let normalized = normalized_relative(self.root, &relative.to_string_lossy())?;
        let matches = self.index.entry(file_name).or_default();
        // One path is resolvable and two are enough to prove ambiguity.
        if matches.len() < 2 {
            matches.push(normalized);
        }
        Ok(())
    }
}

fn excluded_muter_directory(entry: &fs::DirEntry) -> bool {
    matches!(
        entry.file_name().to_str(),
        Some(".git" | ".build" | "build" | "target" | "node_modules" | ".venv" | "venv")
    )
}

fn required_string<'a>(object: &'a Value, name: &str, parent: &str) -> Result<&'a str, ProviderError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("{parent}.{name}"), "expected a string"))
}

fn required_array<'a>(object: &'a Value, name: &str, field: &str) -> Result<&'a [Value], ProviderError> {
    object
        .get(name)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(field, "expected an array"))
}

fn required_u32(object: &Value, name: &str, parent: &str) -> Result<u32, ProviderError> {
    let value = required_u64(object, name, parent)?;
    u32::try_from(value).map_err(|_| invalid(format!("{parent}.{name}"), "value does not fit in u32"))
}

fn required_usize(object: &Value, name: &str, parent: &str) -> Result<usize, ProviderError> {
    let value = required_u64(object, name, parent)?;
    usize::try_from(value)
        .map_err(|_| invalid(format!("{parent}.{name}"), "value does not fit on this platform"))
}

fn required_u64(object: &Value, name: &str, parent: &str) -> Result<u64, ProviderError> {
    let value = object
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(format!("{parent}.{name}"), "expected an unsigned integer"))?;
    Ok(value)
}

fn report_language(value: &str) -> Result<Language, ProviderError> {
    value
        .parse()
        .map_err(|_| invalid("files.*.language", format!("unsupported {value:?}")))
}

fn mte_status(value: &str) -> Result<MutationStatus, ProviderError> {
    imported_status(StatusDialect::Mte, value)
}

fn cargo_mutants_status(value: &str) -> Result<MutationStatus, ProviderError> {
    imported_status(StatusDialect::CargoMutants, value)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusDialect {
    Mte,
    CargoMutants,
    Muter,
}

fn imported_status(dialect: StatusDialect, value: &str) -> Result<MutationStatus, ProviderError> {
    use MutationStatus::{
        CompileError, Ignored, Killed, NoCoverage, Pending, RuntimeError, Survived, Timeout,
    };

    const STATUSES: &[(StatusDialect, &str, MutationStatus)] = &[
        (StatusDialect::Mte, "Killed", Killed),
        (StatusDialect::Mte, "Survived", Survived),
        (StatusDialect::Mte, "NoCoverage", NoCoverage),
        (StatusDialect::Mte, "CompileError", CompileError),
        (StatusDialect::Mte, "RuntimeError", RuntimeError),
        (StatusDialect::Mte, "Timeout", Timeout),
        (StatusDialect::Mte, "Ignored", Ignored),
        (StatusDialect::Mte, "Pending", Pending),
        (StatusDialect::CargoMutants, "CaughtMutant", Killed),
        (StatusDialect::CargoMutants, "MissedMutant", Survived),
        (StatusDialect::CargoMutants, "Success", Survived),
        (StatusDialect::CargoMutants, "Unviable", CompileError),
        (StatusDialect::CargoMutants, "Failure", RuntimeError),
        (StatusDialect::CargoMutants, "Timeout", Timeout),
        (StatusDialect::Muter, "failed", Killed),
        (StatusDialect::Muter, "runtimeError", Killed),
        (StatusDialect::Muter, "passed", Survived),
        (StatusDialect::Muter, "buildError", CompileError),
        (StatusDialect::Muter, "timeout", Timeout),
    ];
    let status = STATUSES
        .iter()
        .find(|(candidate, reported, _)| *candidate == dialect && *reported == value)
        .map(|(_, _, status)| *status);
    status.ok_or_else(|| invalid(status_field(dialect), format!("unsupported status {value:?}")))
}

const fn status_field(dialect: StatusDialect) -> &'static str {
    match dialect {
        StatusDialect::Mte => "files.*.mutants[].status",
        StatusDialect::CargoMutants => "outcomes[].summary",
        StatusDialect::Muter => "fileReports[].appliedOperators[].testSuiteOutcome",
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
    normalize_repository_path(&relative.to_string_lossy()).map_err(|message| invalid("files", message))
}

fn validated_relative_path(root: &Path, input: &Path, field: &str) -> Result<PathBuf, ProviderError> {
    let relative = relative_input(root, input, field)?;
    let normalized = normalize_relative_components(relative, input, field)?;
    reject_escaping_symlink_prefixes(root, &normalized, field)?;
    Ok(normalized)
}

fn relative_input<'a>(root: &Path, input: &'a Path, field: &str) -> Result<&'a Path, ProviderError> {
    if !input.is_absolute() {
        return Ok(input);
    }
    input.strip_prefix(root).map_err(|_| {
        invalid(
            field,
            format!(
                "absolute path {:?} is outside project root",
                input.to_string_lossy()
            ),
        )
    })
}

fn normalize_relative_components(
    relative: &Path,
    input: &Path,
    field: &str,
) -> Result<PathBuf, ProviderError> {
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
    Ok(normalized)
}

fn reject_escaping_symlink_prefixes(root: &Path, relative: &Path, field: &str) -> Result<(), ProviderError> {
    let mut candidate = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            unreachable!("relative path was normalized before symlink validation");
        };
        candidate.push(part);
        if !validate_symlink_prefix(root, relative, &candidate, field)? {
            break;
        }
    }
    Ok(())
}

fn validate_symlink_prefix(
    root: &Path,
    relative: &Path,
    candidate: &Path,
    field: &str,
) -> Result<bool, ProviderError> {
    match fs::symlink_metadata(candidate) {
        Ok(_) => validate_canonical_prefix(root, relative, candidate, field).map(|()| true),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ProviderError::Io {
            path: candidate.to_path_buf(),
            source,
        }),
    }
}

fn validate_canonical_prefix(
    root: &Path,
    relative: &Path,
    candidate: &Path,
    field: &str,
) -> Result<(), ProviderError> {
    let canonical = fs::canonicalize(candidate).map_err(|source| ProviderError::Io {
        path: candidate.to_path_buf(),
        source,
    })?;
    if canonical.starts_with(root) {
        Ok(())
    } else {
        Err(invalid(
            field,
            format!(
                "path {:?} resolves outside project root",
                relative.to_string_lossy()
            ),
        ))
    }
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

fn reserve_stable_id(
    used_ids: &mut BTreeSet<u64>,
    provider: MutationProvider,
    file: &str,
    external_id: &str,
) -> u64 {
    let mut id = stable_id(provider, file, external_id);
    while !used_ids.insert(id) {
        id = id.wrapping_add(1);
    }
    id
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
    use crate::test_support::must;

    #[test]
    fn existing_safe_symlink_prefix_continues_validation() {
        let directory = must(tempfile::tempdir());
        let root = must(fs::canonicalize(directory.path()));
        let candidate = root.join("present");
        must(fs::create_dir(&candidate));

        assert!(must(validate_symlink_prefix(
            &root,
            Path::new("present"),
            &candidate,
            "files",
        )));
    }

    #[test]
    fn import_budget_caps_mutant_count_and_aggregate_retained_bytes() {
        let mut count_budget = ImportBudget::default();
        for _ in 0..MAX_MUTANTS {
            count_budget
                .add_mutant(0, 0)
                .unwrap_or_else(|error| panic!("within count budget: {error}"));
        }
        assert_budget_error(&count_budget.add_mutant(0, 0), "limits.mutants");

        let mut byte_budget = ImportBudget::default();
        for _ in 0..(MAX_TOTAL_RETAINED_BYTES / MAX_RETAINED_BYTES_PER_MUTANT) {
            byte_budget
                .add_mutant(MAX_RETAINED_BYTES_PER_MUTANT, 0)
                .unwrap_or_else(|error| panic!("within retained-byte budget: {error}"));
        }
        assert_budget_error(&byte_budget.add_mutant(1, 0), "limits.totalRetainedMutationBytes");
    }

    #[test]
    fn coordinate_checkpoints_preserve_bidirectional_positions() {
        let source = "a".repeat(COORDINATE_CHECKPOINT_STRIDE_BYTES * 3);
        let mut budget = ImportBudget::default();
        let table = must(CoordinateTable::new(
            &source,
            CoordinateMode::UnicodeScalar,
            &mut budget,
        ));
        let expected = COORDINATE_CHECKPOINT_STRIDE_BYTES * 2;
        let position = ReportPosition {
            line: 1,
            column: must(u32::try_from(expected + 1)),
        };

        assert!(!table.checkpoints.is_empty());
        assert_eq!(must(table.offset(&source, position)), expected);
        assert_eq!(
            must(table.scalar_position(&source, expected)),
            (1, position.column)
        );
    }

    fn assert_budget_error(result: &Result<(), ProviderError>, expected_field: &str) {
        assert!(matches!(
            result,
            Err(ProviderError::InvalidReport { ref field, .. }) if field == expected_field
        ));
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use xml::attribute::OwnedAttribute;
use xml::reader::{ParserConfig, XmlEvent};

pub type LineCoverage = BTreeMap<u32, u64>;

/// Maximum accepted size of one coverage artifact or direct parser input.
pub const MAX_COVERAGE_REPORT_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum directory entries inspected while discovering a coverage artifact.
pub const MAX_COVERAGE_DISCOVERY_ENTRIES: usize = 100_000;
/// Maximum directories inspected while discovering a coverage artifact.
pub const MAX_COVERAGE_DISCOVERY_DIRECTORIES: usize = 10_000;
/// Maximum conventional coverage artifacts retained during discovery.
pub const MAX_COVERAGE_DISCOVERY_CANDIDATES: usize = 4_096;
/// Maximum aggregate size of conventional artifacts encountered during discovery.
pub const MAX_COVERAGE_DISCOVERY_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum normalized source paths retained from a coverage artifact.
pub const MAX_COVERAGE_FILES: usize = 50_000;
/// Maximum UTF-8 byte length of one report-provided source path.
pub const MAX_COVERAGE_PATH_BYTES: usize = 32 * 1024;
/// Maximum unique executable lines retained from a coverage artifact.
pub const MAX_COVERAGE_EXECUTABLE_LINES: usize = 2_000_000;
/// Maximum unique executable lines retained for one normalized source path.
pub const MAX_COVERAGE_LINES_PER_FILE: usize = 500_000;
/// Maximum report-provided line/statement/segment candidates processed.
pub const MAX_COVERAGE_RECORDS: usize = 4_000_000;
/// Maximum Cobertura `<source>` values retained for path resolution.
pub const MAX_COBERTURA_SOURCES: usize = 128;
/// Maximum Cobertura classes retained before source resolution.
pub const MAX_COBERTURA_CLASSES: usize = 50_000;
/// Maximum Cobertura `<line>` records retained across classes.
pub const MAX_COBERTURA_CLASS_LINES: usize = 2_000_000;
/// Maximum raw/source-qualified Cobertura line candidates resolved.
pub const MAX_COBERTURA_RESOLUTION_CANDIDATES: usize = 1_000_000;
/// Maximum attributes accepted on one Cobertura XML element.
pub const MAX_COBERTURA_XML_ATTRIBUTES: usize = 256;
/// Maximum namespace declarations accepted across one Cobertura document.
pub const MAX_COBERTURA_XML_NAMESPACE_DECLARATIONS: usize = 4_096;
/// Maximum nested element depth accepted in one Cobertura document.
pub const MAX_COBERTURA_XML_DEPTH: usize = 1_024;
/// Maximum encoded bytes accepted for one Cobertura XML markup construct.
pub const MAX_COBERTURA_XML_MARKUP_BYTES: usize = 64 * 1024;
/// Maximum bytes accepted for one Cobertura XML name.
pub const MAX_COBERTURA_XML_NAME_BYTES: usize = 1_024;
/// Maximum bytes accepted for one Cobertura XML attribute or text node.
pub const MAX_COBERTURA_XML_VALUE_BYTES: usize = MAX_COVERAGE_PATH_BYTES;
/// Maximum expanded entity bytes accepted by the Cobertura XML parser.
pub const MAX_COBERTURA_XML_ENTITY_BYTES: usize = MAX_COVERAGE_PATH_BYTES;
/// Maximum recursive entity expansion depth accepted by the Cobertura parser.
pub const MAX_COBERTURA_XML_ENTITY_DEPTH: u8 = 8;
/// Maximum executable lines expanded from one LLVM code region.
pub const MAX_LLVM_REGION_LINES: usize = 100_000;
/// Maximum executable lines expanded from LLVM code regions in one artifact.
pub const MAX_LLVM_EXPANDED_LINES: usize = 2_000_000;

const COVERAGE_REPORT_NAMES: &[&str] = &[
    "lcov.info",
    "coverage-final.json",
    "coverage.json",
    "cobertura.xml",
    "coverage.xml",
    "llvm-cov.json",
    "codecov.json",
];

/// Coverage interchange formats understood by the unified analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageFormat {
    Lcov,
    Cobertura,
    CoveragePy,
    Istanbul,
    Llvm,
    /// A report assembled from inputs that used more than one format.
    Merged,
}

impl CoverageFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lcov => "lcov",
            Self::Cobertura => "cobertura",
            Self::CoveragePy => "coverage.py-json",
            Self::Istanbul => "istanbul-json",
            Self::Llvm => "llvm-export-json",
            Self::Merged => "merged",
        }
    }
}

impl std::fmt::Display for CoverageFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Executable-line hit counts, keyed by normalized source path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    format: CoverageFormat,
    files: BTreeMap<String, LineCoverage>,
}

impl CoverageReport {
    #[must_use]
    pub fn new(format: CoverageFormat) -> Self {
        Self {
            format,
            files: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn format(&self) -> CoverageFormat {
        self.format
    }

    #[must_use]
    pub fn files(&self) -> &BTreeMap<String, LineCoverage> {
        &self.files
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn executable_line_count(&self) -> usize {
        self.files
            .values()
            .map(BTreeMap::len)
            .fold(0_usize, usize::saturating_add)
    }

    #[must_use]
    pub fn covered_line_count(&self) -> usize {
        self.files
            .values()
            .flat_map(BTreeMap::values)
            .filter(|hits| **hits > 0)
            .count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.values().all(BTreeMap::is_empty)
    }

    #[must_use]
    pub fn line_hits(&self, file: &str, line: u32) -> Option<u64> {
        self.files
            .get(&normalize_path(file))
            .and_then(|lines| lines.get(&line))
            .copied()
    }

    /// Add one executable line. Repeated records retain the highest hit count,
    /// which makes merging shard/retry output deterministic and order-independent.
    pub fn insert_line(&mut self, file: &str, line: u32, hits: u64) {
        if line == 0 {
            return;
        }
        let file = normalize_path(file);
        if file == "." || file.is_empty() {
            return;
        }
        let entry = self.files.entry(file).or_default().entry(line).or_default();
        *entry = (*entry).max(hits);
    }

    pub fn merge(&mut self, other: &Self) {
        if self.format != other.format {
            self.format = CoverageFormat::Merged;
        }
        for (file, lines) in &other.files {
            for (&line, &hits) in lines {
                self.insert_line(file, line, hits);
            }
        }
    }

    /// Resolve a function/source path against report paths. Exact relative and
    /// absolute matches win. A suffix or basename fallback is accepted only
    /// when exactly one report path matches.
    #[must_use]
    pub fn lines_for_file<'a>(&'a self, root: &Path, file: &str) -> Option<&'a LineCoverage> {
        let relative = normalize_path(file);
        if let Some(lines) = self.files.get(&relative) {
            return Some(lines);
        }

        let joined = normalized_join(root, &relative);
        if let Some(lines) = self.files.get(&joined) {
            return Some(lines);
        }

        let canonical = root
            .join(&relative)
            .canonicalize()
            .ok()
            .map(|path| normalize_path(&path.to_string_lossy()));
        if let Some(lines) = canonical.as_ref().and_then(|path| self.files.get(path)) {
            return Some(lines);
        }

        if let Some(lines) = only_matching_line_set(self.files.iter().filter_map(|(candidate, lines)| {
            (path_is_suffix(candidate, &relative) || path_is_suffix(&relative, candidate)).then_some(lines)
        })) {
            return Some(lines);
        }

        let basename = relative.rsplit('/').next().unwrap_or(relative.as_str());
        only_matching_line_set(self.files.iter().filter_map(|(candidate, lines)| {
            (candidate.rsplit('/').next() == Some(basename)).then_some(lines)
        }))
    }
}

fn only_matching_line_set<'a>(
    mut values: impl Iterator<Item = &'a LineCoverage>,
) -> Option<&'a LineCoverage> {
    let first = values.next()?;
    values.next().is_none().then_some(first)
}

fn path_is_suffix(path: &str, suffix: &str) -> bool {
    path == suffix
        || (!suffix.is_empty()
            && path
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('/')))
}

fn absolute_like(path: &str) -> bool {
    path.starts_with('/') || path.as_bytes().get(1).is_some_and(|byte| *byte == b':')
}

fn normalized_join(root: &Path, file: &str) -> String {
    if absolute_like(file) {
        normalize_path(file)
    } else {
        normalize_path(&root.join(file).to_string_lossy())
    }
}

/// Normalize separators and `.`/`..` components without requiring the path to
/// exist. Windows drive and UNC paths are folded to lowercase because their
/// coverage producers commonly disagree on casing.
#[must_use]
pub fn normalize_path(value: &str) -> String {
    let mut raw = value.trim().replace('\\', "/");
    if let Some(without_scheme) = raw.strip_prefix("file://") {
        raw = without_scheme.to_owned();
    }

    let windows_drive = raw.as_bytes().get(1).is_some_and(|byte| *byte == b':');
    let unc = raw.starts_with("//");
    let absolute = raw.starts_with('/');
    let (prefix, remainder) = if windows_drive {
        (&raw[..2], raw[2..].trim_start_matches('/'))
    } else if unc {
        ("//", raw.trim_start_matches('/'))
    } else if absolute {
        ("/", raw.trim_start_matches('/'))
    } else {
        ("", raw.as_str())
    };

    let mut components: Vec<&str> = Vec::new();
    for component in remainder.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.last().is_some_and(|last| *last != "..") {
                    components.pop();
                } else if prefix.is_empty() {
                    components.push(component);
                }
            }
            _ => components.push(component),
        }
    }

    let body = components.join("/");
    let mut normalized = match prefix {
        "/" => format!("/{body}"),
        "//" => format!("//{body}"),
        "" if body.is_empty() => ".".to_owned(),
        "" => body,
        drive => {
            if body.is_empty() {
                format!("{drive}/")
            } else {
                format!("{drive}/{body}")
            }
        }
    };
    if windows_drive || unc {
        normalized.make_ascii_lowercase();
    }
    normalized
}

#[derive(Debug, thiserror::Error)]
pub enum CoverageError {
    #[error("cannot read coverage report {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("coverage report does not exist: {0}")]
    Missing(String),
    #[error("no supported coverage report found under {0}")]
    NotFound(String),
    #[error("cannot determine coverage format for {0}")]
    Unsupported(String),
    #[error("invalid {format} coverage: {message}")]
    Parse { format: &'static str, message: String },
    #[error("coverage report contains no executable lines: {0}")]
    Empty(String),
    #[error("unsafe coverage path {path}: {reason}")]
    UnsafePath { path: String, reason: &'static str },
    #[error("coverage resource limit exceeded for {resource}: maximum {limit}")]
    ResourceLimit { resource: &'static str, limit: u64 },
}

fn resource_limit(resource: &'static str, limit: usize) -> CoverageError {
    CoverageError::ResourceLimit {
        resource,
        limit: u64::try_from(limit).unwrap_or(u64::MAX),
    }
}

fn ensure_count(actual: usize, limit: usize, resource: &'static str) -> Result<(), CoverageError> {
    if actual > limit {
        return Err(resource_limit(resource, limit));
    }
    Ok(())
}

fn ensure_report_size(size: u64) -> Result<(), CoverageError> {
    if size > MAX_COVERAGE_REPORT_BYTES {
        return Err(CoverageError::ResourceLimit {
            resource: "coverage report bytes",
            limit: MAX_COVERAGE_REPORT_BYTES,
        });
    }
    Ok(())
}

fn unsafe_path(path: &Path, reason: &'static str) -> CoverageError {
    CoverageError::UnsafePath {
        path: path.display().to_string(),
        reason,
    }
}

fn validate_discovered_candidate(
    entry_path: &Path,
    discovery_root: &Path,
    candidate_bytes: &mut u64,
) -> Result<PathBuf, CoverageError> {
    let candidate = entry_path.canonicalize().map_err(|source| CoverageError::Read {
        path: entry_path.display().to_string(),
        source,
    })?;
    if !candidate.starts_with(discovery_root) {
        return Err(unsafe_path(
            &candidate,
            "discovered report escapes the requested directory",
        ));
    }
    let metadata = fs::symlink_metadata(&candidate).map_err(|source| CoverageError::Read {
        path: candidate.display().to_string(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(unsafe_path(&candidate, "expected a regular non-symlink file"));
    }
    ensure_report_size(metadata.len())?;
    *candidate_bytes = candidate_bytes
        .checked_add(metadata.len())
        .ok_or(CoverageError::ResourceLimit {
            resource: "coverage discovery candidate bytes",
            limit: MAX_COVERAGE_DISCOVERY_BYTES,
        })?;
    if *candidate_bytes > MAX_COVERAGE_DISCOVERY_BYTES {
        return Err(CoverageError::ResourceLimit {
            resource: "coverage discovery candidate bytes",
            limit: MAX_COVERAGE_DISCOVERY_BYTES,
        });
    }
    Ok(candidate)
}

/// Locate a conventional report below a supplied file or directory.
///
/// # Errors
///
/// Returns an error when the path does not exist, a directory cannot be read,
/// or no file with a supported conventional report name can be found.
pub fn discover_coverage_report(path: &Path) -> Result<PathBuf, CoverageError> {
    let supplied_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(CoverageError::Missing(path.display().to_string()));
        }
        Err(source) => {
            return Err(CoverageError::Read {
                path: path.display().to_string(),
                source,
            });
        }
    };
    if supplied_metadata.file_type().is_symlink() {
        return Err(unsafe_path(path, "symbolic links are not accepted"));
    }
    if supplied_metadata.is_file() {
        ensure_report_size(supplied_metadata.len())?;
        return Ok(path.to_path_buf());
    }
    if !supplied_metadata.is_dir() {
        return Err(unsafe_path(path, "expected a regular file or directory"));
    }

    let discovery_root = path.canonicalize().map_err(|source| CoverageError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mut pending = vec![discovery_root.clone()];
    let mut candidates = Vec::new();
    let mut entry_count = 0_usize;
    let mut directory_count = 1_usize;
    let mut candidate_bytes = 0_u64;
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory).map_err(|source| CoverageError::Read {
            path: directory.display().to_string(),
            source,
        })?;
        for entry in entries {
            entry_count = entry_count.saturating_add(1);
            ensure_count(
                entry_count,
                MAX_COVERAGE_DISCOVERY_ENTRIES,
                "coverage discovery entries",
            )?;
            let entry = entry.map_err(|source| CoverageError::Read {
                path: directory.display().to_string(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| CoverageError::Read {
                path: entry.path().display().to_string(),
                source,
            })?;
            if file_type.is_dir() && !file_type.is_symlink() {
                directory_count = directory_count.saturating_add(1);
                ensure_count(
                    directory_count,
                    MAX_COVERAGE_DISCOVERY_DIRECTORIES,
                    "coverage discovery directories",
                )?;
                pending.push(entry.path());
            } else if file_type.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| COVERAGE_REPORT_NAMES.contains(&name))
            {
                let entry_path = entry.path();
                let candidate =
                    validate_discovered_candidate(&entry_path, &discovery_root, &mut candidate_bytes)?;
                ensure_count(
                    candidates.len().saturating_add(1),
                    MAX_COVERAGE_DISCOVERY_CANDIDATES,
                    "coverage discovery candidates",
                )?;
                candidates.push(candidate);
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| CoverageError::NotFound(path.display().to_string()))
}

fn read_coverage_text(path: &Path) -> Result<String, CoverageError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| CoverageError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(unsafe_path(path, "expected a regular non-symlink file"));
    }
    ensure_report_size(metadata.len())?;

    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|source| CoverageError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let opened_metadata = file.metadata().map_err(|source| CoverageError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if !opened_metadata.is_file() {
        return Err(unsafe_path(path, "expected a regular file"));
    }
    ensure_report_size(opened_metadata.len())?;

    let mut text = String::new();
    file.take(MAX_COVERAGE_REPORT_BYTES.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|source| CoverageError::Read {
            path: path.display().to_string(),
            source,
        })?;
    ensure_report_size(u64::try_from(text.len()).unwrap_or(u64::MAX))?;
    Ok(text)
}

/// Load a coverage report from a file or a directory containing a
/// conventionally named report.
///
/// # Errors
///
/// Returns an error for discovery/read failures, unrecognized or malformed
/// content, or reports containing no executable lines.
pub fn load_coverage(path: &Path) -> Result<CoverageReport, CoverageError> {
    let report_path = discover_coverage_report(path)?;
    let text = read_coverage_text(&report_path)?;
    let format = detect_format(&report_path, &text)?;
    let report = parse_coverage(format, &text)?;
    if report.is_empty() {
        return Err(CoverageError::Empty(report_path.display().to_string()));
    }
    Ok(report)
}

/// Load a report using an explicitly selected format.
///
/// # Errors
///
/// Returns an error when the file cannot be read, its content is invalid for
/// `format`, or it contains no executable lines.
pub fn load_coverage_as(path: &Path, format: CoverageFormat) -> Result<CoverageReport, CoverageError> {
    let text = read_coverage_text(path)?;
    let report = parse_coverage(format, &text)?;
    if report.is_empty() {
        return Err(CoverageError::Empty(path.display().to_string()));
    }
    Ok(report)
}

fn detect_format(path: &Path, text: &str) -> Result<CoverageFormat, CoverageError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "info" | "lcov") {
        return Ok(CoverageFormat::Lcov);
    }
    if extension == "xml" {
        return Ok(CoverageFormat::Cobertura);
    }
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("TN:") || trimmed.starts_with("SF:") {
        return Ok(CoverageFormat::Lcov);
    }
    if trimmed.starts_with('<') {
        return Ok(CoverageFormat::Cobertura);
    }

    let value: Value = serde_json::from_str(trimmed).map_err(|error| CoverageError::Parse {
        format: "json",
        message: error.to_string(),
    })?;
    let object = value.as_object().ok_or_else(|| CoverageError::Parse {
        format: "json",
        message: "top-level value must be an object".to_owned(),
    })?;
    if object.get("data").is_some_and(Value::is_array) {
        return Ok(CoverageFormat::Llvm);
    }
    if object
        .get("files")
        .and_then(Value::as_object)
        .is_some_and(|files| {
            files
                .values()
                .any(|file| file.get("executed_lines").is_some() || file.get("missing_lines").is_some())
        })
    {
        return Ok(CoverageFormat::CoveragePy);
    }
    if object.values().any(is_istanbul_file) {
        return Ok(CoverageFormat::Istanbul);
    }
    Err(CoverageError::Unsupported(path.display().to_string()))
}

/// Parse coverage text using an explicitly selected interchange format.
///
/// # Errors
///
/// Returns an error when the selected format is not an input format or the
/// text does not satisfy that format's required structure.
pub fn parse_coverage(format: CoverageFormat, text: &str) -> Result<CoverageReport, CoverageError> {
    match format {
        CoverageFormat::Lcov => parse_lcov(text),
        CoverageFormat::Cobertura => parse_cobertura(text),
        CoverageFormat::CoveragePy => parse_coverage_py_json(text),
        CoverageFormat::Istanbul => parse_istanbul_json(text),
        CoverageFormat::Llvm => parse_llvm_json(text),
        CoverageFormat::Merged => Err(CoverageError::Unsupported(
            "merged is an output format, not an input format".to_owned(),
        )),
    }
}

#[derive(Debug, Default)]
struct ParseBudget {
    records: usize,
    unique_lines: usize,
}

impl ParseBudget {
    fn consume_records(&mut self, amount: usize, resource: &'static str) -> Result<(), CoverageError> {
        self.records = self
            .records
            .checked_add(amount)
            .ok_or_else(|| resource_limit(resource, MAX_COVERAGE_RECORDS))?;
        ensure_count(self.records, MAX_COVERAGE_RECORDS, resource)
    }

    fn insert_line(
        &mut self,
        report: &mut CoverageReport,
        file: &str,
        line: u32,
        hits: u64,
    ) -> Result<(), CoverageError> {
        if line == 0 {
            return Ok(());
        }
        ensure_count(file.len(), MAX_COVERAGE_PATH_BYTES, "coverage source path bytes")?;
        let file = normalize_path(file);
        if file == "." || file.is_empty() {
            return Ok(());
        }

        let new_file = !report.files.contains_key(&file);
        if new_file {
            ensure_count(
                report.files.len().saturating_add(1),
                MAX_COVERAGE_FILES,
                "normalized coverage files",
            )?;
        }
        let new_line = report
            .files
            .get(&file)
            .is_none_or(|lines| !lines.contains_key(&line));
        if new_line {
            let file_lines = report.files.get(&file).map_or(0, BTreeMap::len);
            ensure_count(
                file_lines.saturating_add(1),
                MAX_COVERAGE_LINES_PER_FILE,
                "executable coverage lines per file",
            )?;
            self.unique_lines = self.unique_lines.checked_add(1).ok_or_else(|| {
                resource_limit("unique executable coverage lines", MAX_COVERAGE_EXECUTABLE_LINES)
            })?;
            ensure_count(
                self.unique_lines,
                MAX_COVERAGE_EXECUTABLE_LINES,
                "unique executable coverage lines",
            )?;
        }

        let entry = report.files.entry(file).or_default().entry(line).or_default();
        *entry = (*entry).max(hits);
        Ok(())
    }
}

fn ensure_text_size(text: &str) -> Result<(), CoverageError> {
    ensure_report_size(u64::try_from(text.len()).unwrap_or(u64::MAX))
}

/// Parse an LCOV tracefile.
///
/// # Errors
///
/// Malformed individual records are ignored. Oversized inputs and reports that
/// exceed the documented record or normalized-output limits are rejected.
pub fn parse_lcov(text: &str) -> Result<CoverageReport, CoverageError> {
    ensure_text_size(text)?;
    let mut report = CoverageReport::new(CoverageFormat::Lcov);
    let mut budget = ParseBudget::default();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        budget.consume_records(1, "LCOV records")?;
        let line = raw.trim_end_matches('\r');
        if let Some(file) = line.strip_prefix("SF:") {
            let file = normalize_path(file);
            current = (file != ".").then_some(file);
        } else if line == "end_of_record" {
            current = None;
        } else if let (Some(file), Some(data)) = (current.as_deref(), line.strip_prefix("DA:")) {
            let mut fields = data.split(',');
            let line = fields.next().and_then(parse_line_number);
            let hits = fields.next().and_then(parse_hit_count);
            if let (Some(line), Some(hits)) = (line, hits) {
                budget.insert_line(&mut report, file, line, hits)?;
            }
        }
    }
    Ok(report)
}

fn xml_attribute<'a>(
    attributes: &'a [OwnedAttribute],
    requested: &str,
) -> Result<Option<&'a str>, CoverageError> {
    let mut seen = BTreeSet::new();
    let mut selected = None;
    for attribute in attributes {
        let identity = (
            attribute.name.namespace.as_deref(),
            attribute.name.local_name.as_str(),
        );
        if !seen.insert(identity) {
            return Err(CoverageError::Parse {
                format: "cobertura",
                message: format!("duplicate XML attribute {}", attribute.name),
            });
        }
        if attribute.name.prefix.is_none() && attribute.name.local_name == requested {
            selected = Some(attribute.value.as_str());
        }
    }
    Ok(selected)
}

fn append_xml_line(
    attributes: &[OwnedAttribute],
    current: &mut Option<(String, LineCoverage)>,
    class_line_records: &mut usize,
) -> Result<(), CoverageError> {
    *class_line_records = class_line_records
        .checked_add(1)
        .ok_or_else(|| resource_limit("Cobertura class-line records", MAX_COBERTURA_CLASS_LINES))?;
    ensure_count(
        *class_line_records,
        MAX_COBERTURA_CLASS_LINES,
        "Cobertura class-line records",
    )?;
    let Some((_, lines)) = current.as_mut() else {
        return Ok(());
    };
    let line = xml_attribute(attributes, "number")?.and_then(parse_line_number);
    let hits = xml_attribute(attributes, "hits")?
        .and_then(parse_hit_count)
        .unwrap_or(0);
    if let Some(line) = line {
        if !lines.contains_key(&line) {
            ensure_count(
                lines.len().saturating_add(1),
                MAX_COVERAGE_LINES_PER_FILE,
                "Cobertura lines per class",
            )?;
        }
        let entry = lines.entry(line).or_default();
        *entry = (*entry).max(hits);
    }
    Ok(())
}

fn limited_normalized_path(value: &str, resource: &'static str) -> Result<String, CoverageError> {
    ensure_count(value.len(), MAX_COVERAGE_PATH_BYTES, resource)?;
    let normalized = normalize_path(value);
    ensure_count(normalized.len(), MAX_COVERAGE_PATH_BYTES, resource)?;
    Ok(normalized)
}

fn push_cobertura_class(
    classes: &mut Vec<(String, LineCoverage)>,
    class: (String, LineCoverage),
) -> Result<(), CoverageError> {
    ensure_count(
        classes.len().saturating_add(1),
        MAX_COBERTURA_CLASSES,
        "Cobertura classes",
    )?;
    classes.push(class);
    Ok(())
}

fn append_cobertura_source(current_source: &mut Option<String>, decoded: &str) -> Result<(), CoverageError> {
    if let Some(source) = current_source.as_mut() {
        source.push_str(decoded);
        ensure_count(
            source.len(),
            MAX_COVERAGE_PATH_BYTES,
            "Cobertura source path bytes",
        )?;
    }
    Ok(())
}

fn build_cobertura_report(
    classes: Vec<(String, LineCoverage)>,
    sources: &[String],
    budget: &mut ParseBudget,
) -> Result<CoverageReport, CoverageError> {
    let mut resolution_candidates = 0_usize;
    for (file, lines) in &classes {
        let aliases = if absolute_like(file) {
            1
        } else {
            sources.len().saturating_add(1)
        };
        let class_candidates = lines.len().checked_mul(aliases).ok_or_else(|| {
            resource_limit(
                "Cobertura source-resolution candidates",
                MAX_COBERTURA_RESOLUTION_CANDIDATES,
            )
        })?;
        resolution_candidates = resolution_candidates
            .checked_add(class_candidates)
            .ok_or_else(|| {
                resource_limit(
                    "Cobertura source-resolution candidates",
                    MAX_COBERTURA_RESOLUTION_CANDIDATES,
                )
            })?;
        ensure_count(
            resolution_candidates,
            MAX_COBERTURA_RESOLUTION_CANDIDATES,
            "Cobertura source-resolution candidates",
        )?;
    }

    let mut report = CoverageReport::new(CoverageFormat::Cobertura);
    for (file, lines) in classes {
        for (&line, &hits) in &lines {
            budget.insert_line(&mut report, &file, line, hits)?;
            if !absolute_like(&file) {
                for source in sources {
                    let alias = limited_normalized_path(
                        &format!("{source}/{file}"),
                        "Cobertura resolved path bytes",
                    )?;
                    budget.insert_line(&mut report, &alias, line, hits)?;
                }
            }
        }
    }
    Ok(report)
}

fn find_xml_terminator(bytes: &[u8], from: usize, terminator: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(terminator.len())
        .position(|window| window == terminator)
        .map(|offset| from + offset + terminator.len())
}

fn xml_attribute_name_before_equals(bytes: &[u8], tag_start: usize, equals: usize) -> &[u8] {
    let mut end = equals;
    while end > tag_start + 1 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > tag_start + 1 {
        let byte = bytes[start - 1];
        if byte.is_ascii_whitespace() || matches!(byte, b'<' | b'/' | b'?') {
            break;
        }
        start -= 1;
    }
    &bytes[start..end]
}

fn is_xml_namespace_declaration(name: &[u8]) -> bool {
    name == b"xmlns"
        || name
            .strip_prefix(b"xmlns:")
            .is_some_and(|local| !local.is_empty())
}

fn bounded_xml_construct_end(
    bytes: &[u8],
    start: usize,
    prefix: &[u8],
    terminator: &[u8],
    limit: usize,
    resource: &'static str,
    unterminated: &'static str,
) -> Result<usize, CoverageError> {
    let end =
        find_xml_terminator(bytes, start + prefix.len(), terminator).ok_or_else(|| CoverageError::Parse {
            format: "cobertura",
            message: unterminated.into(),
        })?;
    ensure_count(end - start, limit, resource)?;
    Ok(end)
}

fn preflight_special_xml_markup(bytes: &[u8], start: usize) -> Result<Option<usize>, CoverageError> {
    if bytes[start..].starts_with(b"<!--") {
        return bounded_xml_construct_end(
            bytes,
            start,
            b"<!--",
            b"-->",
            MAX_COBERTURA_XML_VALUE_BYTES,
            "Cobertura XML comment bytes",
            "unterminated XML comment",
        )
        .map(Some);
    }
    if bytes[start..].starts_with(b"<![CDATA[") {
        return bounded_xml_construct_end(
            bytes,
            start,
            b"<![CDATA[",
            b"]]>",
            MAX_COBERTURA_XML_VALUE_BYTES,
            "Cobertura XML CDATA bytes",
            "unterminated XML CDATA section",
        )
        .map(Some);
    }
    if bytes[start..].starts_with(b"<?") {
        return bounded_xml_construct_end(
            bytes,
            start,
            b"<?",
            b"?>",
            MAX_COBERTURA_XML_MARKUP_BYTES,
            "Cobertura XML markup bytes",
            "unterminated XML processing instruction",
        )
        .map(Some);
    }
    if bytes[start..].starts_with(b"<!") {
        return Err(CoverageError::Parse {
            format: "cobertura",
            message: "DTD and other XML declarations are not accepted in coverage reports".into(),
        });
    }
    Ok(None)
}

fn preflight_cobertura_xml(text: &str) -> Result<(), CoverageError> {
    let bytes = text.as_bytes();
    let mut cursor = 0_usize;
    let mut depth = 0_usize;
    let mut namespace_declarations = 0_usize;
    while let Some(relative) = bytes[cursor..].iter().position(|byte| *byte == b'<') {
        let start = cursor + relative;
        if let Some(end) = preflight_special_xml_markup(bytes, start)? {
            cursor = end;
            continue;
        }

        let mut quote = None;
        let mut attributes = 0_usize;
        let mut end = None;
        for (relative_index, byte) in bytes[start + 1..].iter().copied().enumerate() {
            let index = start + 1 + relative_index;
            if let Some(delimiter) = quote {
                if byte == delimiter {
                    quote = None;
                }
                continue;
            }
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'=' => {
                    attributes = attributes.saturating_add(1);
                    ensure_count(
                        attributes,
                        MAX_COBERTURA_XML_ATTRIBUTES,
                        "Cobertura XML attributes per element",
                    )?;
                    if is_xml_namespace_declaration(xml_attribute_name_before_equals(bytes, start, index)) {
                        namespace_declarations = namespace_declarations.saturating_add(1);
                        ensure_count(
                            namespace_declarations,
                            MAX_COBERTURA_XML_NAMESPACE_DECLARATIONS,
                            "Cobertura XML namespace declarations",
                        )?;
                    }
                }
                b'>' => {
                    end = Some(index + 1);
                    break;
                }
                _ => {}
            }
        }
        let end = end.ok_or_else(|| CoverageError::Parse {
            format: "cobertura",
            message: "unterminated XML tag".into(),
        })?;
        ensure_count(
            end - start,
            MAX_COBERTURA_XML_MARKUP_BYTES,
            "Cobertura XML markup bytes",
        )?;

        let closing = bytes.get(start + 1) == Some(&b'/');
        let self_closing = bytes[start..end - 1]
            .iter()
            .rev()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(&b'/');
        if closing {
            depth = depth.saturating_sub(1);
        } else if !self_closing {
            depth = depth.saturating_add(1);
            ensure_count(depth, MAX_COBERTURA_XML_DEPTH, "Cobertura XML element depth")?;
        }
        cursor = end;
    }
    Ok(())
}

/// Parse a Cobertura XML report.
///
/// # Errors
///
/// Returns an error when XML structure, encoding, or attributes cannot be
/// decoded, or when bounded source/class/line resolution limits are exceeded.
pub fn parse_cobertura(text: &str) -> Result<CoverageReport, CoverageError> {
    ensure_text_size(text)?;
    preflight_cobertura_xml(text)?;
    let reader = ParserConfig::new()
        .trim_whitespace(true)
        .ignore_comments(true)
        .coalesce_characters(true)
        .allow_multiple_root_elements(false)
        .max_entity_expansion_length(MAX_COBERTURA_XML_ENTITY_BYTES)
        .max_entity_expansion_depth(MAX_COBERTURA_XML_ENTITY_DEPTH)
        .max_name_length(MAX_COBERTURA_XML_NAME_BYTES)
        .max_attributes(MAX_COBERTURA_XML_ATTRIBUTES)
        .max_attribute_length(MAX_COBERTURA_XML_VALUE_BYTES)
        .max_data_length(MAX_COBERTURA_XML_VALUE_BYTES)
        .create_reader(text.as_bytes());
    let mut budget = ParseBudget::default();
    let mut sources = Vec::new();
    let mut current_source: Option<String> = None;
    let mut current: Option<(String, LineCoverage)> = None;
    let mut classes = Vec::new();
    let mut class_records = 0_usize;
    let mut class_line_records = 0_usize;

    for event in reader {
        budget.consume_records(1, "Cobertura XML events")?;
        let event = event.map_err(|error| CoverageError::Parse {
            format: "cobertura",
            message: error.to_string(),
        })?;
        match event {
            XmlEvent::StartElement { name, attributes, .. } => {
                if name.local_name == "source" {
                    current_source = Some(String::new());
                } else if name.local_name == "class" {
                    class_records = class_records.saturating_add(1);
                    ensure_count(class_records, MAX_COBERTURA_CLASSES, "Cobertura classes")?;
                    if let Some(class) = current.take() {
                        push_cobertura_class(&mut classes, class)?;
                    }
                    current = xml_attribute(&attributes, "filename")?
                        .map(|file| {
                            limited_normalized_path(file, "Cobertura class filename bytes")
                                .map(|file| (file, LineCoverage::new()))
                        })
                        .transpose()?;
                } else if name.local_name == "line" {
                    append_xml_line(&attributes, &mut current, &mut class_line_records)?;
                }
            }
            XmlEvent::Characters(value) | XmlEvent::CData(value) if current_source.is_some() => {
                append_cobertura_source(&mut current_source, &value)?;
            }
            XmlEvent::EndElement { name } if name.local_name == "source" => {
                let source = limited_normalized_path(
                    current_source.take().as_deref().unwrap_or_default(),
                    "Cobertura source path bytes",
                )?;
                if source != "." {
                    ensure_count(
                        sources.len().saturating_add(1),
                        MAX_COBERTURA_SOURCES,
                        "Cobertura sources",
                    )?;
                    sources.push(source);
                }
            }
            XmlEvent::EndElement { name } if name.local_name == "class" => {
                if let Some(class) = current.take() {
                    push_cobertura_class(&mut classes, class)?;
                }
            }
            XmlEvent::EndDocument => break,
            _ => {}
        }
    }
    if let Some(class) = current {
        push_cobertura_class(&mut classes, class)?;
    }
    build_cobertura_report(classes, &sources, &mut budget)
}

fn json_object(text: &str, format: &'static str) -> Result<Value, CoverageError> {
    ensure_text_size(text)?;
    let text = text.trim_start_matches('\u{feff}');
    let value: Value = serde_json::from_str(text).map_err(|error| CoverageError::Parse {
        format,
        message: error.to_string(),
    })?;
    if !value.is_object() {
        return Err(CoverageError::Parse {
            format,
            message: "top-level value must be an object".to_owned(),
        });
    }
    Ok(value)
}

/// Parse the JSON report emitted by `coverage.py`.
///
/// # Errors
///
/// Returns an error for invalid JSON or a missing top-level `files` object.
pub fn parse_coverage_py_json(text: &str) -> Result<CoverageReport, CoverageError> {
    let object = json_object(text, "coverage.py json")?;
    let files = object
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| CoverageError::Parse {
            format: "coverage.py json",
            message: "missing files object".to_owned(),
        })?;
    ensure_count(files.len(), MAX_COVERAGE_FILES, "coverage.py files")?;
    let mut report = CoverageReport::new(CoverageFormat::CoveragePy);
    let mut budget = ParseBudget::default();
    budget.consume_records(files.len(), "coverage.py records")?;
    for (file, value) in files {
        ensure_count(
            file.len(),
            MAX_COVERAGE_PATH_BYTES,
            "coverage.py source path bytes",
        )?;
        let Some(data) = value.as_object() else {
            continue;
        };
        if let Some(executed) = data.get("executed_lines").and_then(Value::as_array) {
            budget.consume_records(executed.len(), "coverage.py records")?;
            for line in executed.iter().filter_map(parse_json_line) {
                budget.insert_line(&mut report, file, line, 1)?;
            }
        }
        if let Some(missing) = data.get("missing_lines").and_then(Value::as_array) {
            budget.consume_records(missing.len(), "coverage.py records")?;
            for line in missing.iter().filter_map(parse_json_line) {
                budget.insert_line(&mut report, file, line, 0)?;
            }
        }
    }
    Ok(report)
}

fn is_istanbul_file(value: &Value) -> bool {
    value.get("statementMap").is_some_and(Value::is_object) && value.get("s").is_some_and(Value::is_object)
}

/// Parse an Istanbul `coverage-final.json` report.
///
/// # Errors
///
/// Returns an error when the input is not a JSON object.
pub fn parse_istanbul_json(text: &str) -> Result<CoverageReport, CoverageError> {
    let object = json_object(text, "istanbul json")?;
    let object = object.as_object().ok_or_else(|| CoverageError::Parse {
        format: "istanbul json",
        message: "top-level value must be an object".to_owned(),
    })?;
    ensure_count(object.len(), MAX_COVERAGE_FILES, "Istanbul top-level files")?;
    let mut report = CoverageReport::new(CoverageFormat::Istanbul);
    let mut budget = ParseBudget::default();
    budget.consume_records(object.len(), "Istanbul records")?;
    for (key, value) in object {
        if !is_istanbul_file(value) {
            continue;
        }
        let Some(data) = value.as_object() else {
            continue;
        };
        let file = data.get("path").and_then(Value::as_str).unwrap_or(key);
        let Some(statements) = data.get("statementMap").and_then(Value::as_object) else {
            continue;
        };
        let Some(counts) = data.get("s").and_then(Value::as_object) else {
            continue;
        };
        budget.consume_records(statements.len(), "Istanbul records")?;
        budget.consume_records(counts.len(), "Istanbul records")?;
        for (id, location) in statements {
            let line = location
                .get("start")
                .and_then(|start| start.get("line"))
                .and_then(parse_json_line);
            let hits = counts.get(id).and_then(parse_json_hits).unwrap_or(0);
            if let Some(line) = line {
                budget.insert_line(&mut report, file, line, hits)?;
            }
        }
    }
    Ok(report)
}

/// Parse `llvm-cov export` JSON.
///
/// # Errors
///
/// Returns an error for invalid JSON or a missing top-level `data` array.
pub fn parse_llvm_json(text: &str) -> Result<CoverageReport, CoverageError> {
    let object = json_object(text, "llvm export json")?;
    let data = object
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| CoverageError::Parse {
            format: "llvm export json",
            message: "missing data array".to_owned(),
        })?;
    let mut budget = preflight_llvm(data)?;
    let mut report = CoverageReport::new(CoverageFormat::Llvm);
    for item in data.iter().filter_map(Value::as_object) {
        if let Some(functions) = item.get("functions").and_then(Value::as_array) {
            for function in functions.iter().filter_map(Value::as_object) {
                append_llvm_function(&mut report, function, &mut budget)?;
            }
        }
        if let Some(files) = item.get("files").and_then(Value::as_array) {
            for file in files.iter().filter_map(Value::as_object) {
                append_llvm_segments(&mut report, file, &mut budget)?;
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Clone, Copy)]
struct LlvmRegion<'a> {
    start: u32,
    end: u32,
    hits: u64,
    file: &'a str,
}

fn llvm_code_region<'a>(region: &'a [Value], filenames: &'a [Value]) -> Option<LlvmRegion<'a>> {
    if region.len() < 6 || region.get(7).and_then(parse_json_hits).unwrap_or(0) != 0 {
        return None;
    }
    let start = region.first().and_then(parse_json_line)?;
    let end = region
        .get(2)
        .and_then(parse_json_line)
        .unwrap_or(start)
        .max(start);
    let hits = region.get(4).and_then(parse_json_hits).unwrap_or(0);
    let file_index = region
        .get(5)
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())?;
    let file = filenames.get(file_index).and_then(Value::as_str)?;
    Some(LlvmRegion {
        start,
        end,
        hits,
        file,
    })
}

fn llvm_region_span(region: LlvmRegion<'_>) -> Result<usize, CoverageError> {
    let span = u64::from(region.end)
        .checked_sub(u64::from(region.start))
        .and_then(|difference| difference.checked_add(1))
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| resource_limit("LLVM executable lines per region", MAX_LLVM_REGION_LINES))?;
    ensure_count(span, MAX_LLVM_REGION_LINES, "LLVM executable lines per region")?;
    Ok(span)
}

fn preflight_llvm(data: &[Value]) -> Result<ParseBudget, CoverageError> {
    let mut budget = ParseBudget::default();
    let mut expanded_lines = 0_usize;
    budget.consume_records(data.len(), "LLVM coverage records")?;
    for item in data.iter().filter_map(Value::as_object) {
        if let Some(functions) = item.get("functions").and_then(Value::as_array) {
            budget.consume_records(functions.len(), "LLVM coverage records")?;
            for function in functions.iter().filter_map(Value::as_object) {
                let Some(filenames) = function.get("filenames").and_then(Value::as_array) else {
                    continue;
                };
                let Some(regions) = function.get("regions").and_then(Value::as_array) else {
                    continue;
                };
                ensure_count(filenames.len(), MAX_COVERAGE_FILES, "LLVM filenames per function")?;
                budget.consume_records(filenames.len(), "LLVM coverage records")?;
                budget.consume_records(regions.len(), "LLVM coverage records")?;
                for region in regions.iter().filter_map(Value::as_array) {
                    let Some(region) = llvm_code_region(region, filenames) else {
                        continue;
                    };
                    ensure_count(
                        region.file.len(),
                        MAX_COVERAGE_PATH_BYTES,
                        "LLVM source path bytes",
                    )?;
                    let span = llvm_region_span(region)?;
                    expanded_lines = expanded_lines.checked_add(span).ok_or_else(|| {
                        resource_limit("LLVM expanded executable lines", MAX_LLVM_EXPANDED_LINES)
                    })?;
                    ensure_count(
                        expanded_lines,
                        MAX_LLVM_EXPANDED_LINES,
                        "LLVM expanded executable lines",
                    )?;
                    budget.consume_records(span, "LLVM coverage records")?;
                }
            }
        }
        if let Some(files) = item.get("files").and_then(Value::as_array) {
            budget.consume_records(files.len(), "LLVM coverage records")?;
            for file in files.iter().filter_map(Value::as_object) {
                let Some(filename) = file.get("filename").and_then(Value::as_str) else {
                    continue;
                };
                let Some(segments) = file.get("segments").and_then(Value::as_array) else {
                    continue;
                };
                ensure_count(filename.len(), MAX_COVERAGE_PATH_BYTES, "LLVM source path bytes")?;
                budget.consume_records(segments.len(), "LLVM coverage records")?;
            }
        }
    }
    Ok(budget)
}

fn append_llvm_function(
    report: &mut CoverageReport,
    function: &Map<String, Value>,
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    let Some(filenames) = function.get("filenames").and_then(Value::as_array) else {
        return Ok(());
    };
    let Some(regions) = function.get("regions").and_then(Value::as_array) else {
        return Ok(());
    };
    for region in regions.iter().filter_map(Value::as_array) {
        let Some(region) = llvm_code_region(region, filenames) else {
            continue;
        };
        for line in region.start..=region.end {
            budget.insert_line(report, region.file, line, region.hits)?;
        }
    }
    Ok(())
}

fn append_llvm_segments(
    report: &mut CoverageReport,
    file: &Map<String, Value>,
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    let Some(filename) = file.get("filename").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(segments) = file.get("segments").and_then(Value::as_array) else {
        return Ok(());
    };
    for segment in segments.iter().filter_map(Value::as_array) {
        if segment.len() < 4 || !segment.get(3).and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let line = segment.first().and_then(parse_json_line);
        let hits = segment.get(2).and_then(parse_json_hits).unwrap_or(0);
        if let Some(line) = line {
            budget.insert_line(report, filename, line, hits)?;
        }
    }
    Ok(())
}

fn parse_line_number(value: &str) -> Option<u32> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .and_then(|line| u32::try_from(line).ok())
        .filter(|line| *line > 0)
}

fn parse_hit_count(value: &str) -> Option<u64> {
    if let Ok(hits) = value.trim().parse::<u64>() {
        return Some(hits);
    }
    value
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(truncated_nonnegative_u64)
}

fn parse_json_line(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|line| u32::try_from(line).ok())
        .filter(|line| *line > 0)
}

fn parse_json_hits(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().and_then(truncated_nonnegative_u64))
}

fn truncated_nonnegative_u64(value: f64) -> Option<u64> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    value.trunc().to_string().parse().ok()
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use reporigor_core::CoverageSpan;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use xml::attribute::OwnedAttribute;
use xml::reader::{ParserConfig, XmlEvent};

pub type LineCoverage = BTreeMap<u32, u64>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FunctionCoverage {
    pub anchor_line: u32,
    pub anchor_column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub lines: LineCoverage,
    pub spans: Vec<CoverageSpan>,
}

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

const COVERAGE_REPORT_NAMES: &str =
    "lcov.info|coverage-final.json|coverage.json|cobertura.xml|coverage.xml|llvm-cov.json|codecov.json";

/// Coverage interchange formats understood by the unified analyzer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
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
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Executable-line hit counts, keyed by normalized source path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    format: CoverageFormat,
    files: BTreeMap<String, LineCoverage>,
    functions: BTreeMap<String, Vec<FunctionCoverage>>,
}

impl CoverageReport {
    #[must_use]
    pub fn new(format: CoverageFormat) -> Self {
        Self {
            format,
            files: BTreeMap::new(),
            functions: BTreeMap::new(),
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
    pub(crate) fn has_function_coverage(&self) -> bool {
        self.functions.values().any(|functions| !functions.is_empty())
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
        retain_max_hits(entry, hits);
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
        for (file, functions) in &other.functions {
            let merged = self.functions.entry(file.clone()).or_default();
            merged.extend(functions.iter().cloned());
            merged.sort_unstable();
            merged.dedup();
        }
    }

    /// Resolve a function/source path against report paths. Exact relative and
    /// absolute matches win. A suffix or basename fallback is accepted only
    /// when exactly one report path matches.
    #[must_use]
    pub fn lines_for_file<'a>(&'a self, root: &Path, file: &str) -> Option<&'a LineCoverage> {
        value_for_file(&self.files, root, file)
    }

    #[must_use]
    pub(crate) fn functions_for_file<'a>(
        &'a self,
        root: &Path,
        file: &str,
    ) -> Option<&'a [FunctionCoverage]> {
        value_for_file(&self.functions, root, file).map(Vec::as_slice)
    }

    fn insert_function(&mut self, file: &str, function: FunctionCoverage) {
        let file = normalize_path(file);
        if file == "." || file.is_empty() {
            return;
        }
        self.functions.entry(file).or_default().push(function);
    }

    fn canonicalize_functions(&mut self) {
        for functions in self.functions.values_mut() {
            functions.sort_unstable();
            functions.dedup();
        }
    }
}

fn retain_max_hits(current: &mut u64, candidate: u64) {
    *current = (*current).max(candidate);
}

fn value_for_file<'a, T>(map: &'a BTreeMap<String, T>, root: &Path, file: &str) -> Option<&'a T> {
    let relative = normalize_path(file);
    if let Some(value) = map.get(&relative) {
        return Some(value);
    }

    let joined = normalized_join(root, &relative);
    if let Some(value) = map.get(&joined) {
        return Some(value);
    }

    let canonical = root
        .join(&relative)
        .canonicalize()
        .ok()
        .map(|path| normalize_path(&path.to_string_lossy()));
    if let Some(value) = canonical.as_ref().and_then(|path| map.get(path)) {
        return Some(value);
    }

    if let Some(value) = only_matching_value(map.iter().filter_map(|(candidate, value)| {
        (path_is_suffix(candidate, &relative) || path_is_suffix(&relative, candidate)).then_some(value)
    })) {
        return Some(value);
    }

    let basename = relative.rsplit('/').next().unwrap_or(relative.as_str());
    only_matching_value(
        map.iter().filter_map(|(candidate, value)| {
            (candidate.rsplit('/').next() == Some(basename)).then_some(value)
        }),
    )
}

fn only_matching_value<'a, T>(mut values: impl Iterator<Item = &'a T>) -> Option<&'a T> {
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
    let parts = split_path(&raw);
    let components = collapse_path_components(parts.remainder, parts.prefix.is_empty());
    let mut normalized = join_path_parts(parts.prefix, &components.join("/"));
    if parts.case_insensitive {
        normalized.make_ascii_lowercase();
    }
    normalized
}

#[derive(Debug, Clone, Copy)]
struct PathParts<'a> {
    prefix: &'a str,
    remainder: &'a str,
    case_insensitive: bool,
}

fn split_path(raw: &str) -> PathParts<'_> {
    if raw.as_bytes().get(1) == Some(&b':') {
        return PathParts {
            prefix: &raw[..2],
            remainder: raw[2..].trim_start_matches('/'),
            case_insensitive: true,
        };
    }
    if raw.starts_with("//") {
        return PathParts {
            prefix: "//",
            remainder: raw.trim_start_matches('/'),
            case_insensitive: true,
        };
    }
    if raw.starts_with('/') {
        return PathParts {
            prefix: "/",
            remainder: raw.trim_start_matches('/'),
            case_insensitive: false,
        };
    }
    PathParts {
        prefix: "",
        remainder: raw,
        case_insensitive: false,
    }
}

fn collapse_path_components(remainder: &str, relative: bool) -> Vec<&str> {
    let mut components = Vec::new();
    for component in remainder.split('/') {
        append_path_component(&mut components, component, relative);
    }
    components
}

fn append_path_component<'a>(components: &mut Vec<&'a str>, component: &'a str, relative: bool) {
    match component {
        "" | "." => {}
        ".." => resolve_parent_component(components, component, relative),
        _ => components.push(component),
    }
}

fn resolve_parent_component<'a>(components: &mut Vec<&'a str>, component: &'a str, relative: bool) {
    if components.last().is_some_and(|last| *last != "..") {
        components.pop();
    } else if relative {
        components.push(component);
    }
}

fn join_path_parts(prefix: &str, body: &str) -> String {
    if prefix.is_empty() {
        return relative_path_body(body);
    }
    if is_root_prefix(prefix) {
        return format!("{prefix}{body}");
    }
    drive_path_body(prefix, body)
}

fn relative_path_body(body: &str) -> String {
    if body.is_empty() { "." } else { body }.to_owned()
}

fn is_root_prefix(prefix: &str) -> bool {
    prefix == "/" || prefix == "//"
}

fn drive_path_body(prefix: &str, body: &str) -> String {
    if body.is_empty() {
        format!("{prefix}/")
    } else {
        format!("{prefix}/{body}")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoverageError {
    #[error("cannot read coverage report {path}: {source}")]
    Read {
        #[source]
        source: std::io::Error,
        path: String,
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
    let candidate = canonical_discovery_candidate(entry_path)?;
    ensure_candidate_within_root(&candidate, discovery_root)?;
    let size = validated_candidate_size(&candidate)?;
    add_candidate_bytes(candidate_bytes, size)?;
    Ok(candidate)
}

fn canonical_discovery_candidate(path: &Path) -> Result<PathBuf, CoverageError> {
    canonicalize_coverage_path(path)
}

fn canonicalize_coverage_path(path: &Path) -> Result<PathBuf, CoverageError> {
    path.canonicalize()
        .map_err(|source| coverage_read_error(path, source))
}

fn validated_candidate_size(candidate: &Path) -> Result<u64, CoverageError> {
    let metadata =
        fs::symlink_metadata(candidate).map_err(|source| coverage_read_error(candidate, source))?;
    ensure_regular_non_symlink(candidate, &metadata)?;
    ensure_report_size(metadata.len())?;
    Ok(metadata.len())
}

fn ensure_candidate_within_root(candidate: &Path, root: &Path) -> Result<(), CoverageError> {
    if candidate.starts_with(root) {
        return Ok(());
    }
    Err(unsafe_path(
        candidate,
        "discovered report escapes the requested directory",
    ))
}

fn ensure_regular_non_symlink(path: &Path, metadata: &fs::Metadata) -> Result<(), CoverageError> {
    if metadata.is_file() && !metadata.file_type().is_symlink() {
        return Ok(());
    }
    Err(unsafe_path(path, "expected a regular non-symlink file"))
}

fn add_candidate_bytes(total: &mut u64, amount: u64) -> Result<(), CoverageError> {
    *total = total.checked_add(amount).ok_or_else(candidate_bytes_limit)?;
    ensure_candidate_byte_limit(*total)
}

fn candidate_bytes_limit() -> CoverageError {
    CoverageError::ResourceLimit {
        resource: "coverage discovery candidate bytes",
        limit: MAX_COVERAGE_DISCOVERY_BYTES,
    }
}

fn ensure_candidate_byte_limit(total: u64) -> Result<(), CoverageError> {
    if total <= MAX_COVERAGE_DISCOVERY_BYTES {
        return Ok(());
    }
    Err(candidate_bytes_limit())
}

/// Locate a conventional report below a supplied file or directory.
///
/// # Errors
///
/// Returns an error when the path does not exist, a directory cannot be read,
/// or no file with a supported conventional report name can be found.
pub fn discover_coverage_report(path: &Path) -> Result<PathBuf, CoverageError> {
    let supplied_metadata = supplied_metadata(path)?;
    match classify_coverage_path(path, &supplied_metadata)? {
        CoveragePath::Report(report) => Ok(report),
        CoveragePath::Directory => CoverageDiscovery::new(path)?.run(),
    }
}

fn supplied_metadata(path: &Path) -> Result<fs::Metadata, CoverageError> {
    fs::symlink_metadata(path).map_err(|source| supplied_metadata_error(path, source))
}

fn supplied_metadata_error(path: &Path, source: std::io::Error) -> CoverageError {
    if source.kind() == std::io::ErrorKind::NotFound {
        CoverageError::Missing(path.display().to_string())
    } else {
        CoverageError::Read {
            path: path.display().to_string(),
            source,
        }
    }
}

enum CoveragePath {
    Report(PathBuf),
    Directory,
}

fn classify_coverage_path(path: &Path, metadata: &fs::Metadata) -> Result<CoveragePath, CoverageError> {
    if metadata.file_type().is_symlink() {
        return Err(unsafe_path(path, "symbolic links are not accepted"));
    }
    if metadata.is_file() {
        ensure_report_size(metadata.len())?;
        return Ok(CoveragePath::Report(path.to_path_buf()));
    }
    if metadata.is_dir() {
        return Ok(CoveragePath::Directory);
    }
    Err(unsafe_path(path, "expected a regular file or directory"))
}

struct CoverageDiscovery {
    root: PathBuf,
    pending: Vec<PathBuf>,
    candidates: Vec<PathBuf>,
    entry_count: usize,
    directory_count: usize,
    candidate_bytes: u64,
}

impl CoverageDiscovery {
    fn new(path: &Path) -> Result<Self, CoverageError> {
        let root = canonicalize_coverage_path(path)?;
        Ok(Self {
            pending: vec![root.clone()],
            root,
            candidates: Vec::new(),
            entry_count: 0,
            directory_count: 1,
            candidate_bytes: 0,
        })
    }

    fn run(mut self) -> Result<PathBuf, CoverageError> {
        while let Some(directory) = self.pending.pop() {
            self.scan_directory(&directory)?;
        }
        self.candidates.sort_by(candidate_order);
        self.candidates
            .into_iter()
            .next()
            .ok_or_else(|| CoverageError::NotFound(self.root.display().to_string()))
    }

    fn scan_directory(&mut self, directory: &Path) -> Result<(), CoverageError> {
        let entries = fs::read_dir(directory).map_err(|source| coverage_read_error(directory, source))?;
        for entry in entries {
            self.visit_entry(directory, entry)?;
        }
        Ok(())
    }

    fn visit_entry(
        &mut self,
        directory: &Path,
        entry: Result<fs::DirEntry, std::io::Error>,
    ) -> Result<(), CoverageError> {
        self.entry_count = self.entry_count.saturating_add(1);
        ensure_count(
            self.entry_count,
            MAX_COVERAGE_DISCOVERY_ENTRIES,
            "coverage discovery entries",
        )?;
        let (entry, file_type) = resolved_directory_entry(directory, entry)?;
        self.visit_resolved_entry(&entry, file_type)
    }

    fn visit_resolved_entry(
        &mut self,
        entry: &fs::DirEntry,
        file_type: fs::FileType,
    ) -> Result<(), CoverageError> {
        if file_type.is_dir() && !file_type.is_symlink() {
            return self.queue_directory(entry.path());
        }
        if is_coverage_candidate(entry, file_type) {
            self.retain_candidate(&entry.path())?;
        }
        Ok(())
    }

    fn queue_directory(&mut self, path: PathBuf) -> Result<(), CoverageError> {
        self.directory_count = self.directory_count.saturating_add(1);
        ensure_count(
            self.directory_count,
            MAX_COVERAGE_DISCOVERY_DIRECTORIES,
            "coverage discovery directories",
        )?;
        self.pending.push(path);
        Ok(())
    }

    fn retain_candidate(&mut self, path: &Path) -> Result<(), CoverageError> {
        let candidate = validate_discovered_candidate(path, &self.root, &mut self.candidate_bytes)?;
        ensure_count(
            self.candidates.len().saturating_add(1),
            MAX_COVERAGE_DISCOVERY_CANDIDATES,
            "coverage discovery candidates",
        )?;
        self.candidates.push(candidate);
        Ok(())
    }
}

fn resolved_directory_entry(
    directory: &Path,
    entry: Result<fs::DirEntry, std::io::Error>,
) -> Result<(fs::DirEntry, fs::FileType), CoverageError> {
    let entry = entry.map_err(|source| coverage_read_error(directory, source))?;
    let file_type = entry
        .file_type()
        .map_err(|source| coverage_read_error(&entry.path(), source))?;
    Ok((entry, file_type))
}

fn is_coverage_candidate(entry: &fs::DirEntry, file_type: fs::FileType) -> bool {
    file_type.is_file()
        && entry.file_name().to_str().is_some_and(|name| {
            COVERAGE_REPORT_NAMES
                .split('|')
                .any(|candidate| candidate == name)
        })
}

fn candidate_order(left: &PathBuf, right: &PathBuf) -> std::cmp::Ordering {
    left.components()
        .count()
        .cmp(&right.components().count())
        .then_with(|| left.cmp(right))
}

fn read_coverage_text(path: &Path) -> Result<String, CoverageError> {
    validate_coverage_path(path)?;
    let file = open_coverage_file(path)?;
    validate_open_coverage_file(&file, path)?;
    read_bounded_coverage(file, path)
}

fn validate_coverage_path(path: &Path) -> Result<(), CoverageError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| coverage_read_error(path, source))?;
    ensure_regular_non_symlink(path, &metadata)?;
    ensure_report_size(metadata.len())
}

fn validate_open_coverage_file(file: &fs::File, path: &Path) -> Result<(), CoverageError> {
    let opened_metadata = file
        .metadata()
        .map_err(|source| coverage_read_error(path, source))?;
    if !opened_metadata.is_file() {
        return Err(unsafe_path(path, "expected a regular file"));
    }
    ensure_report_size(opened_metadata.len())
}

fn open_coverage_file(path: &Path) -> Result<fs::File, CoverageError> {
    let mut options = fs::File::options();
    configure_coverage_open(&mut options);
    options
        .read(true)
        .open(path)
        .map_err(|source| coverage_read_error(path, source))
}

#[cfg(unix)]
fn configure_coverage_open(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn configure_coverage_open(_options: &mut fs::OpenOptions) {}

fn read_bounded_coverage(file: fs::File, path: &Path) -> Result<String, CoverageError> {
    let mut text = String::new();
    file.take(MAX_COVERAGE_REPORT_BYTES.saturating_add(1))
        .read_to_string(&mut text)
        .map_err(|source| coverage_read_error(path, source))?;
    ensure_report_size(u64::try_from(text.len()).unwrap_or(u64::MAX))?;
    Ok(text)
}

fn coverage_read_error(path: &Path, source: std::io::Error) -> CoverageError {
    CoverageError::Read {
        path: path.display().to_string(),
        source,
    }
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
    non_empty_report(report, &report_path)
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
    non_empty_report(report, path)
}

fn non_empty_report(report: CoverageReport, path: &Path) -> Result<CoverageReport, CoverageError> {
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
    if let Some(format) = format_from_extension(&extension) {
        return Ok(format);
    }
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if let Some(format) = format_from_prefix(trimmed) {
        return Ok(format);
    }
    format_from_json(path, trimmed)
}

fn format_from_extension(extension: &str) -> Option<CoverageFormat> {
    match extension {
        "info" | "lcov" => Some(CoverageFormat::Lcov),
        "xml" => Some(CoverageFormat::Cobertura),
        _ => None,
    }
}

fn format_from_prefix(text: &str) -> Option<CoverageFormat> {
    if text.starts_with("TN:") || text.starts_with("SF:") {
        return Some(CoverageFormat::Lcov);
    }
    text.starts_with('<').then_some(CoverageFormat::Cobertura)
}

fn format_from_json(path: &Path, text: &str) -> Result<CoverageFormat, CoverageError> {
    let value: Value = serde_json::from_str(text).map_err(|error| CoverageError::Parse {
        format: "json",
        message: error.to_string(),
    })?;
    let object = required_json_object(&value, "json")?;
    coverage_json_format(object).ok_or_else(|| CoverageError::Unsupported(path.display().to_string()))
}

fn required_json_object<'a>(
    value: &'a Value,
    format: &'static str,
) -> Result<&'a Map<String, Value>, CoverageError> {
    value.as_object().ok_or_else(|| CoverageError::Parse {
        format,
        message: "top-level value must be an object".to_owned(),
    })
}

fn coverage_json_format(object: &Map<String, Value>) -> Option<CoverageFormat> {
    if object.get("data").is_some_and(Value::is_array) {
        return Some(CoverageFormat::Llvm);
    }
    if object
        .get("files")
        .and_then(Value::as_object)
        .is_some_and(is_coverage_py_files)
    {
        return Some(CoverageFormat::CoveragePy);
    }
    object
        .values()
        .any(is_istanbul_file)
        .then_some(CoverageFormat::Istanbul)
}

fn is_coverage_py_files(files: &Map<String, Value>) -> bool {
    files.values().any(is_coverage_py_file)
}

fn is_coverage_py_file(file: &Value) -> bool {
    file.get("executed_lines").is_some() || file.get("missing_lines").is_some()
}

/// Parse coverage text using an explicitly selected interchange format.
///
/// # Errors
///
/// Returns an error when the selected format is not an input format or the
/// text does not satisfy that format's required structure.
pub fn parse_coverage(format: CoverageFormat, text: &str) -> Result<CoverageReport, CoverageError> {
    type Parser = fn(&str) -> Result<CoverageReport, CoverageError>;
    const PARSERS: [Parser; 5] = [
        parse_lcov,
        parse_cobertura,
        parse_coverage_py_json,
        parse_istanbul_json,
        parse_llvm_json,
    ];
    let parser = PARSERS.get(format as usize).ok_or_else(|| {
        CoverageError::Unsupported("merged is an output format, not an input format".to_owned())
    })?;
    parser(text)
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
        let Some(file) = bounded_source_path(file, line)? else {
            return Ok(());
        };
        ensure_new_file_capacity(report, &file)?;
        self.reserve_line(report, &file, line)?;
        let entry = report.files.entry(file).or_default().entry(line).or_default();
        retain_max_hits(entry, hits);
        Ok(())
    }

    fn reserve_line(&mut self, report: &CoverageReport, file: &str, line: u32) -> Result<(), CoverageError> {
        if !is_new_coverage_line(report, file, line) {
            return Ok(());
        }
        ensure_file_line_capacity(report, file)?;
        self.unique_lines = self.unique_lines.checked_add(1).ok_or_else(|| {
            resource_limit("unique executable coverage lines", MAX_COVERAGE_EXECUTABLE_LINES)
        })?;
        ensure_count(
            self.unique_lines,
            MAX_COVERAGE_EXECUTABLE_LINES,
            "unique executable coverage lines",
        )
    }
}

fn bounded_source_path(file: &str, line: u32) -> Result<Option<String>, CoverageError> {
    if line == 0 {
        return Ok(None);
    }
    ensure_count(file.len(), MAX_COVERAGE_PATH_BYTES, "coverage source path bytes")?;
    let file = normalize_path(file);
    Ok((file != "." && !file.is_empty()).then_some(file))
}

fn ensure_new_file_capacity(report: &CoverageReport, file: &str) -> Result<(), CoverageError> {
    if report.files.contains_key(file) {
        return Ok(());
    }
    ensure_count(
        report.files.len().saturating_add(1),
        MAX_COVERAGE_FILES,
        "normalized coverage files",
    )
}

fn is_new_coverage_line(report: &CoverageReport, file: &str, line: u32) -> bool {
    report
        .files
        .get(file)
        .is_none_or(|lines| !lines.contains_key(&line))
}

fn ensure_file_line_capacity(report: &CoverageReport, file: &str) -> Result<(), CoverageError> {
    let file_lines = report.files.get(file).map_or(0, BTreeMap::len);
    ensure_count(
        file_lines.saturating_add(1),
        MAX_COVERAGE_LINES_PER_FILE,
        "executable coverage lines per file",
    )
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
        consume_lcov_record(raw.trim_end_matches('\r'), &mut current, &mut report, &mut budget)?;
    }
    Ok(report)
}

fn consume_lcov_record(
    record: &str,
    current: &mut Option<String>,
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    if let Some(file) = record.strip_prefix("SF:") {
        let file = normalize_path(file);
        *current = (file != ".").then_some(file);
        return Ok(());
    }
    if record == "end_of_record" {
        *current = None;
        return Ok(());
    }
    append_lcov_data(record, current.as_deref(), report, budget)
}

fn append_lcov_data(
    record: &str,
    current: Option<&str>,
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    let (Some(file), Some(data)) = (current, record.strip_prefix("DA:")) else {
        return Ok(());
    };
    let mut fields = data.split(',');
    let line = fields.next().and_then(parse_line_number);
    let hits = fields.next().and_then(parse_hit_count);
    if let (Some(line), Some(hits)) = (line, hits) {
        budget.insert_line(report, file, line, hits)?;
    }
    Ok(())
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
    increment_class_line_records(class_line_records)?;
    append_current_xml_line(attributes, current)
}

fn append_current_xml_line(
    attributes: &[OwnedAttribute],
    current: &mut Option<(String, LineCoverage)>,
) -> Result<(), CoverageError> {
    let Some((_, lines)) = current.as_mut() else {
        return Ok(());
    };
    if let Some((line, hits)) = xml_line_data(attributes)? {
        insert_class_line(lines, line, hits)?;
    }
    Ok(())
}

fn increment_class_line_records(class_line_records: &mut usize) -> Result<(), CoverageError> {
    *class_line_records = class_line_records
        .checked_add(1)
        .ok_or_else(|| resource_limit("Cobertura class-line records", MAX_COBERTURA_CLASS_LINES))?;
    ensure_count(
        *class_line_records,
        MAX_COBERTURA_CLASS_LINES,
        "Cobertura class-line records",
    )
}

fn xml_line_data(attributes: &[OwnedAttribute]) -> Result<Option<(u32, u64)>, CoverageError> {
    let line = xml_attribute(attributes, "number")?.and_then(parse_line_number);
    let hits = xml_attribute(attributes, "hits")?
        .and_then(parse_hit_count)
        .unwrap_or(0);
    Ok(line.map(|line| (line, hits)))
}

fn insert_class_line(lines: &mut LineCoverage, line: u32, hits: u64) -> Result<(), CoverageError> {
    if !lines.contains_key(&line) {
        ensure_count(
            lines.len().saturating_add(1),
            MAX_COVERAGE_LINES_PER_FILE,
            "Cobertura lines per class",
        )?;
    }
    let entry = lines.entry(line).or_default();
    *entry = (*entry).max(hits);
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
    preflight_cobertura_resolution(&classes, sources)?;
    let mut report = CoverageReport::new(CoverageFormat::Cobertura);
    for (file, lines) in classes {
        insert_cobertura_class(&mut report, budget, sources, &file, &lines)?;
    }
    Ok(report)
}

fn preflight_cobertura_resolution(
    classes: &[(String, LineCoverage)],
    sources: &[String],
) -> Result<(), CoverageError> {
    let mut resolution_candidates = 0_usize;
    for (file, lines) in classes {
        let aliases = cobertura_alias_count(file, sources.len());
        add_resolution_candidates(&mut resolution_candidates, lines.len(), aliases)?;
    }
    Ok(())
}

fn cobertura_alias_count(file: &str, source_count: usize) -> usize {
    if absolute_like(file) {
        1
    } else {
        source_count.saturating_add(1)
    }
}

fn add_resolution_candidates(
    total: &mut usize,
    line_count: usize,
    aliases: usize,
) -> Result<(), CoverageError> {
    let limit = || {
        resource_limit(
            "Cobertura source-resolution candidates",
            MAX_COBERTURA_RESOLUTION_CANDIDATES,
        )
    };
    let class_candidates = line_count.checked_mul(aliases).ok_or_else(limit)?;
    *total = total.checked_add(class_candidates).ok_or_else(limit)?;
    ensure_count(
        *total,
        MAX_COBERTURA_RESOLUTION_CANDIDATES,
        "Cobertura source-resolution candidates",
    )
}

fn insert_cobertura_class(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    sources: &[String],
    file: &str,
    lines: &LineCoverage,
) -> Result<(), CoverageError> {
    for (&line, &hits) in lines {
        insert_cobertura_line(report, budget, sources, file, line, hits)?;
    }
    Ok(())
}

fn insert_cobertura_line(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    sources: &[String],
    file: &str,
    line: u32,
    hits: u64,
) -> Result<(), CoverageError> {
    budget.insert_line(report, file, line, hits)?;
    if absolute_like(file) {
        return Ok(());
    }
    for source in sources {
        let alias = limited_normalized_path(&format!("{source}/{file}"), "Cobertura resolved path bytes")?;
        budget.insert_line(report, &alias, line, hits)?;
    }
    Ok(())
}

fn find_xml_terminator(bytes: &[u8], from: usize, terminator: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(terminator.len())
        .position(|window| window == terminator)
        .map(|offset| from + offset + terminator.len())
}

fn xml_attribute_name_before_equals(bytes: &[u8], tag_start: usize, equals: usize) -> &[u8] {
    let name_floor = tag_start.saturating_add(1);
    let end = trimmed_xml_name_end(bytes, name_floor, equals);
    let start = xml_name_start(bytes, name_floor, end);
    &bytes[start..end]
}

fn trimmed_xml_name_end(bytes: &[u8], floor: usize, mut end: usize) -> usize {
    while end > floor && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    end
}

fn xml_name_start(bytes: &[u8], floor: usize, end: usize) -> usize {
    let mut start = end;
    while start > floor {
        let byte = bytes[start - 1];
        if byte.is_ascii_whitespace() || matches!(byte, b'<' | b'/' | b'?') {
            break;
        }
        start -= 1;
    }
    start
}

fn is_xml_namespace_declaration(name: &[u8]) -> bool {
    name == b"xmlns"
        || name
            .strip_prefix(b"xmlns:")
            .is_some_and(|local| !local.is_empty())
}

#[derive(Debug)]
struct XmlConstruct {
    prefix: &'static [u8],
    terminator: &'static [u8],
    limit: usize,
    resource: &'static str,
    unterminated: &'static str,
}

const XML_CONSTRUCTS: &[XmlConstruct] = &[
    XmlConstruct {
        prefix: b"<!--",
        terminator: b"-->",
        limit: MAX_COBERTURA_XML_VALUE_BYTES,
        resource: "Cobertura XML comment bytes",
        unterminated: "unterminated XML comment",
    },
    XmlConstruct {
        prefix: b"<![CDATA[",
        terminator: b"]]>",
        limit: MAX_COBERTURA_XML_VALUE_BYTES,
        resource: "Cobertura XML CDATA bytes",
        unterminated: "unterminated XML CDATA section",
    },
    XmlConstruct {
        prefix: b"<?",
        terminator: b"?>",
        limit: MAX_COBERTURA_XML_MARKUP_BYTES,
        resource: "Cobertura XML markup bytes",
        unterminated: "unterminated XML processing instruction",
    },
];

fn bounded_xml_construct_end(
    bytes: &[u8],
    start: usize,
    construct: &XmlConstruct,
) -> Result<usize, CoverageError> {
    let end = find_xml_terminator(bytes, start + construct.prefix.len(), construct.terminator).ok_or_else(
        || CoverageError::Parse {
            format: "cobertura",
            message: construct.unterminated.into(),
        },
    )?;
    ensure_count(end - start, construct.limit, construct.resource)?;
    Ok(end)
}

fn preflight_special_xml_markup(bytes: &[u8], start: usize) -> Result<Option<usize>, CoverageError> {
    if let Some(construct) = special_xml_construct(&bytes[start..]) {
        return bounded_xml_construct_end(bytes, start, construct).map(Some);
    }
    if bytes[start..].starts_with(b"<!") {
        return Err(CoverageError::Parse {
            format: "cobertura",
            message: "DTD and other XML declarations are not accepted in coverage reports".into(),
        });
    }
    Ok(None)
}

fn special_xml_construct(bytes: &[u8]) -> Option<&'static XmlConstruct> {
    XML_CONSTRUCTS
        .iter()
        .find(|construct| bytes.starts_with(construct.prefix))
}

fn preflight_cobertura_xml(text: &str) -> Result<(), CoverageError> {
    XmlPreflight::default().scan(text.as_bytes())
}

#[derive(Debug, Default)]
struct XmlPreflight {
    depth: usize,
    namespace_declarations: usize,
}

impl XmlPreflight {
    fn scan(mut self, bytes: &[u8]) -> Result<(), CoverageError> {
        let mut cursor = 0_usize;
        while let Some(start) = next_xml_tag(bytes, cursor) {
            cursor = self.consume_construct(bytes, start)?;
        }
        Ok(())
    }

    fn consume_construct(&mut self, bytes: &[u8], start: usize) -> Result<usize, CoverageError> {
        if let Some(end) = preflight_special_xml_markup(bytes, start)? {
            return Ok(end);
        }
        let end = scan_regular_xml_tag(bytes, start, &mut self.namespace_declarations)?;
        self.update_depth(bytes, start, end)?;
        Ok(end)
    }

    fn update_depth(&mut self, bytes: &[u8], start: usize, end: usize) -> Result<(), CoverageError> {
        if bytes.get(start + 1) == Some(&b'/') {
            self.depth = self.depth.saturating_sub(1);
            return Ok(());
        }
        if xml_tag_is_self_closing(bytes, start, end) {
            return Ok(());
        }
        self.depth = self.depth.saturating_add(1);
        ensure_count(self.depth, MAX_COBERTURA_XML_DEPTH, "Cobertura XML element depth")
    }
}

fn next_xml_tag(bytes: &[u8], cursor: usize) -> Option<usize> {
    bytes
        .get(cursor..)?
        .iter()
        .position(|byte| *byte == b'<')
        .map(|relative| cursor + relative)
}

fn scan_regular_xml_tag(
    bytes: &[u8],
    start: usize,
    namespace_declarations: &mut usize,
) -> Result<usize, CoverageError> {
    let mut scanner = XmlTagScanner::new(namespace_declarations);
    for (relative, byte) in bytes[start + 1..].iter().copied().enumerate() {
        let index = start + 1 + relative;
        if scanner.consume_byte(XmlByteInput {
            bytes,
            start,
            index,
            byte,
        })? {
            return finish_xml_tag(start, index + 1);
        }
    }
    Err(CoverageError::Parse {
        format: "cobertura",
        message: "unterminated XML tag".into(),
    })
}

struct XmlTagScanner<'a> {
    quote: Option<u8>,
    attributes: usize,
    namespace_declarations: &'a mut usize,
}

#[derive(Clone, Copy)]
struct XmlByteInput<'a> {
    bytes: &'a [u8],
    start: usize,
    index: usize,
    byte: u8,
}

impl<'a> XmlTagScanner<'a> {
    fn new(namespace_declarations: &'a mut usize) -> Self {
        Self {
            quote: None,
            attributes: 0,
            namespace_declarations,
        }
    }

    fn consume_byte(&mut self, input: XmlByteInput<'_>) -> Result<bool, CoverageError> {
        if self.consume_quoted_byte(input.byte) {
            return Ok(false);
        }
        self.consume_unquoted_byte(input)
    }

    fn consume_quoted_byte(&mut self, byte: u8) -> bool {
        let Some(delimiter) = self.quote else {
            return false;
        };
        if byte == delimiter {
            self.quote = None;
        }
        true
    }

    fn consume_unquoted_byte(&mut self, input: XmlByteInput<'_>) -> Result<bool, CoverageError> {
        match input.byte {
            b'\'' | b'"' => self.quote = Some(input.byte),
            b'=' => self.record_attribute(input.bytes, input.start, input.index)?,
            b'>' => return Ok(true),
            _ => {}
        }
        Ok(false)
    }

    fn record_attribute(&mut self, bytes: &[u8], start: usize, index: usize) -> Result<(), CoverageError> {
        self.attributes = self.attributes.saturating_add(1);
        ensure_count(
            self.attributes,
            MAX_COBERTURA_XML_ATTRIBUTES,
            "Cobertura XML attributes per element",
        )?;
        if is_xml_namespace_declaration(xml_attribute_name_before_equals(bytes, start, index)) {
            *self.namespace_declarations = self.namespace_declarations.saturating_add(1);
            ensure_count(
                *self.namespace_declarations,
                MAX_COBERTURA_XML_NAMESPACE_DECLARATIONS,
                "Cobertura XML namespace declarations",
            )?;
        }
        Ok(())
    }
}

fn finish_xml_tag(start: usize, end: usize) -> Result<usize, CoverageError> {
    ensure_count(
        end - start,
        MAX_COBERTURA_XML_MARKUP_BYTES,
        "Cobertura XML markup bytes",
    )?;
    Ok(end)
}

fn xml_tag_is_self_closing(bytes: &[u8], start: usize, end: usize) -> bool {
    bytes[start..end - 1]
        .iter()
        .rev()
        .find(|byte| !byte.is_ascii_whitespace())
        == Some(&b'/')
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
    let mut state = CoberturaState::default();
    consume_cobertura_events(reader, &mut state)?;
    state.into_report()
}

#[derive(Debug, Default)]
struct CoberturaState {
    budget: ParseBudget,
    sources: Vec<String>,
    current_source: Option<String>,
    current_class: Option<(String, LineCoverage)>,
    classes: Vec<(String, LineCoverage)>,
    class_records: usize,
    class_line_records: usize,
}

impl CoberturaState {
    fn consume_reader_event(
        &mut self,
        event: Result<XmlEvent, xml::reader::Error>,
    ) -> Result<bool, CoverageError> {
        self.budget.consume_records(1, "Cobertura XML events")?;
        let event = event.map_err(|error| CoverageError::Parse {
            format: "cobertura",
            message: error.to_string(),
        })?;
        self.handle_event(event)
    }

    fn handle_event(&mut self, event: XmlEvent) -> Result<bool, CoverageError> {
        match event {
            XmlEvent::StartElement { name, attributes, .. } => {
                self.handle_start(&name.local_name, &attributes).map(|()| false)
            }
            XmlEvent::Characters(value) | XmlEvent::CData(value) => {
                append_cobertura_source(&mut self.current_source, &value).map(|()| false)
            }
            XmlEvent::EndElement { name } => self.handle_end(&name.local_name).map(|()| false),
            XmlEvent::EndDocument => Ok(true),
            _ => Ok(false),
        }
    }

    fn handle_start(&mut self, local_name: &str, attributes: &[OwnedAttribute]) -> Result<(), CoverageError> {
        match local_name {
            "source" => {
                self.current_source = Some(String::new());
                Ok(())
            }
            "class" => self.start_class(attributes),
            "line" => append_xml_line(attributes, &mut self.current_class, &mut self.class_line_records),
            _ => Ok(()),
        }
    }

    fn start_class(&mut self, attributes: &[OwnedAttribute]) -> Result<(), CoverageError> {
        self.class_records = self.class_records.saturating_add(1);
        ensure_count(self.class_records, MAX_COBERTURA_CLASSES, "Cobertura classes")?;
        self.finish_class()?;
        self.current_class = xml_attribute(attributes, "filename")?
            .map(|file| {
                limited_normalized_path(file, "Cobertura class filename bytes")
                    .map(|file| (file, LineCoverage::new()))
            })
            .transpose()?;
        Ok(())
    }

    fn handle_end(&mut self, local_name: &str) -> Result<(), CoverageError> {
        match local_name {
            "source" => self.finish_source(),
            "class" => self.finish_class(),
            _ => Ok(()),
        }
    }

    fn finish_source(&mut self) -> Result<(), CoverageError> {
        let source = limited_normalized_path(
            self.current_source.take().as_deref().unwrap_or_default(),
            "Cobertura source path bytes",
        )?;
        if source == "." {
            return Ok(());
        }
        ensure_count(
            self.sources.len().saturating_add(1),
            MAX_COBERTURA_SOURCES,
            "Cobertura sources",
        )?;
        self.sources.push(source);
        Ok(())
    }

    fn finish_class(&mut self) -> Result<(), CoverageError> {
        if let Some(class) = self.current_class.take() {
            push_cobertura_class(&mut self.classes, class)?;
        }
        Ok(())
    }

    fn into_report(mut self) -> Result<CoverageReport, CoverageError> {
        self.finish_class()?;
        build_cobertura_report(self.classes, &self.sources, &mut self.budget)
    }
}

fn consume_cobertura_events(
    events: impl IntoIterator<Item = Result<XmlEvent, xml::reader::Error>>,
    state: &mut CoberturaState,
) -> Result<(), CoverageError> {
    for event in events {
        if state.consume_reader_event(event)? {
            break;
        }
    }
    Ok(())
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
    let files = coverage_py_files(&object)?;
    build_coverage_py_report(files)
}

fn coverage_py_files(object: &Value) -> Result<&Map<String, Value>, CoverageError> {
    object
        .get("files")
        .and_then(Value::as_object)
        .ok_or_else(|| CoverageError::Parse {
            format: "coverage.py json",
            message: "missing files object".to_owned(),
        })
}

fn build_coverage_py_report(files: &Map<String, Value>) -> Result<CoverageReport, CoverageError> {
    ensure_count(files.len(), MAX_COVERAGE_FILES, "coverage.py files")?;
    let mut report = CoverageReport::new(CoverageFormat::CoveragePy);
    let mut budget = ParseBudget::default();
    budget.consume_records(files.len(), "coverage.py records")?;
    for (file, value) in files {
        append_coverage_py_file(&mut report, &mut budget, file, value)?;
    }
    Ok(report)
}

fn append_coverage_py_file(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    file: &str,
    value: &Value,
) -> Result<(), CoverageError> {
    ensure_count(
        file.len(),
        MAX_COVERAGE_PATH_BYTES,
        "coverage.py source path bytes",
    )?;
    let Some(data) = value.as_object() else {
        return Ok(());
    };
    append_json_lines(report, budget, file, data, "executed_lines", 1)?;
    append_json_lines(report, budget, file, data, "missing_lines", 0)
}

fn append_json_lines(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    file: &str,
    data: &Map<String, Value>,
    key: &str,
    hits: u64,
) -> Result<(), CoverageError> {
    let Some(lines) = data.get(key).and_then(Value::as_array) else {
        return Ok(());
    };
    budget.consume_records(lines.len(), "coverage.py records")?;
    for line in lines.iter().filter_map(parse_json_line) {
        budget.insert_line(report, file, line, hits)?;
    }
    Ok(())
}

fn is_istanbul_file(value: &Value) -> bool {
    istanbul_file_parts(value).is_some()
}

type IstanbulFileParts<'a> = (
    &'a Map<String, Value>,
    &'a Map<String, Value>,
    &'a Map<String, Value>,
);

fn istanbul_file_parts(value: &Value) -> Option<IstanbulFileParts<'_>> {
    let data = value.as_object()?;
    let statements = data.get("statementMap").and_then(Value::as_object)?;
    let counts = data.get("s").and_then(Value::as_object)?;
    Some((data, statements, counts))
}

/// Parse an Istanbul `coverage-final.json` report.
///
/// # Errors
///
/// Returns an error when the input is not a JSON object.
pub fn parse_istanbul_json(text: &str) -> Result<CoverageReport, CoverageError> {
    let object = json_object(text, "istanbul json")?;
    let object = required_json_object(&object, "istanbul json")?;
    build_istanbul_report(object)
}

fn build_istanbul_report(object: &Map<String, Value>) -> Result<CoverageReport, CoverageError> {
    ensure_count(object.len(), MAX_COVERAGE_FILES, "Istanbul top-level files")?;
    let mut report = CoverageReport::new(CoverageFormat::Istanbul);
    let mut budget = ParseBudget::default();
    budget.consume_records(object.len(), "Istanbul records")?;
    for (key, value) in object {
        append_istanbul_file(&mut report, &mut budget, key, value)?;
    }
    Ok(report)
}

fn append_istanbul_file(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    key: &str,
    value: &Value,
) -> Result<(), CoverageError> {
    let Some((data, statements, counts)) = istanbul_file_parts(value) else {
        return Ok(());
    };
    consume_istanbul_records(budget, statements, counts)?;
    let file = data.get("path").and_then(Value::as_str).unwrap_or(key);
    for (id, location) in statements {
        append_istanbul_statement(report, budget, file, counts, id, location)?;
    }
    Ok(())
}

fn consume_istanbul_records(
    budget: &mut ParseBudget,
    statements: &Map<String, Value>,
    counts: &Map<String, Value>,
) -> Result<(), CoverageError> {
    budget.consume_records(statements.len(), "Istanbul records")?;
    budget.consume_records(counts.len(), "Istanbul records")
}

fn append_istanbul_statement(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    file: &str,
    counts: &Map<String, Value>,
    id: &str,
    location: &Value,
) -> Result<(), CoverageError> {
    let line = location
        .get("start")
        .and_then(|start| start.get("line"))
        .and_then(parse_json_line);
    let hits = counts.get(id).and_then(parse_json_hits).unwrap_or(0);
    if let Some(line) = line {
        budget.insert_line(report, file, line, hits)?;
    }
    Ok(())
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
        append_llvm_item(&mut report, item, &mut budget)?;
    }
    report.canonicalize_functions();
    Ok(report)
}

fn append_llvm_item(
    report: &mut CoverageReport,
    item: &Map<String, Value>,
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    let (functions, files) = llvm_item_parts(item);
    if let Some(functions) = functions {
        append_llvm_functions(report, functions, budget)?;
    }
    if let Some(files) = files {
        append_llvm_files(report, files, budget)?;
    }
    Ok(())
}

fn append_llvm_functions(
    report: &mut CoverageReport,
    functions: &[Value],
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    for function in functions.iter().filter_map(Value::as_object) {
        append_llvm_function(report, function, budget)?;
    }
    Ok(())
}

fn append_llvm_files(
    report: &mut CoverageReport,
    files: &[Value],
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    for file in files.iter().filter_map(Value::as_object) {
        append_llvm_segments(report, file, budget)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct LlvmRegion<'a> {
    start: u32,
    start_column: u32,
    end: u32,
    end_column: u32,
    hits: u64,
    file: &'a str,
}

fn llvm_code_region<'a>(region: &'a [Value], filenames: &'a [Value]) -> Option<LlvmRegion<'a>> {
    if !is_llvm_code_region(region) {
        return None;
    }
    let (start, start_column) = llvm_region_start(region)?;
    let (end, end_column) = llvm_region_end(region, start)?;
    let hits = region.get(4).and_then(parse_json_hits).unwrap_or(0);
    let file = llvm_region_file(region, filenames)?;
    Some(LlvmRegion {
        start,
        start_column,
        end,
        end_column,
        hits,
        file,
    })
}

fn is_llvm_code_region(region: &[Value]) -> bool {
    region.len() >= 6 && region.get(7).and_then(parse_json_hits).unwrap_or(0) == 0
}

fn llvm_region_start(region: &[Value]) -> Option<(u32, u32)> {
    let line = region.first().and_then(parse_json_line)?;
    let column = region.get(1).and_then(parse_json_column)?;
    Some((line, column))
}

fn llvm_region_end(region: &[Value], start: u32) -> Option<(u32, u32)> {
    let line = region
        .get(2)
        .and_then(parse_json_line)
        .unwrap_or(start)
        .max(start);
    let column = region.get(3).and_then(parse_json_column)?;
    Some((line, column))
}

fn llvm_region_file<'a>(region: &[Value], filenames: &'a [Value]) -> Option<&'a str> {
    let file_index = region
        .get(5)
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())?;
    filenames.get(file_index).and_then(Value::as_str)
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
    {
        let mut state = LlvmPreflight {
            budget: &mut budget,
            expanded_lines: &mut expanded_lines,
        };
        for item in data.iter().filter_map(Value::as_object) {
            preflight_llvm_item(item, &mut state)?;
        }
    }
    Ok(budget)
}

struct LlvmPreflight<'a> {
    budget: &'a mut ParseBudget,
    expanded_lines: &'a mut usize,
}

fn preflight_llvm_item(
    item: &Map<String, Value>,
    state: &mut LlvmPreflight<'_>,
) -> Result<(), CoverageError> {
    let (functions, files) = llvm_item_parts(item);
    functions.map_or(Ok(()), |functions| {
        preflight_llvm_functions(functions, state.budget, state.expanded_lines)
    })?;
    files.map_or(Ok(()), |files| preflight_llvm_files(files, state.budget))
}

fn llvm_item_parts(item: &Map<String, Value>) -> (Option<&[Value]>, Option<&[Value]>) {
    (
        item.get("functions").and_then(Value::as_array).map(Vec::as_slice),
        item.get("files").and_then(Value::as_array).map(Vec::as_slice),
    )
}

fn preflight_llvm_functions(
    functions: &[Value],
    budget: &mut ParseBudget,
    expanded_lines: &mut usize,
) -> Result<(), CoverageError> {
    budget.consume_records(functions.len(), "LLVM coverage records")?;
    for function in functions.iter().filter_map(Value::as_object) {
        preflight_llvm_function(function, budget, expanded_lines)?;
    }
    Ok(())
}

fn llvm_function_parts(function: &Map<String, Value>) -> Option<(&[Value], &[Value])> {
    let filenames = function.get("filenames").and_then(Value::as_array)?;
    let regions = function.get("regions").and_then(Value::as_array)?;
    Some((filenames, regions))
}

fn preflight_llvm_function(
    function: &Map<String, Value>,
    budget: &mut ParseBudget,
    expanded_lines: &mut usize,
) -> Result<(), CoverageError> {
    if let Some((filenames, regions)) = llvm_function_parts(function) {
        budget_llvm_function_arrays(budget, filenames, regions)?;
        for region in regions.iter().filter_map(Value::as_array) {
            preflight_llvm_region(region, filenames, budget, expanded_lines)?;
        }
    }
    Ok(())
}

fn budget_llvm_function_arrays(
    budget: &mut ParseBudget,
    filenames: &[Value],
    regions: &[Value],
) -> Result<(), CoverageError> {
    ensure_count(filenames.len(), MAX_COVERAGE_FILES, "LLVM filenames per function")?;
    budget.consume_records(filenames.len(), "LLVM coverage records")?;
    budget.consume_records(regions.len(), "LLVM coverage records")
}

fn preflight_llvm_region(
    raw: &[Value],
    filenames: &[Value],
    budget: &mut ParseBudget,
    expanded_lines: &mut usize,
) -> Result<(), CoverageError> {
    llvm_code_region(raw, filenames).map_or(Ok(()), |region| {
        preflight_llvm_code_region(region, budget, expanded_lines)
    })
}

fn preflight_llvm_code_region(
    region: LlvmRegion<'_>,
    budget: &mut ParseBudget,
    expanded_lines: &mut usize,
) -> Result<(), CoverageError> {
    ensure_count(
        region.file.len(),
        MAX_COVERAGE_PATH_BYTES,
        "LLVM source path bytes",
    )?;
    let span = llvm_region_span(region)?;
    add_expanded_llvm_lines(expanded_lines, span)?;
    budget.consume_records(span, "LLVM coverage records")
}

fn add_expanded_llvm_lines(total: &mut usize, span: usize) -> Result<(), CoverageError> {
    *total = total
        .checked_add(span)
        .ok_or_else(|| resource_limit("LLVM expanded executable lines", MAX_LLVM_EXPANDED_LINES))?;
    ensure_count(*total, MAX_LLVM_EXPANDED_LINES, "LLVM expanded executable lines")
}

fn preflight_llvm_files(files: &[Value], budget: &mut ParseBudget) -> Result<(), CoverageError> {
    budget.consume_records(files.len(), "LLVM coverage records")?;
    for file in files.iter().filter_map(Value::as_object) {
        preflight_llvm_file(file, budget)?;
    }
    Ok(())
}

fn llvm_file_parts(file: &Map<String, Value>) -> Option<(&str, &[Value])> {
    let filename = file.get("filename").and_then(Value::as_str)?;
    let segments = file.get("segments").and_then(Value::as_array)?;
    Some((filename, segments))
}

fn preflight_llvm_file(file: &Map<String, Value>, budget: &mut ParseBudget) -> Result<(), CoverageError> {
    if let Some((filename, segments)) = llvm_file_parts(file) {
        ensure_count(filename.len(), MAX_COVERAGE_PATH_BYTES, "LLVM source path bytes")?;
        budget.consume_records(segments.len(), "LLVM coverage records")?;
    }
    Ok(())
}

fn append_llvm_function(
    report: &mut CoverageReport,
    function: &Map<String, Value>,
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    let Some((filenames, regions)) = llvm_function_parts(function) else {
        return Ok(());
    };
    let mut grouped: BTreeMap<&str, Vec<LlvmRegion<'_>>> = BTreeMap::new();
    for region in regions.iter().filter_map(Value::as_array) {
        append_llvm_function_region(report, budget, &mut grouped, filenames, region)?;
    }
    for (file, regions) in grouped {
        insert_grouped_llvm_function(report, file, &regions);
    }
    Ok(())
}

fn insert_grouped_llvm_function(report: &mut CoverageReport, file: &str, regions: &[LlvmRegion<'_>]) {
    if let Some(coverage) = grouped_function_coverage(regions) {
        report.insert_function(file, coverage);
    }
}

fn append_llvm_function_region<'a>(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    grouped: &mut BTreeMap<&'a str, Vec<LlvmRegion<'a>>>,
    filenames: &'a [Value],
    raw: &'a [Value],
) -> Result<(), CoverageError> {
    let Some(region) = llvm_code_region(raw, filenames) else {
        return Ok(());
    };
    grouped.entry(region.file).or_default().push(region);
    for line in region.start..=region.end {
        budget.insert_line(report, region.file, line, region.hits)?;
    }
    Ok(())
}

fn grouped_function_coverage(regions: &[LlvmRegion<'_>]) -> Option<FunctionCoverage> {
    let anchor = regions
        .iter()
        .min_by_key(|region| (region.start, region.start_column, region.end, region.end_column))?;
    let end = regions
        .iter()
        .max_by_key(|region| (region.end, region.end_column, region.start, region.start_column))?;
    let (lines, spans) = llvm_function_lines_and_spans(regions);
    Some(FunctionCoverage {
        anchor_line: anchor.start,
        anchor_column: anchor.start_column,
        end_line: end.end,
        end_column: end.end_column,
        lines,
        spans,
    })
}

fn llvm_function_lines_and_spans(regions: &[LlvmRegion<'_>]) -> (LineCoverage, Vec<CoverageSpan>) {
    let mut lines = LineCoverage::new();
    let mut spans = Vec::with_capacity(regions.len());
    for region in regions {
        spans.push(llvm_region_coverage_span(*region));
        merge_llvm_region_lines(&mut lines, *region);
    }
    spans.sort_unstable();
    spans.dedup();
    (lines, spans)
}

fn llvm_region_coverage_span(region: LlvmRegion<'_>) -> CoverageSpan {
    CoverageSpan {
        start_line: region.start,
        start_column: region.start_column,
        end_line: region.end,
        end_column: region.end_column,
    }
}

fn merge_llvm_region_lines(lines: &mut LineCoverage, region: LlvmRegion<'_>) {
    for line in region.start..=region.end {
        let hits = lines.entry(line).or_default();
        *hits = (*hits).max(region.hits);
    }
}

fn append_llvm_segments(
    report: &mut CoverageReport,
    file: &Map<String, Value>,
    budget: &mut ParseBudget,
) -> Result<(), CoverageError> {
    if let Some((filename, segments)) = llvm_file_parts(file) {
        for segment in segments.iter().filter_map(Value::as_array) {
            append_llvm_segment(report, budget, filename, segment)?;
        }
    }
    Ok(())
}

fn append_llvm_segment(
    report: &mut CoverageReport,
    budget: &mut ParseBudget,
    filename: &str,
    segment: &[Value],
) -> Result<(), CoverageError> {
    if !is_executable_llvm_segment(segment) {
        return Ok(());
    }
    let line = segment.first().and_then(parse_json_line);
    let hits = segment.get(2).and_then(parse_json_hits).unwrap_or(0);
    if let Some(line) = line {
        budget.insert_line(report, filename, line, hits)?;
    }
    Ok(())
}

fn is_executable_llvm_segment(segment: &[Value]) -> bool {
    segment.len() >= 4 && segment.get(3).and_then(Value::as_bool).unwrap_or(false)
}

fn parse_line_number(value: &str) -> Option<u32> {
    value.trim().parse::<u64>().ok().and_then(positive_u32)
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
    value.as_u64().and_then(positive_u32)
}

fn parse_json_column(value: &Value) -> Option<u32> {
    value.as_u64().and_then(positive_u32)
}

fn positive_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok().filter(|value| *value > 0)
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

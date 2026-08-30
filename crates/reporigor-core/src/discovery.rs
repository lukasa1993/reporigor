use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::bounded_file::{canonical_directory, optional_symlink_metadata};
use crate::path_io::CoreIoResult;
use crate::{
    read_optional_bounded_utf8_file_within, AnalysisRequest, CoreError, Diagnostic, Language, ProjectContext,
    ProjectKind, Severity, SourceBudget, SourceFile,
};

const EXCLUDED_DIRS: &str =
    ".git|.hg|.idea|.pytest_cache|.tox|.venv|.build|build|coverage|dist|node_modules|target|vendor|venv|DerivedData|Pods";
const SHEBANG_PREFIX_BYTES: u64 = 4 * 1024;
const IGNORE_FILE_MAX_BYTES: u64 = 256 * 1024;
const IGNORE_FILE_MAX_LINES: usize = 8 * 1024;
const IGNORE_FILE_MAX_PATTERNS: usize = 4 * 1024;
const IGNORE_PATTERN_MAX_BYTES: usize = 8 * 1024;
const IGNORE_PROJECT_MAX_FILES: usize = 1024;
const IGNORE_PROJECT_MAX_BYTES: usize = 4 * 1024 * 1024;
const IGNORE_PROJECT_MAX_PATTERNS: usize = 32 * 1024;
const DISCOVERY_MAX_ENTRIES: usize = 1_000_000;
const DISCOVERY_MAX_DIRECTORY_ENTRIES: usize = 100_000;
const DISCOVERY_MAX_DIRECTORIES: usize = 100_000;
const DISCOVERY_MAX_DEPTH: usize = 256;

#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    pub languages: BTreeSet<Language>,
    pub filters: Vec<String>,
    pub include_tests: bool,
    pub max_source_bytes: usize,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            languages: BTreeSet::new(),
            filters: Vec::new(),
            include_tests: false,
            max_source_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Default)]
struct IgnoreBudget {
    files: usize,
    bytes: usize,
    patterns: usize,
}

#[derive(Debug, Clone, Default)]
struct IgnoreMatchers {
    dot_ignore: Vec<Arc<Gitignore>>,
    git_ignore: Vec<Arc<Gitignore>>,
    git_exclude: Option<Arc<Gitignore>>,
}

#[derive(Debug, Default)]
struct TraversalBudget {
    entries: usize,
    directories: usize,
}

fn reject_unsafe_path(condition: bool, path: &Path, message: impl Into<String>) -> Result<(), CoreError> {
    if condition {
        Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: message.into(),
        })
    } else {
        Ok(())
    }
}

impl TraversalBudget {
    fn observe_entry(&mut self, directory: &Path, directory_entries: usize) -> Result<(), CoreError> {
        if directory_entries > DISCOVERY_MAX_DIRECTORY_ENTRIES {
            return Err(discovery_limit_error(
                directory,
                &format!("directory contains more than {DISCOVERY_MAX_DIRECTORY_ENTRIES} entries"),
            ));
        }
        let entries = self.entries.saturating_add(1);
        if entries > DISCOVERY_MAX_ENTRIES {
            return Err(discovery_limit_error(
                directory,
                &format!("traversal exceeds {DISCOVERY_MAX_ENTRIES} filesystem entries"),
            ));
        }
        self.entries = entries;
        Ok(())
    }

    fn register_directory(&mut self, path: &Path, depth: usize) -> Result<(), CoreError> {
        if depth > DISCOVERY_MAX_DEPTH {
            return Err(discovery_limit_error(
                path,
                &format!("directory depth exceeds {DISCOVERY_MAX_DEPTH}"),
            ));
        }
        let directories = self.directories.saturating_add(1);
        if directories > DISCOVERY_MAX_DIRECTORIES {
            return Err(discovery_limit_error(
                path,
                &format!("traversal exceeds {DISCOVERY_MAX_DIRECTORIES} directories"),
            ));
        }
        self.directories = directories;
        Ok(())
    }
}

#[derive(Debug)]
struct DirectoryTask {
    path: PathBuf,
    depth: usize,
    matchers: IgnoreMatchers,
}

#[must_use]
pub fn discover_project(root: &Path) -> BTreeSet<ProjectKind> {
    let mut kinds = BTreeSet::new();
    let markers: &[(ProjectKind, &[&str])] = &[
        (ProjectKind::Cargo, &["Cargo.toml"]),
        (
            ProjectKind::CompilationDatabase,
            &["compile_commands.json", "build/compile_commands.json"],
        ),
        (ProjectKind::TypeScript, &["tsconfig.json", "package.json"]),
        (ProjectKind::SwiftPackage, &["Package.swift"]),
        (
            ProjectKind::Python,
            &["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"],
        ),
    ];
    for (kind, candidates) in markers {
        if has_project_marker(root, candidates) {
            kinds.insert(*kind);
        }
    }
    if kinds.is_empty() {
        kinds.insert(ProjectKind::Generic);
    }
    kinds
}

fn has_project_marker(root: &Path, candidates: &[&str]) -> bool {
    candidates.iter().any(|name| root.join(name).is_file())
}

/// Discover supported source files under a project root.
///
/// # Errors
///
/// Returns an error when `root` is not a directory or filesystem traversal
/// fails.
pub fn discover_sources(root: &Path, options: &DiscoveryOptions) -> Result<Vec<SourceFile>, CoreError> {
    let canonical_root = canonical_directory(root)?;
    DiscoveryState::new(&canonical_root, options)?.run()
}

struct DiscoveryState<'a> {
    root: &'a Path,
    options: &'a DiscoveryOptions,
    sources: Vec<SourceFile>,
    source_budget: SourceBudget,
    ignore_budget: IgnoreBudget,
    traversal_budget: TraversalBudget,
    pending: Vec<DirectoryTask>,
}

struct InspectedEntry {
    path: PathBuf,
    relative: PathBuf,
    file_type: fs::FileType,
}

struct SelectedSource {
    relative: String,
    language: Language,
    generated: bool,
    test: bool,
}

impl<'a> DiscoveryState<'a> {
    fn new(root: &'a Path, options: &'a DiscoveryOptions) -> Result<Self, CoreError> {
        let source_budget = SourceBudget::new(options.max_source_bytes)?;
        let mut traversal_budget = TraversalBudget::default();
        traversal_budget.register_directory(root, 0)?;
        Ok(Self {
            root,
            options,
            sources: Vec::new(),
            source_budget,
            ignore_budget: IgnoreBudget::default(),
            traversal_budget,
            pending: vec![DirectoryTask {
                path: root.to_path_buf(),
                depth: 0,
                matchers: IgnoreMatchers::default(),
            }],
        })
    }

    fn run(mut self) -> Result<Vec<SourceFile>, CoreError> {
        while let Some(task) = self.pending.pop() {
            self.process_directory(task)?;
        }
        self.sources
            .sort_by(|left, right| left.relative.cmp(&right.relative));
        Ok(self.sources)
    }

    fn process_directory(&mut self, task: DirectoryTask) -> Result<(), CoreError> {
        let (matchers, directories) = self.collect_directory(task)?;
        self.enqueue_directories(directories, &matchers);
        Ok(())
    }

    fn collect_directory(
        &mut self,
        task: DirectoryTask,
    ) -> Result<(IgnoreMatchers, Vec<(PathBuf, usize)>), CoreError> {
        validate_directory_within(self.root, &task.path)?;
        let mut matchers = task.matchers;
        load_directory_ignores(self.root, &task.path, &mut matchers, &mut self.ignore_budget)?;
        let entries = read_directory(&task.path, &mut self.traversal_budget)?;
        let mut directories = Vec::new();
        for entry in entries {
            self.process_entry(&entry, task.depth, &matchers, &mut directories)?;
        }
        Ok((matchers, directories))
    }

    fn enqueue_directories(&mut self, directories: Vec<(PathBuf, usize)>, matchers: &IgnoreMatchers) {
        for (path, depth) in directories.into_iter().rev() {
            self.pending.push(DirectoryTask {
                path,
                depth,
                matchers: matchers.clone(),
            });
        }
    }

    fn process_entry(
        &mut self,
        entry: &fs::DirEntry,
        parent_depth: usize,
        matchers: &IgnoreMatchers,
        directories: &mut Vec<(PathBuf, usize)>,
    ) -> Result<(), CoreError> {
        let Some(entry) = inspect_entry(self.root, entry)? else {
            return Ok(());
        };
        if should_skip_entry(&entry, matchers) {
            return Ok(());
        }
        self.process_selected_entry(entry, parent_depth, directories)
    }

    fn process_selected_entry(
        &mut self,
        entry: InspectedEntry,
        parent_depth: usize,
        directories: &mut Vec<(PathBuf, usize)>,
    ) -> Result<(), CoreError> {
        if entry.file_type.is_dir() {
            let depth = parent_depth.saturating_add(1);
            self.traversal_budget.register_directory(&entry.path, depth)?;
            directories.push((entry.path, depth));
            return Ok(());
        }
        if entry.file_type.is_file() {
            self.process_source_file(&entry)?;
        }
        Ok(())
    }

    fn process_source_file(&mut self, entry: &InspectedEntry) -> Result<(), CoreError> {
        let (canonical_path, source_bytes) = validate_regular_file_within(self.root, &entry.path)?;
        let Some(selected) = selected_source(&canonical_path, &entry.relative, self.options)? else {
            return Ok(());
        };
        self.source_budget.observe(&canonical_path, source_bytes)?;
        self.sources.push(SourceFile {
            path: canonical_path,
            relative: selected.relative,
            language: selected.language,
            generated: selected.generated,
            test: selected.test,
        });
        Ok(())
    }
}

fn inspect_entry(root: &Path, entry: &fs::DirEntry) -> Result<Option<InspectedEntry>, CoreError> {
    let path = entry.path();
    let file_type = entry.file_type().for_read_path(&path)?;
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => return Ok(None),
    };
    Ok(Some(InspectedEntry {
        path,
        relative,
        file_type,
    }))
}

fn should_skip_entry(entry: &InspectedEntry, matchers: &IgnoreMatchers) -> bool {
    is_excluded_path(&entry.relative) || is_ignored(matchers, &entry.path, entry.file_type.is_dir())
}

fn selected_source(
    canonical_path: &Path,
    relative_path: &Path,
    options: &DiscoveryOptions,
) -> Result<Option<SelectedSource>, CoreError> {
    let Some(language) = source_language(canonical_path, &options.languages) else {
        return Ok(None);
    };
    let relative = relative_path
        .to_str()
        .ok_or_else(|| non_utf8_source_path_error(relative_path))?
        .replace('\\', "/");
    let test = language.is_test_path(&relative);
    if !source_is_selected(options, language, &relative, test) {
        return Ok(None);
    }
    Ok(Some(SelectedSource {
        relative,
        language,
        generated: is_generated_path(relative_path),
        test,
    }))
}

fn source_language(path: &Path, requested: &BTreeSet<Language>) -> Option<Language> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| language_from_extension(extension, requested))
        .or_else(|| detect_shebang_language(path))
}

fn source_is_selected(options: &DiscoveryOptions, language: Language, relative: &str, test: bool) -> bool {
    language_is_selected(&options.languages, language)
        && filters_select(&options.filters, relative)
        && (!test || options.include_tests)
}

fn language_is_selected(requested: &BTreeSet<Language>, language: Language) -> bool {
    requested.is_empty() || requested.contains(&language)
}

fn filters_select(filters: &[String], relative: &str) -> bool {
    filters.is_empty() || filters.iter().any(|item| relative.contains(item))
}

fn validate_directory_within(root: &Path, path: &Path) -> Result<(), CoreError> {
    validate_path_metadata(path, DiscoveryEntryKind::Directory)?;
    let canonical = canonical_path_within(root, path)?;
    validate_directory_alias(path, &canonical)
}

fn validate_directory_alias(path: &Path, canonical: &Path) -> Result<(), CoreError> {
    reject_unsafe_path(
        canonical != path,
        path,
        format!(
            "directory resolves through an unexpected alias to {}",
            canonical.display()
        ),
    )
}

fn validate_regular_file_within(root: &Path, path: &Path) -> Result<(PathBuf, u64), CoreError> {
    validate_path_metadata(path, DiscoveryEntryKind::Source)?;
    let canonical = canonical_path_within(root, path)?;
    let opened_metadata = fs::metadata(&canonical).for_read_path(path)?;
    validate_opened_source_metadata(path, &opened_metadata)?;
    Ok((canonical, opened_metadata.len()))
}

#[derive(Clone, Copy)]
enum DiscoveryEntryKind {
    Directory,
    Source,
}

fn validate_path_metadata(path: &Path, kind: DiscoveryEntryKind) -> Result<(), CoreError> {
    let metadata = fs::symlink_metadata(path).for_read_path(path)?;
    validate_discovery_metadata(path, &metadata, kind)
}

fn validate_discovery_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    kind: DiscoveryEntryKind,
) -> Result<(), CoreError> {
    let (valid, message) = match kind {
        DiscoveryEntryKind::Directory => (
            metadata.is_dir(),
            "expected a non-symlink directory during source discovery",
        ),
        DiscoveryEntryKind::Source => (metadata.is_file(), "expected a non-symlink regular source file"),
    };
    reject_unsafe_path(metadata.file_type().is_symlink() || !valid, path, message)
}

fn validate_opened_source_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), CoreError> {
    reject_unsafe_path(
        !metadata.is_file(),
        path,
        "source path no longer resolves to a regular file",
    )
}

fn canonical_path_within(root: &Path, path: &Path) -> Result<PathBuf, CoreError> {
    let canonical = path.canonicalize().for_read_path(path)?;
    reject_unsafe_path(
        !canonical.starts_with(root),
        path,
        format!(
            "resolved path {} escapes project root {}",
            canonical.display(),
            root.display()
        ),
    )?;
    Ok(canonical)
}

#[allow(clippy::unnecessary_debug_formatting)]
fn non_utf8_source_path_error(relative: &Path) -> CoreError {
    CoreError::UnsafePath {
        path: format!("{relative:?}"),
        message: "selected source path is not valid UTF-8".to_string(),
    }
}

fn read_directory(path: &Path, budget: &mut TraversalBudget) -> Result<Vec<fs::DirEntry>, CoreError> {
    let entries = fs::read_dir(path).for_read_path(path)?;
    let mut output = Vec::new();
    for entry in entries {
        let entry = entry.for_read_path(path)?;
        budget.observe_entry(path, output.len().saturating_add(1))?;
        output.push(entry);
    }
    output.sort_by_key(fs::DirEntry::file_name);
    Ok(output)
}

fn load_directory_ignores(
    root: &Path,
    directory: &Path,
    matchers: &mut IgnoreMatchers,
    budget: &mut IgnoreBudget,
) -> Result<(), CoreError> {
    // Repository-local exclude rules are lower precedence than .gitignore and
    // .ignore. Global and parent-repository excludes are deliberately omitted:
    // an analyzed checkout cannot safely authorize reads outside its root.
    let local_git_directory = has_local_git_directory(directory)?;
    let repository_boundary = directory == root || local_git_directory;
    reset_git_matchers_at_boundary(matchers, repository_boundary && directory != root);
    update_git_exclude(root, directory, local_git_directory, matchers, budget)?;
    push_ignore_matcher(
        root,
        &directory.join(".gitignore"),
        directory,
        budget,
        &mut matchers.git_ignore,
    )?;
    push_ignore_matcher(
        root,
        &directory.join(".ignore"),
        directory,
        budget,
        &mut matchers.dot_ignore,
    )
}

fn update_git_exclude(
    root: &Path,
    directory: &Path,
    local_git_directory: bool,
    matchers: &mut IgnoreMatchers,
    budget: &mut IgnoreBudget,
) -> Result<(), CoreError> {
    if local_git_directory {
        matchers.git_exclude =
            load_ignore_file(root, &directory.join(".git/info/exclude"), directory, budget)?.map(Arc::new);
    }
    Ok(())
}

fn reset_git_matchers_at_boundary(matchers: &mut IgnoreMatchers, reset: bool) {
    if reset {
        matchers.git_ignore.clear();
        matchers.git_exclude = None;
    }
}

fn push_ignore_matcher(
    root: &Path,
    path: &Path,
    matcher_root: &Path,
    budget: &mut IgnoreBudget,
    target: &mut Vec<Arc<Gitignore>>,
) -> Result<(), CoreError> {
    if let Some(matcher) = load_ignore_file(root, path, matcher_root, budget)? {
        target.push(Arc::new(matcher));
    }
    Ok(())
}

fn has_local_git_directory(directory: &Path) -> Result<bool, CoreError> {
    let path = directory.join(".git");
    Ok(optional_symlink_metadata(&path)?.is_some_and(|metadata| metadata.file_type().is_dir()))
}

fn load_ignore_file(
    root: &Path,
    path: &Path,
    matcher_root: &Path,
    budget: &mut IgnoreBudget,
) -> Result<Option<Gitignore>, CoreError> {
    let mut builder = GitignoreBuilder::new(matcher_root);
    if add_ignore_file(root, path, &mut builder, budget)? == 0 {
        return Ok(None);
    }
    map_ignore_builder_error(path, "failed to compile bounded ignore patterns", builder.build()).map(Some)
}

fn add_ignore_file(
    root: &Path,
    path: &Path,
    builder: &mut GitignoreBuilder,
    budget: &mut IgnoreBudget,
) -> Result<usize, CoreError> {
    let Some(contents) = read_optional_bounded_utf8_file_within(root, path, IGNORE_FILE_MAX_BYTES)? else {
        return Ok(0);
    };
    observe_ignore_file_budget(path, contents.len(), budget)?;
    let mut patterns = 0;
    for (index, line) in contents.lines().enumerate() {
        add_ignore_line(
            path,
            line,
            index.saturating_add(1),
            builder,
            budget,
            &mut patterns,
        )?;
    }
    Ok(patterns)
}

fn observe_ignore_file_budget(path: &Path, bytes: usize, budget: &mut IgnoreBudget) -> Result<(), CoreError> {
    let next_files = budget.files.saturating_add(1);
    let next_bytes = budget.bytes.saturating_add(bytes);
    if next_files > IGNORE_PROJECT_MAX_FILES || next_bytes > IGNORE_PROJECT_MAX_BYTES {
        return Err(ignore_parse_error(
            path,
            format!(
                "project ignore budget exceeds {IGNORE_PROJECT_MAX_FILES} files or {IGNORE_PROJECT_MAX_BYTES} bytes"
            ),
        ));
    }
    budget.files = next_files;
    budget.bytes = next_bytes;
    Ok(())
}

fn add_ignore_line(
    path: &Path,
    line: &str,
    line_number: usize,
    builder: &mut GitignoreBuilder,
    budget: &mut IgnoreBudget,
    patterns: &mut usize,
) -> Result<(), CoreError> {
    validate_ignore_line(path, line, line_number)?;
    if is_ignore_pattern(line) {
        observe_ignore_pattern(path, budget, patterns)?;
    }
    let context = format!("invalid ignore pattern on line {line_number}");
    map_ignore_builder_error(path, &context, builder.add_line(Some(path.to_path_buf()), line))?;
    Ok(())
}

fn map_ignore_builder_error<T>(
    path: &Path,
    context: &str,
    result: Result<T, ignore::Error>,
) -> Result<T, CoreError> {
    result.map_err(|error| ignore_parse_error(path, format!("{context}: {error}")))
}

fn validate_ignore_line(path: &Path, line: &str, line_number: usize) -> Result<(), CoreError> {
    if line_number > IGNORE_FILE_MAX_LINES {
        return Err(ignore_parse_error(
            path,
            format!("ignore file exceeds {IGNORE_FILE_MAX_LINES} lines"),
        ));
    }
    if line.len() > IGNORE_PATTERN_MAX_BYTES {
        return Err(ignore_parse_error(
            path,
            format!("ignore line {line_number} exceeds {IGNORE_PATTERN_MAX_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn observe_ignore_pattern(
    path: &Path,
    budget: &mut IgnoreBudget,
    patterns: &mut usize,
) -> Result<(), CoreError> {
    *patterns = patterns.saturating_add(1);
    if *patterns > IGNORE_FILE_MAX_PATTERNS {
        return Err(ignore_parse_error(
            path,
            format!("ignore file exceeds {IGNORE_FILE_MAX_PATTERNS} patterns"),
        ));
    }
    let next_patterns = budget.patterns.saturating_add(1);
    if next_patterns > IGNORE_PROJECT_MAX_PATTERNS {
        return Err(ignore_parse_error(
            path,
            format!("project ignore budget exceeds {IGNORE_PROJECT_MAX_PATTERNS} patterns"),
        ));
    }
    budget.patterns = next_patterns;
    Ok(())
}

fn is_ignore_pattern(line: &str) -> bool {
    let normalized = if line.ends_with("\\ ") {
        line
    } else {
        line.trim_end()
    };
    !normalized.is_empty() && !normalized.starts_with('#')
}

fn ignore_parse_error(path: &Path, message: String) -> CoreError {
    CoreError::Parse {
        path: path.display().to_string(),
        message,
    }
}

fn discovery_limit_error(path: &Path, message: &str) -> CoreError {
    CoreError::Backend {
        backend: "discovery".to_string(),
        message: format!("{}: {message}", path.display()),
    }
}

fn is_ignored(matchers: &IgnoreMatchers, path: &Path, is_directory: bool) -> bool {
    match_ignore_chain(&matchers.dot_ignore, path, is_directory)
        .or_else(|| match_ignore_chain(&matchers.git_ignore, path, is_directory))
        .or_else(|| {
            matchers
                .git_exclude
                .as_ref()
                .and_then(|matcher| match_ignore(matcher, path, is_directory))
        })
        .unwrap_or(false)
}

fn match_ignore_chain(matchers: &[Arc<Gitignore>], path: &Path, is_directory: bool) -> Option<bool> {
    for matcher in matchers.iter().rev() {
        if let Some(ignored) = match_ignore(matcher, path, is_directory) {
            return Some(ignored);
        }
    }
    None
}

fn match_ignore(matcher: &Gitignore, path: &Path, is_directory: bool) -> Option<bool> {
    let outcome = matcher.matched(path, is_directory);
    if outcome.is_ignore() {
        return Some(true);
    }
    if outcome.is_whitelist() {
        return Some(false);
    }
    None
}

fn is_excluded_path(relative: &Path) -> bool {
    relative.components().any(|part| {
        EXCLUDED_DIRS
            .split('|')
            .any(|excluded| part.as_os_str() == excluded)
    })
}

fn language_from_extension(extension: &str, requested: &BTreeSet<Language>) -> Option<Language> {
    if extension.trim_start_matches('.').eq_ignore_ascii_case("h") {
        let mut selected = requested
            .iter()
            .copied()
            .filter(|language| language.is_c_family());
        return match (selected.next(), selected.next()) {
            (Some(language), None) => Some(language),
            _ => Some(Language::C),
        };
    }
    Language::from_extension(extension)
}

fn detect_shebang_language(path: &Path) -> Option<Language> {
    let prefix_bytes = shebang_prefix_length(path)?;
    let bytes = read_shebang_prefix(path, prefix_bytes)?;
    shell_shebang_language(&bytes)
}

fn shebang_prefix_length(path: &Path) -> Option<u64> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() < 2 {
        return None;
    }
    Some(metadata.len().min(SHEBANG_PREFIX_BYTES))
}

fn read_shebang_prefix(path: &Path, prefix_bytes: u64) -> Option<Vec<u8>> {
    let capacity = usize::try_from(prefix_bytes).ok()?;
    let mut bytes = Vec::with_capacity(capacity);
    File::open(path)
        .ok()?
        .take(prefix_bytes)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(bytes)
}

fn shell_shebang_language(bytes: &[u8]) -> Option<Language> {
    let first_line = bytes.split(|byte| *byte == b'\n').next()?;
    is_shell_shebang(first_line).then_some(Language::Bash)
}

fn is_shell_shebang(first_line: &[u8]) -> bool {
    first_line.starts_with(b"#!")
        && (first_line.windows(4).any(|window| window == b"bash")
            || first_line.windows(3).any(|window| window == b"/sh"))
}

fn is_generated_path(relative: &Path) -> bool {
    relative.components().any(|part| {
        matches!(
            part.as_os_str().to_string_lossy().as_ref(),
            "generated" | "gen" | "DerivedSources"
        )
    })
}

impl ProjectContext {
    /// Resolve and inspect the project described by an analysis request.
    ///
    /// # Errors
    ///
    /// Returns an error when the root cannot be canonicalized or source
    /// discovery fails.
    pub fn discover(request: &AnalysisRequest) -> Result<Self, CoreError> {
        let canonical = canonical_directory(&request.root)?;
        let sources = discover_sources(
            &canonical,
            &DiscoveryOptions {
                languages: request.languages.clone(),
                filters: request.filters.clone(),
                include_tests: request.include_tests,
                max_source_bytes: request.max_source_bytes,
            },
        )?;
        let diagnostics = if sources.is_empty() {
            vec![Diagnostic::new(
                Severity::Warning,
                "discovery",
                "no supported source files discovered",
            )]
        } else {
            Vec::new()
        };
        Ok(Self {
            kinds: discover_project(&canonical),
            root: canonical,
            sources,
            backends: Vec::new(),
            diagnostics,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::symlink},
    };

    use super::*;

    fn expect_core_error<T>(result: Result<T, CoreError>) -> CoreError {
        match result {
            Ok(_) => panic!("operation unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    fn discovery_error(root: &Path) -> CoreError {
        expect_core_error(discover_sources(root, &DiscoveryOptions::default()))
    }

    fn write_fixture_files(root: &Path, files: &[(&str, &str)]) -> Result<(), Box<dyn std::error::Error>> {
        for (relative, contents) in files {
            fs::write(root.join(relative), contents)?;
        }
        Ok(())
    }

    fn populated_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
        let fixture = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        for (relative, contents) in files {
            let path = fixture.path().join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap_or_else(|error| panic!("fixture directory: {error}"));
            }
            fs::write(path, contents).unwrap_or_else(|error| panic!("write fixture: {error}"));
        }
        fixture
    }

    fn discovered_fixture_sources(root: &Path) -> Vec<SourceFile> {
        discover_sources(root, &DiscoveryOptions::default())
            .unwrap_or_else(|error| panic!("discover fixture: {error}"))
    }

    fn assert_single_source(root: &Path, expected: &str) -> Result<(), Box<dyn std::error::Error>> {
        let sources = discover_sources(root, &DiscoveryOptions::default())?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].relative, expected);
        Ok(())
    }

    fn write_repository_fixture(
        root: &Path,
        directories: &str,
        rules: &[(&str, &str)],
        sources: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for relative in directories.split('|') {
            fs::create_dir_all(root.join(relative))?;
        }
        write_fixture_files(root, rules)?;
        for relative in sources.split('|') {
            fs::write(root.join(relative), "value = 1\n")?;
        }
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum RepositoryFixtureCase {
        IgnorePrecedence,
        NestedRepository,
    }

    fn repository_fixture(
        case: RepositoryFixtureCase,
    ) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        let directories = match case {
            RepositoryFixtureCase::IgnorePrecedence => ".git/info|nested|ignored-dir",
            RepositoryFixtureCase::NestedRepository => "nested/.git",
        };
        let sources = match case {
            RepositoryFixtureCase::IgnorePrecedence => {
                "drop.py|keep.py|from-exclude.py|from-ignore.py|nested/drop.py|nested/nested-keep.py|nested/root-ignore-wins.py|ignored-dir/resurrect.py"
            }
            RepositoryFixtureCase::NestedRepository => "root.py|nested/selected.py|nested/custom.py",
        };
        let rules: &[(&str, &str)] = match case {
            RepositoryFixtureCase::IgnorePrecedence => &[
                (".git/info/exclude", "from-exclude.py\n"),
                (".gitignore", "*.py\n!keep.py\n!from-exclude.py\nignored-dir/\n"),
                (".ignore", "!from-ignore.py\nnested/root-ignore-wins.py\n"),
                ("nested/.gitignore", "!nested-keep.py\n!root-ignore-wins.py\n"),
            ],
            RepositoryFixtureCase::NestedRepository => {
                &[(".gitignore", "*.py\n"), (".ignore", "nested/custom.py\n")]
            }
        };
        let fixture = tempfile::tempdir()?;
        write_repository_fixture(fixture.path(), directories, rules, sources)?;
        Ok(fixture)
    }

    #[cfg(unix)]
    fn write_non_utf8_named_source(
        root: &Path,
        filename: &std::ffi::OsString,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match fs::write(root.join(filename), "value = 1\n") {
            Ok(()) => Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(unix)]
    fn assert_non_utf8_directory_rejected() -> Result<(), Box<dyn std::error::Error>> {
        populated_non_utf8_directory().map(|fixture| {
            let error = discovery_error(fixture.path());
            assert!(matches!(error, CoreError::UnsafePath { .. }));
        })
    }

    #[cfg(unix)]
    fn populated_non_utf8_directory() -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        let fixture = tempfile::tempdir()?;
        populate_non_utf8_directory(fixture.path()).map(|()| fixture)
    }

    #[cfg(unix)]
    fn populate_non_utf8_directory(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let dirname = std::ffi::OsString::from_vec(b"package-\xff".to_vec());
        let directory = root.join(dirname);
        fs::create_dir(&directory)?;
        fs::write(directory.join("source.py"), "value = 1\n")
            .map_err(|error| Box::new(error) as Box<dyn std::error::Error>)
    }

    #[cfg(unix)]
    fn create_directory_symlink_fixture(
        root: &Path,
        outside: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        fs::write(root.join("inside.py"), "value = 1\n")?;
        fs::write(outside.join("outside.py"), "value = 1\n")?;
        symlink(outside, root.join("linked"))?;
        Ok(())
    }

    fn assert_sparse_ignore_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let sparse = tempfile::tempdir()?;
        File::create(sparse.path().join(".gitignore"))?.set_len(IGNORE_FILE_MAX_BYTES + 1)?;
        assert!(matches!(
            discover_sources(sparse.path(), &DiscoveryOptions::default()),
            Err(CoreError::FileTooLarge { .. })
        ));
        Ok(())
    }

    fn assert_invalid_ignore(contents: String, expected: &str) -> Result<(), Box<dyn std::error::Error>> {
        let fixture = tempfile::tempdir()?;
        fs::write(fixture.path().join(".gitignore"), contents)?;
        let error = discovery_error(fixture.path());
        assert!(error.to_string().contains(expected));
        Ok(())
    }

    fn assert_excessive_ignore_lines_rejected() -> Result<(), Box<dyn std::error::Error>> {
        assert_invalid_ignore("\n".repeat(IGNORE_FILE_MAX_LINES + 1), "exceeds 8192 lines")
    }

    fn assert_excessive_ignore_patterns_rejected() -> Result<(), Box<dyn std::error::Error>> {
        assert_invalid_ignore(
            "ignored\n".repeat(IGNORE_FILE_MAX_PATTERNS + 1),
            "exceeds 4096 patterns",
        )
    }

    #[test]
    fn language_detection_handles_all_supported_extensions() {
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("mm"), Some(Language::ObjectiveC));
        assert_eq!(Language::from_extension("unknown"), None);
    }

    #[test]
    fn project_context_reports_empty_roots_and_discovers_populated_projects() {
        let fixture = tempfile::tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        let request = AnalysisRequest::new(fixture.path().to_path_buf());
        let empty = ProjectContext::discover(&request)
            .unwrap_or_else(|error| panic!("empty context discovery: {error}"));
        assert!(empty.sources.is_empty());
        assert_eq!(empty.diagnostics.len(), 1);
        assert!(empty.kinds.contains(&ProjectKind::Generic));

        write_fixture_files(
            fixture.path(),
            &[(
                "Cargo.toml",
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n",
            )],
        )
        .unwrap_or_else(|error| panic!("manifest fixture: {error}"));
        write_fixture_files(fixture.path(), &[("lib.rs", "pub fn run() {}\n")])
            .unwrap_or_else(|error| panic!("source fixture: {error}"));
        let populated = ProjectContext::discover(&request)
            .unwrap_or_else(|error| panic!("populated context discovery: {error}"));
        assert_eq!(populated.sources.len(), 1);
        assert!(populated.diagnostics.is_empty());
        assert!(populated.kinds.contains(&ProjectKind::Cargo));
    }

    #[test]
    fn explicit_c_family_language_owns_ambiguous_headers() {
        assert_eq!(
            language_from_extension("h", &BTreeSet::from([Language::Cpp])),
            Some(Language::Cpp)
        );
        assert_eq!(
            language_from_extension(".H", &BTreeSet::from([Language::ObjectiveC])),
            Some(Language::ObjectiveC)
        );
        assert_eq!(
            language_from_extension("h", &BTreeSet::from([Language::Python, Language::Cpp])),
            Some(Language::Cpp)
        );
    }

    #[test]
    fn ambiguous_or_unfiltered_headers_keep_the_c_default() {
        assert_eq!(language_from_extension("h", &BTreeSet::new()), Some(Language::C));
        assert_eq!(
            language_from_extension("h", &BTreeSet::from([Language::C, Language::Cpp])),
            Some(Language::C)
        );
        assert_eq!(
            language_from_extension("h", &BTreeSet::from([Language::Cpp, Language::ObjectiveC])),
            Some(Language::C)
        );
    }

    #[test]
    fn explicit_cpp_and_objective_c_discovery_include_headers() {
        let temp = populated_fixture(&[("shared.h", "int shared(void);\n")]);

        for language in [Language::Cpp, Language::ObjectiveC] {
            let files = discover_sources(
                temp.path(),
                &DiscoveryOptions {
                    languages: BTreeSet::from([language]),
                    ..DiscoveryOptions::default()
                },
            )
            .unwrap_or_else(|error| panic!("discover fixture: {error}"));
            assert_eq!(files.len(), 1, "{language}");
            assert_eq!(files[0].language, language);
            assert_eq!(files[0].relative, "shared.h");
        }
    }

    #[test]
    fn discovers_extensionless_bash_and_skips_tests() {
        let temp = populated_fixture(&[
            ("tool", "#!/usr/bin/env bash\necho ok\n"),
            ("tests/test_tool.sh", "echo test\n"),
        ]);
        let files = discovered_fixture_sources(temp.path());
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].language, Language::Bash);
    }

    #[test]
    fn rejects_a_huge_sparse_shebang_without_reading_the_file() -> Result<(), Box<dyn std::error::Error>> {
        const SPARSE_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

        use std::io::Write;

        let temp = tempfile::tempdir()?;
        let path = temp.path().join("huge-tool");
        let mut file = File::create(&path)?;
        file.write_all(b"#!/usr/bin/env bash\n")?;
        file.set_len(SPARSE_FILE_BYTES)?;
        drop(file);
        assert_eq!(fs::metadata(&path)?.len(), SPARSE_FILE_BYTES);

        let result = discover_sources(
            temp.path(),
            &DiscoveryOptions {
                languages: BTreeSet::from([Language::Bash]),
                ..DiscoveryOptions::default()
            },
        );

        assert!(matches!(result, Err(CoreError::SourceTooLarge { .. })));
        Ok(())
    }

    #[test]
    fn aggregate_sparse_source_metadata_is_bounded_before_parsing() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let per_file = u64::try_from(crate::MAX_SOURCE_BYTES_HARD_LIMIT).unwrap_or(u64::MAX);
        for index in 0..17 {
            let path = temp.path().join(format!("source-{index}.py"));
            File::create(path)?.set_len(per_file)?;
        }

        let result = discover_sources(
            temp.path(),
            &DiscoveryOptions {
                languages: BTreeSet::from([Language::Python]),
                max_source_bytes: crate::MAX_SOURCE_BYTES_HARD_LIMIT,
                ..DiscoveryOptions::default()
            },
        );
        assert!(matches!(
            result,
            Err(CoreError::SourceBudgetExceeded {
                selected_files: 17,
                selected_bytes,
                ..
            }) if selected_bytes > crate::MAX_SELECTED_SOURCE_BYTES
        ));
        Ok(())
    }

    #[test]
    fn bounded_ignore_rules_preserve_negation_depth_and_source_precedence() {
        let temp = repository_fixture(RepositoryFixtureCase::IgnorePrecedence)
            .unwrap_or_else(|error| panic!("ignore precedence fixture: {error}"));

        let sources = discovered_fixture_sources(temp.path());
        let names = sources
            .iter()
            .map(|source| source.relative.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "from-exclude.py",
                "from-ignore.py",
                "keep.py",
                "nested/nested-keep.py"
            ]
        );
    }

    #[test]
    fn parent_ignore_files_are_not_opened_or_applied() -> Result<(), Box<dyn std::error::Error>> {
        let parent = tempfile::tempdir()?;
        let root = parent.path().join("project");
        fs::create_dir(&root)?;
        fs::write(parent.path().join(".gitignore"), "*.py\n")?;
        fs::write(root.join("selected.py"), "value = 1\n")?;

        assert_single_source(&root, "selected.py")
    }

    #[test]
    fn nested_real_repository_resets_git_rules_but_not_custom_ignore_rules(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = repository_fixture(RepositoryFixtureCase::NestedRepository)?;

        assert_single_source(temp.path(), "nested/selected.py")
    }

    #[cfg(unix)]
    #[test]
    fn ignore_symlinks_to_devices_are_rejected_without_opening_them() -> Result<(), Box<dyn std::error::Error>>
    {
        for name in [".gitignore", ".ignore"] {
            let temp = tempfile::tempdir()?;
            symlink("/dev/zero", temp.path().join(name))?;
            fs::write(temp.path().join("selected.py"), "value = 1\n")?;
            let result = discover_sources(temp.path(), &DiscoveryOptions::default());
            assert!(matches!(result, Err(CoreError::UnsafePath { .. })), "{name}");
        }
        Ok(())
    }

    #[test]
    fn sparse_and_excessive_ignore_files_fail_before_unbounded_work() -> Result<(), Box<dyn std::error::Error>>
    {
        assert_sparse_ignore_rejected()?;
        assert_excessive_ignore_lines_rejected()?;
        assert_excessive_ignore_patterns_rejected()?;
        Ok(())
    }

    #[test]
    fn invalid_ignore_encoding_and_globs_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        let encoding = tempfile::tempdir()?;
        fs::write(encoding.path().join(".gitignore"), [0xff, b'\n'])?;
        assert!(matches!(
            discover_sources(encoding.path(), &DiscoveryOptions::default()),
            Err(CoreError::Parse { .. })
        ));

        let glob = tempfile::tempdir()?;
        fs::write(glob.path().join(".gitignore"), "[unterminated\n")?;
        assert!(matches!(
            discover_sources(glob.path(), &DiscoveryOptions::default()),
            Err(CoreError::Parse { .. })
        ));
        Ok(())
    }

    #[test]
    fn unsupported_fanout_and_directory_depth_have_immutable_limits() {
        let path = Path::new("unsupported-only");
        let mut fanout = TraversalBudget::default();
        let error = expect_core_error(fanout.observe_entry(path, DISCOVERY_MAX_DIRECTORY_ENTRIES + 1));
        assert!(error.to_string().contains("directory contains more than"));

        let mut total = TraversalBudget {
            entries: DISCOVERY_MAX_ENTRIES,
            directories: 0,
        };
        assert!(total.observe_entry(path, 1).is_err());

        let mut depth = TraversalBudget::default();
        assert!(depth.register_directory(path, DISCOVERY_MAX_DEPTH + 1).is_err());
        depth.directories = DISCOVERY_MAX_DIRECTORIES;
        assert!(depth.register_directory(path, 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_selected_file_and_directory_names_fail_without_lossy_aliases(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bad_file = tempfile::tempdir()?;
        let filename = OsString::from_vec(b"source-\xff.py".to_vec());
        let raw_relative = PathBuf::from(filename.clone());
        assert!(non_utf8_source_path_error(&raw_relative)
            .to_string()
            .contains("not valid UTF-8"));
        if !write_non_utf8_named_source(bad_file.path(), &filename)? {
            eprintln!("filesystem rejects non-UTF-8 names; path-level regression still ran");
            return Ok(());
        }
        let error = discovery_error(bad_file.path());
        assert!(matches!(error, CoreError::UnsafePath { .. }));
        assert!(error.to_string().contains("not valid UTF-8"));
        assert_non_utf8_directory_rejected()
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_never_followed() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        create_directory_symlink_fixture(root.path(), outside.path())?;
        assert_single_source(root.path(), "inside.py")
    }
}

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::{
    read_optional_bounded_utf8_file_within, AnalysisRequest, CoreError, Diagnostic, Language, ProjectContext,
    ProjectKind, Severity, SourceBudget, SourceFile,
};

const EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".idea",
    ".pytest_cache",
    ".tox",
    ".venv",
    ".build",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "vendor",
    "venv",
    "DerivedData",
    "Pods",
];
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
    if root.join("Cargo.toml").is_file() {
        kinds.insert(ProjectKind::Cargo);
    }
    if root.join("compile_commands.json").is_file() || root.join("build/compile_commands.json").is_file() {
        kinds.insert(ProjectKind::CompilationDatabase);
    }
    if root.join("tsconfig.json").is_file() || root.join("package.json").is_file() {
        kinds.insert(ProjectKind::TypeScript);
    }
    if root.join("Package.swift").is_file() {
        kinds.insert(ProjectKind::SwiftPackage);
    }
    if ["pyproject.toml", "setup.py", "setup.cfg", "requirements.txt"]
        .iter()
        .any(|name| root.join(name).is_file())
    {
        kinds.insert(ProjectKind::Python);
    }
    if kinds.is_empty() {
        kinds.insert(ProjectKind::Generic);
    }
    kinds
}

/// Discover supported source files under a project root.
///
/// # Errors
///
/// Returns an error when `root` is not a directory or filesystem traversal
/// fails.
pub fn discover_sources(root: &Path, options: &DiscoveryOptions) -> Result<Vec<SourceFile>, CoreError> {
    let canonical_root = canonical_discovery_root(root)?;
    let root = canonical_root.as_path();
    let mut sources = Vec::new();
    let mut source_budget = SourceBudget::new(options.max_source_bytes)?;
    let mut ignore_budget = IgnoreBudget::default();
    let mut traversal_budget = TraversalBudget::default();
    traversal_budget.register_directory(root, 0)?;
    let mut pending = vec![DirectoryTask {
        path: root.to_path_buf(),
        depth: 0,
        matchers: IgnoreMatchers::default(),
    }];

    while let Some(task) = pending.pop() {
        validate_directory_within(root, &task.path)?;
        let mut matchers = task.matchers;
        load_directory_ignores(root, &task.path, &mut matchers, &mut ignore_budget)?;

        let mut directories = Vec::new();
        for entry in read_directory(&task.path, &mut traversal_budget)? {
            let path = entry.path();
            let file_type = entry.file_type().map_err(|source| CoreError::Read {
                path: path.display().to_string(),
                source,
            })?;
            let Ok(relative_path) = path.strip_prefix(root) else {
                continue;
            };
            if is_excluded_path(relative_path) {
                continue;
            }
            let is_directory = file_type.is_dir();
            if is_ignored(&matchers, &path, is_directory) {
                continue;
            }
            if is_directory {
                let depth = task.depth.saturating_add(1);
                traversal_budget.register_directory(&path, depth)?;
                directories.push((path, depth));
                continue;
            }
            // Match WalkBuilder's default no-follow behavior. Known ignore
            // files were already inspected with the stricter bounded reader.
            if !file_type.is_file() {
                continue;
            }

            let (canonical_path, source_bytes) = validate_regular_file_within(root, &path)?;
            let language = canonical_path
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| language_from_extension(extension, &options.languages))
                .or_else(|| detect_shebang_language(&canonical_path));
            let Some(language) = language else {
                continue;
            };
            if !options.languages.is_empty() && !options.languages.contains(&language) {
                continue;
            }
            let relative = relative_path
                .to_str()
                .ok_or_else(|| non_utf8_source_path_error(relative_path))?
                .replace('\\', "/");
            if !options.filters.is_empty() && !options.filters.iter().any(|item| relative.contains(item)) {
                continue;
            }
            let test = language.is_test_path(&relative);
            if test && !options.include_tests {
                continue;
            }
            source_budget.observe(&canonical_path, source_bytes)?;
            let generated = is_generated_path(relative_path);
            sources.push(SourceFile {
                path: canonical_path,
                relative,
                language,
                generated,
                test,
            });
        }

        for (path, depth) in directories.into_iter().rev() {
            pending.push(DirectoryTask {
                path,
                depth,
                matchers: matchers.clone(),
            });
        }
    }
    sources.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(sources)
}

fn canonical_discovery_root(root: &Path) -> Result<PathBuf, CoreError> {
    let canonical = root.canonicalize().map_err(|source| CoreError::Read {
        path: root.display().to_string(),
        source,
    })?;
    if !fs::metadata(&canonical)
        .map_err(|source| CoreError::Read {
            path: root.display().to_string(),
            source,
        })?
        .is_dir()
    {
        return Err(CoreError::InvalidRoot {
            path: root.display().to_string(),
            message: "not a directory".to_string(),
        });
    }
    Ok(canonical)
}

fn validate_directory_within(root: &Path, path: &Path) -> Result<(), CoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: "expected a non-symlink directory during source discovery".to_string(),
        });
    }
    let canonical = canonical_path_within(root, path)?;
    if canonical != path {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: format!(
                "directory resolves through an unexpected alias to {}",
                canonical.display()
            ),
        });
    }
    Ok(())
}

fn validate_regular_file_within(root: &Path, path: &Path) -> Result<(PathBuf, u64), CoreError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: "expected a non-symlink regular source file".to_string(),
        });
    }
    let canonical = canonical_path_within(root, path)?;
    let opened_metadata = fs::metadata(&canonical).map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if !opened_metadata.is_file() {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: "source path no longer resolves to a regular file".to_string(),
        });
    }
    Ok((canonical, opened_metadata.len()))
}

fn canonical_path_within(root: &Path, path: &Path) -> Result<PathBuf, CoreError> {
    let canonical = path.canonicalize().map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if !canonical.starts_with(root) {
        return Err(CoreError::UnsafePath {
            path: path.display().to_string(),
            message: format!(
                "resolved path {} escapes project root {}",
                canonical.display(),
                root.display()
            ),
        });
    }
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
    let entries = fs::read_dir(path).map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mut output = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| CoreError::Read {
            path: path.display().to_string(),
            source,
        })?;
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
    if repository_boundary && directory != root {
        matchers.git_ignore.clear();
        matchers.git_exclude = None;
    }
    if local_git_directory {
        matchers.git_exclude =
            load_ignore_file(root, &directory.join(".git/info/exclude"), directory, budget)?.map(Arc::new);
    }
    if let Some(matcher) = load_ignore_file(root, &directory.join(".gitignore"), directory, budget)? {
        matchers.git_ignore.push(Arc::new(matcher));
    }
    // .ignore is a distinct higher-precedence class. A root .ignore match
    // overrides even a nested .gitignore match, matching the ignore crate.
    if let Some(matcher) = load_ignore_file(root, &directory.join(".ignore"), directory, budget)? {
        matchers.dot_ignore.push(Arc::new(matcher));
    }
    Ok(())
}

fn has_local_git_directory(directory: &Path) -> Result<bool, CoreError> {
    let path = directory.join(".git");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => Ok(metadata.file_type().is_dir()),
        Err(source) if source.kind() == ErrorKind::NotFound => Ok(false),
        Err(source) => Err(CoreError::Read {
            path: path.display().to_string(),
            source,
        }),
    }
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
    builder.build().map(Some).map_err(|error| CoreError::Parse {
        path: path.display().to_string(),
        message: format!("failed to compile bounded ignore patterns: {error}"),
    })
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

    let bytes = contents.len();
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

    let mut patterns: usize = 0;
    for (index, line) in contents.lines().enumerate() {
        let line_number = index.saturating_add(1);
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
        if is_ignore_pattern(line) {
            patterns = patterns.saturating_add(1);
            if patterns > IGNORE_FILE_MAX_PATTERNS {
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
        }
        builder
            .add_line(Some(path.to_path_buf()), line)
            .map_err(|error| CoreError::Parse {
                path: path.display().to_string(),
                message: format!("invalid ignore pattern on line {line_number}: {error}"),
            })?;
    }
    Ok(patterns)
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
    relative
        .components()
        .any(|part| EXCLUDED_DIRS.iter().any(|excluded| part.as_os_str() == *excluded))
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
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() < 2 {
        return None;
    }
    let prefix_bytes = metadata.len().min(SHEBANG_PREFIX_BYTES);
    let capacity = usize::try_from(prefix_bytes).ok()?;
    let mut bytes = Vec::with_capacity(capacity);
    File::open(path)
        .ok()?
        .take(prefix_bytes)
        .read_to_end(&mut bytes)
        .ok()?;
    let first_line = bytes.split(|byte| *byte == b'\n').next()?;
    if first_line.starts_with(b"#!")
        && (first_line.windows(4).any(|window| window == b"bash")
            || first_line.windows(3).any(|window| window == b"/sh"))
    {
        Some(Language::Bash)
    } else {
        None
    }
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
        let canonical = request.root.canonicalize().map_err(|source| CoreError::Read {
            path: request.root.display().to_string(),
            source,
        })?;
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
            vec![Diagnostic {
                severity: Severity::Warning,
                backend: "discovery".to_string(),
                message: "no supported source files discovered".to_string(),
                location: None,
                fallback_used: false,
            }]
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

#[allow(dead_code)]
fn _assert_pathbuf_send_sync(_: PathBuf) {}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn expect_core_error<T>(result: Result<T, CoreError>) -> CoreError {
        match result {
            Ok(_) => panic!("operation unexpectedly succeeded"),
            Err(error) => error,
        }
    }

    #[test]
    fn language_detection_handles_all_supported_extensions() {
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("mm"), Some(Language::ObjectiveC));
        assert_eq!(Language::from_extension("unknown"), None);
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
        let temp = std::env::temp_dir().join(format!("reporigor-header-discovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap_or_else(|error| panic!("create fixture: {error}"));
        fs::write(temp.join("shared.h"), "int shared(void);\n")
            .unwrap_or_else(|error| panic!("write fixture: {error}"));

        for language in [Language::Cpp, Language::ObjectiveC] {
            let files = discover_sources(
                &temp,
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
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn discovers_extensionless_bash_and_skips_tests() {
        let temp = std::env::temp_dir().join(format!("reporigor-discovery-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(temp.join("tests")).unwrap_or_else(|error| panic!("create fixture: {error}"));
        fs::write(temp.join("tool"), "#!/usr/bin/env bash\necho ok\n")
            .unwrap_or_else(|error| panic!("write fixture: {error}"));
        fs::write(temp.join("tests/test_tool.sh"), "echo test\n")
            .unwrap_or_else(|error| panic!("write fixture: {error}"));
        let files = discover_sources(&temp, &DiscoveryOptions::default())
            .unwrap_or_else(|error| panic!("discover fixture: {error}"));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].language, Language::Bash);
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn rejects_a_huge_sparse_shebang_without_reading_the_file() -> Result<(), Box<dyn std::error::Error>> {
        const SPARSE_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

        use std::io::Write;

        let temp = std::env::temp_dir().join(format!(
            "reporigor-sparse-shebang-discovery-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp)?;
        let path = temp.join("huge-tool");
        let mut file = File::create(&path)?;
        file.write_all(b"#!/usr/bin/env bash\n")?;
        file.set_len(SPARSE_FILE_BYTES)?;
        drop(file);
        assert_eq!(fs::metadata(&path)?.len(), SPARSE_FILE_BYTES);

        let result = discover_sources(
            &temp,
            &DiscoveryOptions {
                languages: BTreeSet::from([Language::Bash]),
                ..DiscoveryOptions::default()
            },
        );

        assert!(matches!(result, Err(CoreError::SourceTooLarge { .. })));
        fs::remove_dir_all(temp)?;
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
    fn bounded_ignore_rules_preserve_negation_depth_and_source_precedence(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join(".git/info"))?;
        fs::create_dir_all(temp.path().join("nested"))?;
        fs::create_dir_all(temp.path().join("ignored-dir"))?;
        fs::write(temp.path().join(".git/info/exclude"), "from-exclude.py\n")?;
        fs::write(
            temp.path().join(".gitignore"),
            "*.py\n!keep.py\n!from-exclude.py\nignored-dir/\n",
        )?;
        fs::write(
            temp.path().join(".ignore"),
            "!from-ignore.py\nnested/root-ignore-wins.py\n",
        )?;
        fs::write(
            temp.path().join("nested/.gitignore"),
            "!nested-keep.py\n!root-ignore-wins.py\n",
        )?;
        for path in [
            "drop.py",
            "keep.py",
            "from-exclude.py",
            "from-ignore.py",
            "nested/drop.py",
            "nested/nested-keep.py",
            "nested/root-ignore-wins.py",
            "ignored-dir/resurrect.py",
        ] {
            fs::write(temp.path().join(path), "value = 1\n")?;
        }

        let sources = discover_sources(temp.path(), &DiscoveryOptions::default())?;
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
        Ok(())
    }

    #[test]
    fn parent_ignore_files_are_not_opened_or_applied() -> Result<(), Box<dyn std::error::Error>> {
        let parent = tempfile::tempdir()?;
        let root = parent.path().join("project");
        fs::create_dir(&root)?;
        fs::write(parent.path().join(".gitignore"), "*.py\n")?;
        fs::write(root.join("selected.py"), "value = 1\n")?;

        let sources = discover_sources(&root, &DiscoveryOptions::default())?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].relative, "selected.py");
        Ok(())
    }

    #[test]
    fn nested_real_repository_resets_git_rules_but_not_custom_ignore_rules(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        fs::create_dir_all(temp.path().join("nested/.git"))?;
        fs::write(temp.path().join(".gitignore"), "*.py\n")?;
        fs::write(temp.path().join(".ignore"), "nested/custom.py\n")?;
        fs::write(temp.path().join("root.py"), "value = 1\n")?;
        fs::write(temp.path().join("nested/selected.py"), "value = 1\n")?;
        fs::write(temp.path().join("nested/custom.py"), "value = 1\n")?;

        let sources = discover_sources(temp.path(), &DiscoveryOptions::default())?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].relative, "nested/selected.py");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn ignore_symlinks_to_devices_are_rejected_without_opening_them() -> Result<(), Box<dyn std::error::Error>>
    {
        use std::os::unix::fs::symlink;

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
        let sparse = tempfile::tempdir()?;
        File::create(sparse.path().join(".gitignore"))?.set_len(IGNORE_FILE_MAX_BYTES + 1)?;
        assert!(matches!(
            discover_sources(sparse.path(), &DiscoveryOptions::default()),
            Err(CoreError::FileTooLarge { .. })
        ));

        let lines = tempfile::tempdir()?;
        fs::write(
            lines.path().join(".gitignore"),
            "\n".repeat(IGNORE_FILE_MAX_LINES + 1),
        )?;
        let error = expect_core_error(discover_sources(lines.path(), &DiscoveryOptions::default()));
        assert!(error.to_string().contains("exceeds 8192 lines"));

        let patterns = tempfile::tempdir()?;
        let contents = "ignored\n".repeat(IGNORE_FILE_MAX_PATTERNS + 1);
        fs::write(patterns.path().join(".gitignore"), contents)?;
        let error = expect_core_error(discover_sources(patterns.path(), &DiscoveryOptions::default()));
        assert!(error.to_string().contains("exceeds 4096 patterns"));
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
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let bad_file = tempfile::tempdir()?;
        let filename = OsString::from_vec(b"source-\xff.py".to_vec());
        let raw_relative = PathBuf::from(filename.clone());
        assert!(non_utf8_source_path_error(&raw_relative)
            .to_string()
            .contains("not valid UTF-8"));
        if let Err(error) = fs::write(bad_file.path().join(filename), "value = 1\n") {
            if error.raw_os_error() == Some(libc::EILSEQ) {
                eprintln!("filesystem rejects non-UTF-8 names; path-level regression still ran");
                return Ok(());
            }
            return Err(error.into());
        }
        let error = expect_core_error(discover_sources(bad_file.path(), &DiscoveryOptions::default()));
        assert!(matches!(error, CoreError::UnsafePath { .. }));
        assert!(error.to_string().contains("not valid UTF-8"));

        let bad_directory = tempfile::tempdir()?;
        let dirname = OsString::from_vec(b"package-\xff".to_vec());
        let directory = bad_directory.path().join(dirname);
        fs::create_dir(&directory)?;
        fs::write(directory.join("source.py"), "value = 1\n")?;
        let error = expect_core_error(discover_sources(
            bad_directory.path(),
            &DiscoveryOptions::default(),
        ));
        assert!(matches!(error, CoreError::UnsafePath { .. }));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlinks_are_never_followed() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        fs::write(root.path().join("inside.py"), "value = 1\n")?;
        fs::write(outside.path().join("outside.py"), "value = 1\n")?;
        symlink(outside.path(), root.path().join("linked"))?;
        let sources = discover_sources(root.path(), &DiscoveryOptions::default())?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].relative, "inside.py");
        Ok(())
    }
}

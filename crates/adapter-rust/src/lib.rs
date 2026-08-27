//! Cargo-aware native Rust adapter for the unified reporigor analysis model.
//!
//! The primary API resolves and analyzes a project in one operation because a
//! plain [`reporigor_core::SourceFile`] cannot retain Cargo `cfg` variants or
//! the multiple module aliases under which one physical Rust file may appear.

mod cargo_proxy;
mod command;
mod complexity;
mod mutations;
mod scope;
mod syntax;
mod tokens;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reporigor_core::{
    discover_sources, AnalysisRequest, AnalysisSnapshot, BackendCapabilities, BackendInfo, Capability,
    CoreError, Diagnostic, DiscoveryOptions, FileAnalysis, Language, ProjectBackend, ProjectContext,
    ProjectKind, Severity, SourceFile, SourceLocation, SyntaxBackend,
};

pub use cargo_proxy::CargoProxy;

const BACKEND_ID: &str = "rust-native";

/// Cargo feature and executable selection for native Rust project resolution.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CargoOptions {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub cargo: Option<PathBuf>,
}

impl CargoOptions {
    fn validate(&self) -> Result<(), CoreError> {
        if self.all_features && (!self.features.is_empty() || self.no_default_features) {
            return Err(CoreError::Config(
                "Cargo --all-features conflicts with --features and --no-default-features".into(),
            ));
        }
        if self.features.iter().any(|feature| feature.trim().is_empty()) {
            return Err(CoreError::Config("Cargo feature names must not be empty".into()));
        }
        Ok(())
    }

    #[must_use]
    pub fn feature_args(&self) -> Vec<OsString> {
        if self.all_features {
            return vec![OsString::from("--all-features")];
        }
        let mut args = Vec::new();
        if self.no_default_features {
            args.push(OsString::from("--no-default-features"));
        }
        if !self.features.is_empty() {
            args.push(OsString::from("--features"));
            args.push(OsString::from(self.features.join(",")));
        }
        args
    }

    pub(crate) fn cargo_program(&self) -> &OsStr {
        self.cargo
            .as_deref()
            .map_or_else(|| OsStr::new("cargo"), Path::as_os_str)
    }
}

#[derive(Debug, Clone)]
struct CachedScope {
    root: PathBuf,
    include_tests: bool,
    filters: Vec<String>,
    max_source_bytes: usize,
    allow_parse_errors: bool,
    scopes: Vec<scope::ScopedFile>,
}

/// Native Rust backend backed by Cargo, `syn`, and `rustc_lexer`.
#[derive(Debug)]
pub struct RustAdapter {
    options: CargoOptions,
    cache: Mutex<Option<CachedScope>>,
}

impl Default for RustAdapter {
    fn default() -> Self {
        Self::new(CargoOptions::default())
    }
}

impl RustAdapter {
    #[must_use]
    pub fn new(options: CargoOptions) -> Self {
        Self {
            options,
            cache: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn options(&self) -> &CargoOptions {
        &self.options
    }

    /// Resolves only sources active in the selected Cargo workspace, targets,
    /// features, platform configuration, and test mode.
    ///
    /// # Errors
    ///
    /// Returns a configuration, filesystem, Cargo, or Rust parse error when
    /// the selected workspace cannot be resolved completely.
    pub fn resolve_project(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError> {
        let (context, scopes) = self.resolve_scoped(request)?;
        self.store_cache(&context.root, request, scopes)?;
        Ok(context)
    }

    /// Resolves and analyzes the Cargo project without losing adapter-private
    /// cfg and module-prefix information between the two phases.
    ///
    /// # Errors
    ///
    /// Returns a configuration, filesystem, Cargo, or Rust parse error when
    /// the selected workspace cannot be resolved and analyzed completely.
    pub fn analyze_project(&self, request: &AnalysisRequest) -> Result<AnalysisSnapshot, CoreError> {
        let (context, scopes) = self.resolve_scoped(request)?;
        self.store_cache(&context.root, request, scopes.clone())?;
        let grouped = group_scopes(scopes);
        let mut snapshot = AnalysisSnapshot::default();
        for source in &context.sources {
            let canonical = source.path.canonicalize().map_err(|error| CoreError::Read {
                path: source.path.display().to_string(),
                source: error,
            })?;
            let file_scopes = grouped.get(&canonical).ok_or_else(|| CoreError::Backend {
                backend: BACKEND_ID.into(),
                message: format!(
                    "resolved source has no retained Cargo scope: {}",
                    source.path.display()
                ),
            })?;
            snapshot.push(Self::analyze_scoped_file(
                &context.root,
                source,
                file_scopes,
                request,
            )?);
        }
        snapshot.assign_mutation_ids();
        Ok(snapshot)
    }

    fn resolve_scoped(
        &self,
        request: &AnalysisRequest,
    ) -> Result<(ProjectContext, Vec<scope::ScopedFile>), CoreError> {
        self.options.validate()?;
        let root = request.root.canonicalize().map_err(|source| CoreError::Read {
            path: request.root.display().to_string(),
            source,
        })?;
        if !root.is_dir() {
            return Err(CoreError::InvalidRoot {
                path: root.display().to_string(),
                message: "not a directory".into(),
            });
        }
        if !root.join("Cargo.toml").is_file() {
            return Err(CoreError::BackendUnavailable {
                backend: BACKEND_ID.into(),
                message: format!("{} does not contain Cargo.toml", root.display()),
            });
        }
        let rust_requested = request.languages.is_empty() || request.languages.contains(&Language::Rust);
        let scopes = if rust_requested {
            validate_rust_source_budget(&root, request)?;
            scope::discover(
                &root,
                request.include_tests,
                &request.filters,
                &self.options,
                request.max_source_bytes,
                request.allow_parse_errors,
            )
            .map_err(|message| CoreError::Backend {
                backend: BACKEND_ID.into(),
                message,
            })?
        } else {
            Vec::new()
        };
        let sources = unique_sources(&root, &scopes);
        let diagnostics = if sources.is_empty() && rust_requested {
            vec![Diagnostic {
                severity: Severity::Warning,
                backend: BACKEND_ID.into(),
                message: "Cargo resolved no active Rust source files".into(),
                location: None,
                fallback_used: false,
            }]
        } else {
            Vec::new()
        };
        Ok((
            ProjectContext {
                root,
                kinds: BTreeSet::from([ProjectKind::Cargo]),
                sources,
                backends: vec![backend_info()],
                diagnostics,
            },
            scopes,
        ))
    }

    fn store_cache(
        &self,
        root: &Path,
        request: &AnalysisRequest,
        scopes: Vec<scope::ScopedFile>,
    ) -> Result<(), CoreError> {
        let mut cache = self.cache.lock().map_err(|_| CoreError::Backend {
            backend: BACKEND_ID.into(),
            message: "Rust scope cache lock was poisoned".into(),
        })?;
        *cache = Some(CachedScope {
            root: root.to_path_buf(),
            include_tests: request.include_tests,
            filters: request.filters.clone(),
            max_source_bytes: request.max_source_bytes,
            allow_parse_errors: request.allow_parse_errors,
            scopes,
        });
        Ok(())
    }

    fn scopes_for_file(
        &self,
        root: &Path,
        source: &SourceFile,
        request: &AnalysisRequest,
    ) -> Result<Vec<scope::ScopedFile>, CoreError> {
        let root = root.canonicalize().map_err(|error| CoreError::Read {
            path: root.display().to_string(),
            source: error,
        })?;
        let source_path = source.path.canonicalize().map_err(|error| CoreError::Read {
            path: source.path.display().to_string(),
            source: error,
        })?;
        validate_rust_source_budget(&root, request)?;
        {
            let cache = self.cache.lock().map_err(|_| CoreError::Backend {
                backend: BACKEND_ID.into(),
                message: "Rust scope cache lock was poisoned".into(),
            })?;
            if let Some(cached) = cache.as_ref().filter(|cached| {
                cached.root == root
                    && cached.include_tests == request.include_tests
                    && cached.filters == request.filters
                    && cached.max_source_bytes == request.max_source_bytes
                    && cached.allow_parse_errors == request.allow_parse_errors
            }) {
                let found: Vec<_> = cached
                    .scopes
                    .iter()
                    .filter(|scoped| scoped.path == source_path)
                    .cloned()
                    .collect();
                if !found.is_empty() {
                    return Ok(found);
                }
            }
        }
        let scopes = scope::discover(
            &root,
            request.include_tests,
            &request.filters,
            &self.options,
            request.max_source_bytes,
            request.allow_parse_errors,
        )
        .map_err(|message| CoreError::Backend {
            backend: BACKEND_ID.into(),
            message,
        })?;
        let found: Vec<_> = scopes
            .iter()
            .filter(|scoped| scoped.path == source_path)
            .cloned()
            .collect();
        self.store_cache(&root, request, scopes)?;
        if found.is_empty() {
            Err(CoreError::Backend {
                backend: BACKEND_ID.into(),
                message: format!(
                    "{} is not active in the selected Cargo scope",
                    source.path.display()
                ),
            })
        } else {
            Ok(found)
        }
    }

    fn analyze_scoped_file(
        _root: &Path,
        source_file: &SourceFile,
        scopes: &[scope::ScopedFile],
        request: &AnalysisRequest,
    ) -> Result<FileAnalysis, CoreError> {
        let source = match scope::read_source_bounded(&source_file.path, request.max_source_bytes).map_err(
            |message| CoreError::Backend {
                backend: BACKEND_ID.into(),
                message,
            },
        )? {
            scope::BoundedSource::Content(source) => source,
            scope::BoundedSource::TooLarge { actual_bytes } => {
                return Err(CoreError::source_too_large(
                    &source_file.path,
                    actual_bytes,
                    request.max_source_bytes,
                ));
            }
        };
        let syntax = match syn::parse_file(&source) {
            Ok(syntax) => syntax,
            Err(error) if request.allow_parse_errors => {
                let range = error.span().byte_range();
                let location = match (
                    scalar_position(&source, range.start),
                    scalar_position(&source, range.end),
                ) {
                    (Some((start_line, start_column)), Some((end_line, end_column))) => {
                        Some(SourceLocation {
                            file: source_file.relative.clone(),
                            start_line,
                            start_column,
                            end_line,
                            end_column,
                        })
                    }
                    _ => None,
                };
                return Ok(FileAnalysis {
                    source: source_file.clone(),
                    backend: backend_info(),
                    functions: Vec::new(),
                    tokens: Vec::new(),
                    mutations: Vec::new(),
                    diagnostics: vec![Diagnostic {
                        severity: Severity::Error,
                        backend: BACKEND_ID.into(),
                        message: format!(
                            "native Rust parse failed; generic valid-subtree fallback is required: {error}"
                        ),
                        location,
                        fallback_used: true,
                    }],
                    parse_errors: 1,
                });
            }
            Err(error) => {
                return Err(CoreError::Parse {
                    path: source_file.path.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        let merged_cfg = scope::CfgContext::merged(scopes.iter().map(|scoped| &scoped.cfg));
        Ok(FileAnalysis {
            source: source_file.clone(),
            backend: backend_info(),
            functions: complexity::extract(&syntax, &source, &source_file.relative, scopes),
            tokens: tokens::normalize(&syntax, &source, &merged_cfg),
            mutations: mutations::enumerate(&syntax, &source, &source_file.relative, &merged_cfg),
            diagnostics: Vec::new(),
            parse_errors: 0,
        })
    }
}

fn validate_rust_source_budget(root: &Path, request: &AnalysisRequest) -> Result<(), CoreError> {
    discover_sources(
        root,
        &DiscoveryOptions {
            languages: BTreeSet::from([Language::Rust]),
            filters: request.filters.clone(),
            include_tests: request.include_tests,
            max_source_bytes: request.max_source_bytes,
        },
    )?;
    Ok(())
}

pub(crate) fn scalar_position(source: &str, byte_offset: usize) -> Option<(u32, u32)> {
    let prefix = source.get(..byte_offset)?;
    let line_start = match prefix.rfind('\n') {
        Some(index) => index.checked_add(1)?,
        None => 0,
    };
    let line = prefix
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        .checked_add(1)?;
    let column = prefix[line_start..].chars().count().checked_add(1)?;
    Some((u32::try_from(line).ok()?, u32::try_from(column).ok()?))
}

impl SyntaxBackend for RustAdapter {
    fn info(&self) -> BackendInfo {
        backend_info()
    }

    fn supports(&self, language: Language) -> bool {
        language == Language::Rust
    }

    fn analyze_file(
        &self,
        root: &Path,
        source: &SourceFile,
        request: &AnalysisRequest,
    ) -> Result<FileAnalysis, CoreError> {
        if source.language != Language::Rust {
            return Err(CoreError::BackendUnavailable {
                backend: BACKEND_ID.into(),
                message: format!("Rust adapter cannot analyze {}", source.language),
            });
        }
        let scopes = self.scopes_for_file(root, source, request)?;
        Self::analyze_scoped_file(root, source, &scopes, request)
    }
}

impl ProjectBackend for RustAdapter {
    fn info(&self) -> BackendInfo {
        backend_info()
    }

    fn supports(&self, project: ProjectKind) -> bool {
        project == ProjectKind::Cargo
    }

    fn resolve(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError> {
        self.resolve_project(request)
    }
}

fn backend_info() -> BackendInfo {
    BackendInfo {
        id: BACKEND_ID.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        native: true,
        capabilities: BackendCapabilities::new([
            Capability::Syntax,
            Capability::Functions,
            Capability::Complexity,
            Capability::Tokens,
            Capability::Mutations,
            Capability::ProjectSemantics,
            Capability::ParseValidation,
        ]),
    }
}

fn group_scopes(scopes: Vec<scope::ScopedFile>) -> BTreeMap<PathBuf, Vec<scope::ScopedFile>> {
    let mut grouped = BTreeMap::new();
    for scoped in scopes {
        grouped
            .entry(scoped.path.clone())
            .or_insert_with(Vec::new)
            .push(scoped);
    }
    grouped
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn unique_sources(root: &Path, scopes: &[scope::ScopedFile]) -> Vec<SourceFile> {
    let mut paths = BTreeSet::new();
    for scoped in scopes {
        paths.insert(scoped.path.clone());
    }
    paths
        .into_iter()
        .map(|path| {
            let relative = relative(root, &path);
            SourceFile {
                test: Language::Rust.is_test_path(&relative),
                generated: relative
                    .split('/')
                    .any(|part| matches!(part, "generated" | "gen")),
                path,
                relative,
                language: Language::Rust,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn cargo_feature_arguments_follow_cargo_rules() {
        assert_eq!(
            CargoOptions {
                features: vec!["alpha".into(), "beta".into()],
                no_default_features: true,
                ..CargoOptions::default()
            }
            .feature_args(),
            [
                OsString::from("--no-default-features"),
                OsString::from("--features"),
                OsString::from("alpha,beta")
            ]
        );
        assert_eq!(
            CargoOptions {
                all_features: true,
                ..CargoOptions::default()
            }
            .feature_args(),
            [OsString::from("--all-features")]
        );
    }

    #[test]
    fn end_to_end_analysis_retains_aliases_and_normalized_records() {
        let dir = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        fs::create_dir_all(dir.path().join("src"))
            .unwrap_or_else(|error| panic!("source directory: {error}"));
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='rust-adapter-e2e'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        fs::write(
            dir.path().join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"rust-adapter-e2e\"\nversion = \"0.1.0\"\n",
        )
        .unwrap_or_else(|error| panic!("lockfile: {error}"));
        fs::write(
            dir.path().join("src/lib.rs"),
            "#[path=\"shared.rs\"] mod alpha;\n#[path=\"shared.rs\"] mod beta;\n",
        )
        .unwrap_or_else(|error| panic!("root source: {error}"));
        fs::write(
            dir.path().join("src/shared.rs"),
            "pub fn choose(a: bool, b: bool) -> bool { if a && b { true } else { false } }\n",
        )
        .unwrap_or_else(|error| panic!("shared source: {error}"));

        let adapter = RustAdapter::default();
        let snapshot = adapter
            .analyze_project(&AnalysisRequest::new(dir.path().to_path_buf()))
            .unwrap_or_else(|error| panic!("analysis: {error}"));
        assert_eq!(snapshot.files.len(), 2);
        assert!(snapshot.functions.iter().any(|item| item.name == "alpha::choose"));
        assert!(snapshot.functions.iter().any(|item| item.name == "beta::choose"));
        assert!(snapshot.tokens["src/shared.rs"]
            .iter()
            .any(|token| token.value == "ID"));
        assert!(snapshot
            .mutations
            .iter()
            .any(|candidate| { candidate.original == "&&" && candidate.replacement == "||" }));
        assert!(snapshot.mutations.windows(2).all(|pair| pair[0].id < pair[1].id));
    }

    #[test]
    fn oversized_sparse_source_is_not_read_or_parsed_during_discovery() {
        let dir = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        fs::create_dir_all(dir.path().join("src"))
            .unwrap_or_else(|error| panic!("source directory: {error}"));
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='rust-adapter-large'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        fs::write(
            dir.path().join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"rust-adapter-large\"\nversion = \"0.1.0\"\n",
        )
        .unwrap_or_else(|error| panic!("lockfile: {error}"));
        let source_path = dir.path().join("src/lib.rs");
        fs::write(&source_path, "pub fn small_prefix() {}\n")
            .unwrap_or_else(|error| panic!("source: {error}"));
        fs::OpenOptions::new()
            .write(true)
            .open(&source_path)
            .and_then(|file| file.set_len(16 * 1024 * 1024))
            .unwrap_or_else(|error| panic!("sparse source: {error}"));

        let mut request = AnalysisRequest::new(dir.path().to_path_buf());
        request.max_source_bytes = 64;
        let Err(error) = RustAdapter::default().analyze_project(&request) else {
            panic!("oversized source was unexpectedly analyzed");
        };
        assert!(matches!(
            error,
            CoreError::SourceTooLarge {
                actual_bytes: 16_777_216,
                max_source_bytes: 64,
                ..
            }
        ));
    }

    #[test]
    fn permissive_parse_failure_marks_file_for_generic_fallback() {
        let dir = tempdir().unwrap_or_else(|error| panic!("fixture: {error}"));
        fs::create_dir_all(dir.path().join("src"))
            .unwrap_or_else(|error| panic!("source directory: {error}"));
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname='rust-adapter-malformed'\nversion='0.1.0'\nedition='2021'\n",
        )
        .unwrap_or_else(|error| panic!("manifest: {error}"));
        fs::write(
            dir.path().join("Cargo.lock"),
            "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 3\n\n[[package]]\nname = \"rust-adapter-malformed\"\nversion = \"0.1.0\"\n",
        )
        .unwrap_or_else(|error| panic!("lockfile: {error}"));
        let malformed = "const EMOJI: &str = \"😀\"; @ pub fn valid() -> bool { true }\n";
        fs::write(dir.path().join("src/lib.rs"), malformed).unwrap_or_else(|error| panic!("source: {error}"));

        let mut request = AnalysisRequest::new(dir.path().to_path_buf());
        request.allow_parse_errors = true;
        let snapshot = RustAdapter::default()
            .analyze_project(&request)
            .unwrap_or_else(|error| panic!("analysis: {error}"));
        let diagnostic = snapshot
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.backend == BACKEND_ID && diagnostic.fallback_used)
            .unwrap_or_else(|| panic!("missing fallback marker: {:?}", snapshot.diagnostics));
        assert!(diagnostic
            .message
            .contains("generic valid-subtree fallback is required"));
        assert_eq!(
            diagnostic
                .location
                .as_ref()
                .map(|location| location.file.as_str()),
            Some("src/lib.rs")
        );
        let Some(error_offset) = malformed.find('@') else {
            panic!("fixture parse-error token must exist");
        };
        let expected_column =
            u32::try_from(malformed[..error_offset].chars().count() + 1).unwrap_or(u32::MAX);
        assert_eq!(
            diagnostic
                .location
                .as_ref()
                .map(|location| (location.start_line, location.start_column)),
            Some((1, expected_column))
        );
        assert_ne!(
            expected_column,
            u32::try_from(error_offset + 1).unwrap_or(u32::MAX)
        );
        assert_eq!(snapshot.parse_errors, 1);
        assert!(snapshot.functions.is_empty());
    }
}

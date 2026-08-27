use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reporigor_core::{
    AnalysisRequest, BackendCapabilities, BackendInfo, Capability, CoreError, Diagnostic, ProjectBackend,
    ProjectContext, ProjectKind, Severity, SourceBudget, SourceFile,
};

use crate::database::{discover_compilation_database, load_database, CompilationDatabase};
use crate::validation::{probe_compiler_version, validate_translation_unit};
use crate::{sanitize_compile_command, ClangAdapterError, ClangLanguage, TranslationUnit, ValidationStatus};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_AST_OUTPUT_LIMIT: usize = 128 * 1024 * 1024;

/// Fully resolved Clang project, including standard core context and per-command
/// provenance/validation details.
#[derive(Debug, Clone)]
pub struct ClangProject {
    pub context: ProjectContext,
    pub database: CompilationDatabase,
    pub translation_units: Vec<TranslationUnit>,
    pub compiler: PathBuf,
    pub compiler_version: Option<String>,
}

/// Project backend for an existing Clang JSON compilation database.
#[derive(Debug, Clone)]
pub struct ClangAdapter {
    compiler: PathBuf,
    timeout: Duration,
    validate: bool,
    ast_output_limit: usize,
}

impl Default for ClangAdapter {
    fn default() -> Self {
        Self {
            compiler: PathBuf::from("clang"),
            timeout: DEFAULT_TIMEOUT,
            validate: true,
            ast_output_limit: DEFAULT_AST_OUTPUT_LIMIT,
        }
    }
}

impl ClangAdapter {
    #[must_use]
    pub fn new(compiler: impl Into<PathBuf>) -> Self {
        let compiler = compiler.into();
        let compiler = if compiler.components().count() > 1 {
            compiler.canonicalize().unwrap_or(compiler)
        } else {
            compiler
        };
        Self {
            compiler,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Allow callers performing discovery-only operations to skip validation
    /// explicitly. This is never selected implicitly after a validation error.
    #[must_use]
    pub fn with_validation(mut self, validate: bool) -> Self {
        self.validate = validate;
        self
    }

    /// Set the maximum JSON AST output retained for one translation unit. Clang
    /// is still drained after the limit so it cannot block on a full pipe.
    #[must_use]
    pub fn with_ast_output_limit(mut self, bytes: usize) -> Self {
        self.ast_output_limit = bytes;
        self
    }

    #[must_use]
    pub fn compiler(&self) -> &Path {
        &self.compiler
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    #[must_use]
    pub const fn ast_output_limit(&self) -> usize {
        self.ast_output_limit
    }

    /// Discover an existing compilation database without invoking `CMake`, `Meson`,
    /// Bear, or another generator.
    ///
    /// # Errors
    ///
    /// Returns an error when `root` is invalid or cannot be inspected.
    pub fn discover(root: &Path) -> Result<Option<PathBuf>, ClangAdapterError> {
        discover_compilation_database(root)
    }

    /// Load a specific existing compilation database.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read or does not satisfy the
    /// JSON compilation-database contract.
    pub fn load_database(path: &Path) -> Result<CompilationDatabase, ClangAdapterError> {
        load_database(path)
    }

    /// Resolve project sources and validate each included translation unit with
    /// the configured Clang executable.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid project root, a missing compilation
    /// database, or a malformed database. Individual Clang failures are retained
    /// as diagnostics and translation-unit statuses instead of being hidden.
    #[allow(clippy::too_many_lines)]
    pub fn validate_project(&self, request: &AnalysisRequest) -> Result<ClangProject, CoreError> {
        let root = request.root.canonicalize().map_err(|source| CoreError::Read {
            path: request.root.display().to_string(),
            source,
        })?;
        if !root.is_dir() {
            return Err(CoreError::InvalidRoot {
                path: root.display().to_string(),
                message: "not a directory".to_string(),
            });
        }
        let mut source_budget = SourceBudget::new(request.max_source_bytes)?;
        let mut budgeted_sources = BTreeSet::new();
        let database_path = Self::discover(&root).map_err(map_adapter_error)?.ok_or_else(|| {
            CoreError::BackendUnavailable {
                backend: "clang".to_string(),
                message: format!(
                    "no existing compile_commands.json found under {}; build generation was not attempted",
                    root.display()
                ),
            }
        })?;
        let database = Self::load_database(&database_path).map_err(map_adapter_error)?;

        let mut diagnostics = Vec::new();
        let compiler_version = if self.validate {
            match probe_compiler_version(&self.compiler, self.timeout) {
                Ok(version) => Some(version),
                Err(message) => {
                    diagnostics.push(diagnostic(Severity::Error, message, false));
                    None
                }
            }
        } else {
            diagnostics.push(diagnostic(
                Severity::Info,
                "Clang translation-unit validation was explicitly disabled".to_string(),
                false,
            ));
            None
        };

        let mut backend = self.info();
        if let Some(version) = &compiler_version {
            backend.version.clone_from(version);
        }
        diagnostics.push(diagnostic(
            Severity::Info,
            format!(
                "loaded {} translation-unit command(s) from {}; compiler: {}",
                database.commands.len(),
                database.path.display(),
                self.compiler.display()
            ),
            false,
        ));

        let mut sources = BTreeMap::<String, SourceFile>::new();
        let mut translation_units = Vec::with_capacity(database.commands.len());
        for (command_index, command) in database.commands.iter().cloned().enumerate() {
            let Some(language) = ClangLanguage::classify(&command) else {
                diagnostics.push(diagnostic(
                    Severity::Warning,
                    format!(
                        "could not classify translation unit {} from compilation-database entry {command_index}",
                        command.file.display()
                    ),
                    false,
                ));
                translation_units.push(not_validated(
                    command_index,
                    command,
                    None,
                    "unrecognized Clang language mode",
                ));
                continue;
            };
            let core_language = language.core_language();
            if !request.languages.is_empty() && !request.languages.contains(&core_language) {
                translation_units.push(not_validated(
                    command_index,
                    command,
                    Some(language),
                    "language excluded by request",
                ));
                continue;
            }

            let canonical_file = match command.file.canonicalize() {
                Ok(file) => file,
                Err(error) => {
                    diagnostics.push(diagnostic(
                        Severity::Warning,
                        format!(
                            "skipped missing translation unit {}: {error}",
                            command.file.display()
                        ),
                        false,
                    ));
                    translation_units.push(not_validated(
                        command_index,
                        command,
                        Some(language),
                        "translation-unit source does not exist",
                    ));
                    continue;
                }
            };
            let Ok(relative_path) = canonical_file.strip_prefix(&root) else {
                diagnostics.push(diagnostic(
                    Severity::Error,
                    format!(
                        "compilation-database entry {} resolves outside project root {}",
                        canonical_file.display(),
                        root.display()
                    ),
                    false,
                ));
                translation_units.push(not_validated(
                    command_index,
                    command,
                    Some(language),
                    "translation unit is outside project root",
                ));
                continue;
            };
            let relative = relative_path.to_string_lossy().replace('\\', "/");
            if !request.filters.is_empty() && !request.filters.iter().any(|filter| relative.contains(filter))
            {
                translation_units.push(not_validated(
                    command_index,
                    command,
                    Some(language),
                    "source excluded by request filter",
                ));
                continue;
            }
            let test = core_language.is_test_path(&relative);
            if test && !request.include_tests {
                translation_units.push(not_validated(
                    command_index,
                    command,
                    Some(language),
                    "test source excluded by request",
                ));
                continue;
            }
            let metadata = fs::metadata(&canonical_file).map_err(|source| CoreError::Read {
                path: canonical_file.display().to_string(),
                source,
            })?;
            if !metadata.is_file() {
                return Err(CoreError::UnsafePath {
                    path: canonical_file.display().to_string(),
                    message: "selected translation unit is not a regular file".to_string(),
                });
            }
            if budgeted_sources.insert(canonical_file.clone()) {
                source_budget.observe(&canonical_file, metadata.len())?;
            }

            let generated = is_generated(relative_path);
            let source = SourceFile {
                path: canonical_file,
                relative: relative.clone(),
                language: core_language,
                generated,
                test,
            };
            sources.entry(relative).or_insert_with(|| source.clone());

            translation_units.push(TranslationUnit {
                command_index,
                command,
                language: Some(language),
                source: Some(source),
                invocation: None,
                status: ValidationStatus::NotValidated {
                    message: "source-budget preflight complete; validation pending".to_string(),
                },
                elapsed: Duration::ZERO,
            });
        }

        // Validate only after the complete selected source set has passed its
        // per-file, count, and aggregate metadata budgets.
        for unit in &mut translation_units {
            let (Some(language), Some(source)) = (unit.language, unit.source.clone()) else {
                continue;
            };
            let (invocation, status, elapsed) = if self.validate {
                validate_translation_unit(&unit.command, language, &self.compiler, self.timeout)
            } else {
                let invocation = sanitize_compile_command(&unit.command, &self.compiler, language).ok();
                (
                    invocation,
                    ValidationStatus::NotValidated {
                        message: "validation explicitly disabled".to_string(),
                    },
                    Duration::ZERO,
                )
            };
            add_validation_diagnostic(&mut diagnostics, request, &source, &status, unit.command_index);
            unit.invocation = invocation;
            unit.status = status;
            unit.elapsed = elapsed;
        }

        if database.commands.is_empty() {
            diagnostics.push(diagnostic(
                Severity::Warning,
                "compilation database contains no translation-unit commands".to_string(),
                false,
            ));
        } else if sources.is_empty() {
            diagnostics.push(diagnostic(
                Severity::Warning,
                "compilation database contains no translation units selected by this request".to_string(),
                false,
            ));
        }

        let mut kinds = BTreeSet::new();
        kinds.insert(ProjectKind::CompilationDatabase);
        let context = ProjectContext {
            root,
            kinds,
            sources: sources.into_values().collect(),
            backends: vec![backend],
            diagnostics,
        };
        Ok(ClangProject {
            context,
            database,
            translation_units,
            compiler: self.compiler.clone(),
            compiler_version,
        })
    }
}

impl ProjectBackend for ClangAdapter {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            id: "clang".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            native: true,
            capabilities: BackendCapabilities::new([
                Capability::Syntax,
                Capability::Functions,
                Capability::Complexity,
                Capability::ProjectSemantics,
                Capability::ParseValidation,
            ]),
        }
    }

    fn supports(&self, project: ProjectKind) -> bool {
        project == ProjectKind::CompilationDatabase
    }

    fn resolve(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError> {
        self.validate_project(request).map(|project| project.context)
    }
}

fn not_validated(
    command_index: usize,
    command: crate::CompileCommand,
    language: Option<ClangLanguage>,
    message: &str,
) -> TranslationUnit {
    TranslationUnit {
        command_index,
        command,
        language,
        source: None,
        invocation: None,
        status: ValidationStatus::NotValidated {
            message: message.to_string(),
        },
        elapsed: Duration::ZERO,
    }
}

fn add_validation_diagnostic(
    diagnostics: &mut Vec<Diagnostic>,
    request: &AnalysisRequest,
    source: &SourceFile,
    status: &ValidationStatus,
    command_index: usize,
) {
    let (severity, message) = match status {
        ValidationStatus::Valid | ValidationStatus::NotValidated { .. } => return,
        ValidationStatus::Invalid { exit_code, stderr } => (
            if request.allow_parse_errors {
                Severity::Warning
            } else {
                Severity::Error
            },
            format!(
                "Clang rejected {} (database entry {command_index}, exit {:?}): {}",
                source.relative,
                exit_code,
                nonempty(stderr, "no compiler diagnostics")
            ),
        ),
        ValidationStatus::TimedOut { timeout, stderr } => (
            Severity::Error,
            format!(
                "Clang validation timed out for {} after {:.3}s (database entry {command_index}): {}",
                source.relative,
                timeout.as_secs_f64(),
                nonempty(stderr, "no compiler diagnostics")
            ),
        ),
        ValidationStatus::Unavailable { message } => (
            Severity::Error,
            format!(
                "Clang validation unavailable for {} (database entry {command_index}): {message}",
                source.relative
            ),
        ),
        ValidationStatus::Rejected { message } => (
            Severity::Error,
            format!(
                "refused unsafe validation command for {} (database entry {command_index}): {message}",
                source.relative
            ),
        ),
    };
    diagnostics.push(diagnostic(severity, message, false));
}

fn diagnostic(severity: Severity, message: String, fallback_used: bool) -> Diagnostic {
    Diagnostic {
        severity,
        backend: "clang".to_string(),
        message,
        location: None,
        fallback_used,
    }
}

fn nonempty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

fn is_generated(relative: &Path) -> bool {
    relative.components().any(|component| {
        matches!(
            component.as_os_str().to_string_lossy().as_ref(),
            "generated" | "gen" | "DerivedSources"
        )
    })
}

fn map_adapter_error(error: ClangAdapterError) -> CoreError {
    match error {
        ClangAdapterError::InvalidRoot { path } => CoreError::InvalidRoot {
            path,
            message: "not a directory or compilation database".to_string(),
        },
        ClangAdapterError::Read { path, source } => CoreError::Read { path, source },
        other => CoreError::Parse {
            path: "compile_commands.json".to_string(),
            message: other.to_string(),
        },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use tempfile::tempdir;

    use super::*;

    fn write_compiler(path: &Path) {
        fs::write(
            path,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'fake clang 1.0'; exit 0; fi\nexit 0\n",
        )
        .unwrap_or_else(|error| panic!("write compiler: {error}"));
        let mut permissions = fs::metadata(path)
            .unwrap_or_else(|error| panic!("metadata: {error}"))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap_or_else(|error| panic!("permissions: {error}"));
    }

    #[test]
    fn resolves_database_sources_and_preserves_validation_provenance() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        fs::create_dir(temp.path().join("src")).unwrap_or_else(|error| panic!("create src: {error}"));
        fs::write(temp.path().join("src/main.c"), "int main(void) { return 0; }")
            .unwrap_or_else(|error| panic!("write source: {error}"));
        fs::write(
            temp.path().join("compile_commands.json"),
            format!(
                r#"[{{"directory":{},"file":"src/main.c","arguments":["ccache","clang","-MD","-MF","main.d","-c","src/main.c","-o","main.o"]}}]"#,
                serde_json::to_string(temp.path()).unwrap_or_else(|error| panic!("path json: {error}"))
            ),
        )
        .unwrap_or_else(|error| panic!("write database: {error}"));
        let compiler = temp.path().join("fake-clang");
        write_compiler(&compiler);

        let project = ClangAdapter::new(&compiler)
            .with_timeout(Duration::from_secs(1))
            .validate_project(&AnalysisRequest::new(temp.path().to_path_buf()))
            .unwrap_or_else(|error| panic!("validate project: {error}"));
        assert_eq!(project.context.sources.len(), 1);
        assert_eq!(project.context.sources[0].relative, "src/main.c");
        assert_eq!(project.compiler_version.as_deref(), Some("fake clang 1.0"));
        assert_eq!(project.translation_units.len(), 1);
        assert_eq!(project.translation_units[0].status, ValidationStatus::Valid);
        let invocation = project.translation_units[0]
            .invocation
            .as_ref()
            .unwrap_or_else(|| panic!("missing invocation"));
        assert!(!invocation.arguments.iter().any(|argument| argument == "-o"));
        assert!(!invocation.arguments.iter().any(|argument| argument == "-MF"));
        assert!(matches!(
            project.translation_units[0].command.origin,
            crate::CommandOrigin::Arguments(_)
        ));
        assert_eq!(project.context.backends[0].version, "fake clang 1.0");
    }

    #[test]
    fn missing_database_is_explicit_and_does_not_generate_one() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let Err(error) =
            ClangAdapter::default().validate_project(&AnalysisRequest::new(temp.path().to_path_buf()))
        else {
            panic!("missing database unexpectedly resolved");
        };
        assert!(matches!(error, CoreError::BackendUnavailable { .. }));
        assert!(!temp.path().join("compile_commands.json").exists());
    }

    #[test]
    fn oversized_selected_translation_unit_is_a_typed_error() {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let source = temp.path().join("large.c");
        fs::write(&source, "int value(void) { return 123456789; }\n")
            .unwrap_or_else(|error| panic!("write source: {error}"));
        fs::write(
            temp.path().join("compile_commands.json"),
            format!(
                r#"[{{"directory":{},"file":"large.c","arguments":["clang","-c","large.c"]}}]"#,
                serde_json::to_string(temp.path()).unwrap_or_else(|error| panic!("path json: {error}"))
            ),
        )
        .unwrap_or_else(|error| panic!("write database: {error}"));

        let mut request = AnalysisRequest::new(temp.path().to_path_buf());
        request.max_source_bytes = 8;
        let Err(error) = ClangAdapter::default()
            .with_validation(false)
            .validate_project(&request)
        else {
            panic!("oversized Clang translation unit was unexpectedly accepted");
        };
        assert!(matches!(
            error,
            CoreError::SourceTooLarge {
                actual_bytes: 38,
                max_source_bytes: 8,
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn outside_root_translation_unit_is_rejected() {
        let project = tempdir().unwrap_or_else(|error| panic!("project tempdir: {error}"));
        let outside = tempdir().unwrap_or_else(|error| panic!("outside tempdir: {error}"));
        fs::write(outside.path().join("external.c"), "int external;")
            .unwrap_or_else(|error| panic!("write source: {error}"));
        fs::write(
            project.path().join("compile_commands.json"),
            format!(
                r#"[{{"directory":{},"file":{},"arguments":["clang",{}]}}]"#,
                serde_json::to_string(project.path()).unwrap_or_else(|error| panic!("project json: {error}")),
                serde_json::to_string(&outside.path().join("external.c"))
                    .unwrap_or_else(|error| panic!("file json: {error}")),
                serde_json::to_string(&outside.path().join("external.c"))
                    .unwrap_or_else(|error| panic!("arg json: {error}"))
            ),
        )
        .unwrap_or_else(|error| panic!("write database: {error}"));

        let resolution = ClangAdapter::default()
            .with_validation(false)
            .validate_project(&AnalysisRequest::new(project.path().to_path_buf()))
            .unwrap_or_else(|error| panic!("resolve project: {error}"));
        assert!(resolution.context.sources.is_empty());
        assert!(resolution
            .context
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error
                && diagnostic.message.contains("outside project root")));
    }
}

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use reporigor_core::{
    AnalysisRequest, BackendInfo, Capability, CoreError, Diagnostic, ProjectBackend, ProjectContext,
    ProjectKind, Severity, SourceBudget, SourceFile,
};

use crate::database::{discover_compilation_database, load_database, CompilationDatabase};
use crate::validation::{probe_compiler_version, validate_translation_unit};
use crate::{sanitize_compile_command, ClangAdapterError, ClangLanguage, TranslationUnit, ValidationStatus};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_AST_OUTPUT_LIMIT: usize = 128 * 1024 * 1024;

macro_rules! builder_setter {
    ($(#[$metadata:meta])* $method:ident, $field:ident, $value:ident, $type:ty) => {
        $(#[$metadata])*
        #[must_use]
        pub fn $method(mut self, $value: $type) -> Self {
            self.$field = $value;
            self
        }
    };
}

/// Fully resolved Clang project, including standard core context and per-command
/// provenance/validation details.
#[derive(Debug, Clone)]
#[must_use = "inspect the resolved project and translation units"]
pub struct ClangProject {
    pub context: ProjectContext,
    pub database: CompilationDatabase,
    pub compiler: PathBuf,
    pub compiler_version: Option<String>,
    pub translation_units: Vec<TranslationUnit>,
}

/// Project backend for an existing Clang JSON compilation database.
#[derive(Debug, Clone)]
pub struct ClangAdapter {
    compiler: PathBuf,
    timeout: Duration,
    validate: bool,
    ast_output_limit: usize,
}

struct AdapterSettings<'a> {
    compiler: &'a PathBuf,
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
    const fn settings(&self) -> AdapterSettings<'_> {
        AdapterSettings {
            compiler: &self.compiler,
            timeout: self.timeout,
            validate: self.validate,
            ast_output_limit: self.ast_output_limit,
        }
    }

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

    builder_setter!(with_timeout, timeout, timeout, Duration);

    builder_setter!(
        #[doc = "Allow discovery-only callers to skip validation explicitly."]
        #[doc = "This is never selected implicitly after a validation error."]
        with_validation,
        validate,
        validate,
        bool
    );

    builder_setter!(
        #[doc = "Set the maximum JSON AST output retained for one translation unit."]
        #[doc = "Clang is still drained after the limit so it cannot block on a full pipe."]
        with_ast_output_limit,
        ast_output_limit,
        bytes,
        usize
    );

    #[must_use]
    pub fn compiler(&self) -> &Path {
        self.settings().compiler.as_path()
    }

    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.settings().timeout
    }

    #[must_use]
    pub const fn ast_output_limit(&self) -> usize {
        self.settings().ast_output_limit
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
    pub fn validate_project(&self, request: &AnalysisRequest) -> Result<ClangProject, CoreError> {
        let settings = self.settings();
        let validation = ValidationConfiguration {
            compiler: settings.compiler.clone(),
            timeout: settings.timeout,
            validate: settings.validate,
        };
        let root = canonical_project_root(&request.root)?;
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
        ValidationSession::new(self, validation, request, root, database)?.run()
    }
}

struct ValidationConfiguration {
    compiler: PathBuf,
    timeout: Duration,
    validate: bool,
}

struct ValidationSession<'a> {
    validation: ValidationConfiguration,
    request: &'a AnalysisRequest,
    root: PathBuf,
    database: CompilationDatabase,
    compiler_version: Option<String>,
    backend: BackendInfo,
    diagnostics: Vec<Diagnostic>,
    source_budget: SourceBudget,
    budgeted_sources: BTreeSet<PathBuf>,
    sources: BTreeMap<String, SourceFile>,
    translation_units: Vec<TranslationUnit>,
}

impl<'a> ValidationSession<'a> {
    fn new(
        adapter: &'a ClangAdapter,
        validation: ValidationConfiguration,
        request: &'a AnalysisRequest,
        root: PathBuf,
        database: CompilationDatabase,
    ) -> Result<Self, CoreError> {
        let (compiler_version, mut diagnostics) = compiler_details(&validation);
        let backend = configured_backend(adapter, compiler_version.as_deref());
        diagnostics.push(database_diagnostic(&validation, &database));
        let command_count = database.commands.len();
        Ok(Self {
            validation,
            request,
            root,
            database,
            compiler_version,
            backend,
            diagnostics,
            source_budget: SourceBudget::new(request.max_source_bytes)?,
            budgeted_sources: BTreeSet::new(),
            sources: BTreeMap::new(),
            translation_units: Vec::with_capacity(command_count),
        })
    }

    fn run(mut self) -> Result<ClangProject, CoreError> {
        self.preflight_sources()?;
        self.validate_units();
        self.add_selection_diagnostic();
        Ok(self.finish())
    }

    fn preflight_sources(&mut self) -> Result<(), CoreError> {
        let commands = std::mem::take(&mut self.database.commands);
        for (index, command) in commands.iter().cloned().enumerate() {
            if let Err(error) = self.preflight_command(index, command) {
                self.database.commands = commands;
                return Err(error);
            }
        }
        self.database.commands = commands;
        Ok(())
    }

    fn preflight_command(
        &mut self,
        command_index: usize,
        command: crate::CompileCommand,
    ) -> Result<(), CoreError> {
        let Some(language) = self.select_language(command_index, &command) else {
            return Ok(());
        };
        let Some(source) = self.select_source(command_index, &command, language)? else {
            return Ok(());
        };
        self.sources
            .entry(source.relative.clone())
            .or_insert_with(|| source.clone());
        self.translation_units
            .push(pending_validation(command_index, command, language, source));
        Ok(())
    }

    fn select_language(
        &mut self,
        command_index: usize,
        command: &crate::CompileCommand,
    ) -> Option<ClangLanguage> {
        let Some(language) = ClangLanguage::classify(command) else {
            self.diagnostics
                .push(unclassified_diagnostic(command_index, command));
            self.skip(command_index, command, None, "unrecognized Clang language mode");
            return None;
        };
        if self.language_excluded(language) {
            self.skip(
                command_index,
                command,
                Some(language),
                "language excluded by request",
            );
            return None;
        }
        Some(language)
    }

    fn language_excluded(&self, language: ClangLanguage) -> bool {
        !self.request.languages.is_empty() && !self.request.languages.contains(&language.core_language())
    }

    fn select_source(
        &mut self,
        command_index: usize,
        command: &crate::CompileCommand,
        language: ClangLanguage,
    ) -> Result<Option<SourceFile>, CoreError> {
        let Some(canonical) = self.canonical_source(language, command_index, command) else {
            return Ok(None);
        };
        let Some(relative) = self.relative_source(command_index, command, language, &canonical) else {
            return Ok(None);
        };
        let test = language.core_language().is_test_path(&relative);
        if let Some(reason) = exclusion_reason(self.request, &relative, test) {
            self.skip(command_index, command, Some(language), reason);
            return Ok(None);
        }
        self.observe_source(&canonical)?;
        Ok(Some(SourceFile {
            generated: is_generated(Path::new(&relative)),
            path: canonical,
            relative,
            language: language.core_language(),
            test,
        }))
    }

    fn canonical_source(
        &mut self,
        language: ClangLanguage,
        command_index: usize,
        command: &crate::CompileCommand,
    ) -> Option<PathBuf> {
        match command.file.canonicalize() {
            Ok(file) => Some(file),
            Err(error) => {
                self.diagnostics.push(missing_source_diagnostic(command, &error));
                self.skip(
                    command_index,
                    command,
                    Some(language),
                    "translation-unit source does not exist",
                );
                None
            }
        }
    }

    fn relative_source(
        &mut self,
        command_index: usize,
        command: &crate::CompileCommand,
        language: ClangLanguage,
        canonical: &Path,
    ) -> Option<String> {
        if let Ok(relative) = canonical.strip_prefix(&self.root) {
            return Some(relative.to_string_lossy().replace('\\', "/"));
        }
        self.diagnostics
            .push(outside_root_diagnostic(canonical, &self.root));
        self.skip(
            command_index,
            command,
            Some(language),
            "translation unit is outside project root",
        );
        None
    }

    fn observe_source(&mut self, canonical: &Path) -> Result<(), CoreError> {
        let metadata = fs::metadata(canonical).map_err(|source| source_read_error(source, canonical))?;
        if !metadata.is_file() {
            return Err(CoreError::UnsafePath {
                path: canonical.display().to_string(),
                message: "selected translation unit is not a regular file".to_string(),
            });
        }
        if self.budgeted_sources.insert(canonical.to_path_buf()) {
            self.source_budget.observe(canonical, metadata.len())?;
        }
        Ok(())
    }

    fn skip(
        &mut self,
        command_index: usize,
        command: &crate::CompileCommand,
        language: Option<ClangLanguage>,
        reason: &str,
    ) {
        self.translation_units
            .push(not_validated(command_index, command.clone(), language, reason));
    }

    fn validate_units(&mut self) {
        let mut units = std::mem::take(&mut self.translation_units);
        for unit in &mut units {
            self.validate_unit(unit);
        }
        self.translation_units = units;
    }

    fn validate_unit(&mut self, unit: &mut TranslationUnit) {
        let (Some(language), Some(source)) = (unit.language, unit.source.clone()) else {
            return;
        };
        let (invocation, status, elapsed) = self.validation_result(unit, language);
        add_validation_diagnostic(
            &mut self.diagnostics,
            self.request,
            &source,
            &status,
            unit.command_index,
        );
        unit.invocation = invocation;
        unit.status = status;
        unit.elapsed = elapsed;
    }

    fn validation_result(
        &self,
        unit: &TranslationUnit,
        language: ClangLanguage,
    ) -> (Option<crate::SanitizedCommand>, ValidationStatus, Duration) {
        if self.validation.validate {
            return validate_translation_unit(
                &unit.command,
                language,
                &self.validation.compiler,
                self.validation.timeout,
            );
        }
        (
            sanitize_compile_command(&unit.command, &self.validation.compiler, language).ok(),
            ValidationStatus::NotValidated {
                message: "validation explicitly disabled".to_string(),
            },
            Duration::ZERO,
        )
    }

    fn add_selection_diagnostic(&mut self) {
        let message = if self.database.commands.is_empty() {
            Some("compilation database contains no translation-unit commands")
        } else if self.sources.is_empty() {
            Some("compilation database contains no translation units selected by this request")
        } else {
            None
        };
        if let Some(message) = message {
            self.diagnostics
                .push(diagnostic(Severity::Warning, message.to_string(), false));
        }
    }

    fn finish(self) -> ClangProject {
        let mut kinds = BTreeSet::new();
        kinds.insert(ProjectKind::CompilationDatabase);
        ClangProject {
            context: ProjectContext {
                root: self.root,
                kinds,
                sources: self.sources.into_values().collect(),
                backends: vec![self.backend],
                diagnostics: self.diagnostics,
            },
            database: self.database,
            translation_units: self.translation_units,
            compiler: self.validation.compiler,
            compiler_version: self.compiler_version,
        }
    }
}

fn source_read_error(source: std::io::Error, path: &Path) -> CoreError {
    CoreError::Read {
        source,
        path: format!("{}", path.display()),
    }
}

fn exclusion_reason(request: &AnalysisRequest, relative: &str, test: bool) -> Option<&'static str> {
    if !request.filters.is_empty() && !request.filters.iter().any(|filter| relative.contains(filter)) {
        return Some("source excluded by request filter");
    }
    if test && !request.include_tests {
        return Some("test source excluded by request");
    }
    None
}

fn canonical_project_root(root: &Path) -> Result<PathBuf, CoreError> {
    let canonical = root.canonicalize().map_err(|source| CoreError::Read {
        path: root.display().to_string(),
        source,
    })?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(CoreError::InvalidRoot {
            path: canonical.display().to_string(),
            message: "not a directory".to_string(),
        })
    }
}

fn compiler_details(validation: &ValidationConfiguration) -> (Option<String>, Vec<Diagnostic>) {
    if !validation.validate {
        return (
            None,
            vec![diagnostic(
                Severity::Info,
                "Clang translation-unit validation was explicitly disabled".to_string(),
                false,
            )],
        );
    }
    match probe_compiler_version(&validation.compiler, validation.timeout) {
        Ok(version) => (Some(version), Vec::new()),
        Err(message) => (None, vec![diagnostic(Severity::Error, message, false)]),
    }
}

fn configured_backend(adapter: &ClangAdapter, compiler_version: Option<&str>) -> BackendInfo {
    let mut backend = adapter.info();
    if let Some(version) = compiler_version {
        backend.version = version.to_string();
    }
    backend
}

fn database_diagnostic(validation: &ValidationConfiguration, database: &CompilationDatabase) -> Diagnostic {
    let message = format!(
        "loaded {} translation-unit command(s) from {}; compiler: {}",
        database.commands.len(),
        database.path.display(),
        validation.compiler.display()
    );
    diagnostic(Severity::Info, message, false)
}

fn unclassified_diagnostic(command_index: usize, command: &crate::CompileCommand) -> Diagnostic {
    diagnostic(
        Severity::Warning,
        format!(
            "could not classify translation unit {} from compilation-database entry {command_index}",
            command.file.display()
        ),
        false,
    )
}

fn missing_source_diagnostic(command: &crate::CompileCommand, error: &std::io::Error) -> Diagnostic {
    warning_diagnostic(format!(
        "skipped missing translation unit {}: {error}",
        command.file.display()
    ))
}

fn warning_diagnostic(message: String) -> Diagnostic {
    diagnostic(Severity::Warning, message, false)
}

fn outside_root_diagnostic(canonical: &Path, root: &Path) -> Diagnostic {
    diagnostic(
        Severity::Error,
        format!(
            "compilation-database entry {} resolves outside project root {}",
            canonical.display(),
            root.display()
        ),
        false,
    )
}

fn pending_validation(
    command_index: usize,
    command: crate::CompileCommand,
    language: ClangLanguage,
    source: SourceFile,
) -> TranslationUnit {
    TranslationUnit {
        command_index,
        command,
        language: Some(language),
        source: Some(source),
        invocation: None,
        status: ValidationStatus::NotValidated {
            message: "source-budget preflight complete; validation pending".to_string(),
        },
        elapsed: Duration::ZERO,
    }
}

impl ProjectBackend for ClangAdapter {
    fn info(&self) -> BackendInfo {
        BackendInfo::new(
            "clang",
            env!("CARGO_PKG_VERSION"),
            true,
            [
                Capability::Syntax,
                Capability::Functions,
                Capability::Complexity,
                Capability::ProjectSemantics,
                Capability::ParseValidation,
            ],
        )
    }

    fn supports(&self, project: ProjectKind) -> bool {
        project == ProjectKind::CompilationDatabase
    }

    fn resolve(&self, request: &AnalysisRequest) -> Result<ProjectContext, CoreError> {
        let ClangProject { context, .. } = self.validate_project(request)?;
        Ok(context)
    }
}

fn not_validated(
    command_index: usize,
    command: crate::CompileCommand,
    language: Option<ClangLanguage>,
    message: &str,
) -> TranslationUnit {
    TranslationUnit {
        command,
        command_index,
        invocation: None,
        source: None,
        language,
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
        ClangAdapterError::Read {
            operation: _,
            path,
            source,
        } => CoreError::Read { path, source },
        other => CoreError::Parse {
            path: "compile_commands.json".to_string(),
            message: other.to_string(),
        },
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::{
        compilation_entry, create_dir, expect_error, source_file, temp_dir, write, write_executable,
        write_json_database,
    };

    fn write_compiler(path: &Path) {
        write_executable(
            path,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'fake clang 1.0'; exit 0; fi\nexit 0\n",
        );
    }

    fn write_command_database(root: &Path, file: &Path, arguments: &[&str]) {
        let database = serde_json::Value::Array(vec![compilation_entry(root, file, arguments)]);
        write_json_database(root, &database);
    }

    fn resolve_without_validation(root: &Path) -> Result<ClangProject, CoreError> {
        ClangAdapter::default()
            .with_validation(false)
            .validate_project(&AnalysisRequest::new(root.to_path_buf()))
    }

    fn source_project(file: &str, contents: Option<&str>, arguments: &[&str]) -> tempfile::TempDir {
        let temp = temp_dir();
        let source = temp.path().join(file);
        if let Some(parent) = source.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| panic!("create source parent fixture: {error}"));
        }
        if let Some(contents) = contents {
            write(&source, contents);
        } else {
            create_dir(&source);
        }
        write_command_database(temp.path(), Path::new(file), arguments);
        temp
    }

    #[test]
    fn resolves_database_sources_and_preserves_validation_provenance() {
        let temp = source_project(
            "src/main.c",
            Some("int main(void) { return 0; }"),
            &crate::fixture_words("ccache clang -MD -MF main.d -c src/main.c -o main.o"),
        );
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
        let temp = temp_dir();
        let error = expect_error(
            ClangAdapter::default().validate_project(&AnalysisRequest::new(temp.path().to_path_buf())),
        );
        assert!(matches!(error, CoreError::BackendUnavailable { .. }));
        assert!(!temp.path().join("compile_commands.json").exists());
    }

    #[test]
    fn oversized_selected_translation_unit_is_a_typed_error() {
        let temp = source_project(
            "large.c",
            Some("int value(void) { return 123456789; }\n"),
            &["clang", "-c", "large.c"],
        );

        let mut request = AnalysisRequest::new(temp.path().to_path_buf());
        request.max_source_bytes = 8;
        let error = expect_error(
            ClangAdapter::default()
                .with_validation(false)
                .validate_project(&request),
        );
        assert!(matches!(
            error,
            CoreError::SourceTooLarge {
                actual_bytes: 38,
                max_source_bytes: 8,
                ..
            }
        ));
    }

    #[test]
    fn rejects_selected_translation_units_that_are_not_regular_files() {
        let temp = source_project("source.c", None, &["clang", "source.c"]);
        let CoreError::UnsafePath { .. } = expect_error(resolve_without_validation(temp.path())) else {
            panic!("expected unsafe-path error");
        };
    }

    #[test]
    fn selection_reasons_cover_filters_and_test_policy() {
        let mut request = AnalysisRequest::new(PathBuf::from("/project"));
        assert_eq!(exclusion_reason(&request, "src/main.c", false), None);

        request.filters.push("src/selected".to_string());
        assert_eq!(
            exclusion_reason(&request, "src/main.c", false),
            Some("source excluded by request filter")
        );
        assert_eq!(exclusion_reason(&request, "src/selected.c", false), None);

        request.filters.clear();
        assert_eq!(
            exclusion_reason(&request, "tests/main.c", true),
            Some("test source excluded by request")
        );
        request.include_tests = true;
        assert_eq!(exclusion_reason(&request, "tests/main.c", true), None);
    }

    #[test]
    fn validation_diagnostics_preserve_status_and_parse_error_policy() {
        let source = source_file("/project/source.c", "source.c", reporigor_core::Language::C);
        let mut request = AnalysisRequest::new(PathBuf::from("/project"));
        let mut diagnostics = Vec::new();
        add_validation_diagnostic(&mut diagnostics, &request, &source, &ValidationStatus::Valid, 1);
        add_validation_diagnostic(
            &mut diagnostics,
            &request,
            &source,
            &ValidationStatus::NotValidated {
                message: "disabled".to_string(),
            },
            1,
        );
        assert!(diagnostics.is_empty());

        add_validation_diagnostic(
            &mut diagnostics,
            &request,
            &source,
            &ValidationStatus::Invalid {
                exit_code: Some(2),
                stderr: String::new(),
            },
            1,
        );
        request.allow_parse_errors = true;
        add_validation_diagnostic(
            &mut diagnostics,
            &request,
            &source,
            &ValidationStatus::Invalid {
                exit_code: None,
                stderr: "parse detail".to_string(),
            },
            2,
        );
        for kind in ["timeout", "unavailable", "rejected"] {
            let status = match kind {
                "timeout" => ValidationStatus::TimedOut {
                    timeout: Duration::from_millis(10),
                    stderr: "timeout detail".to_string(),
                },
                "unavailable" => ValidationStatus::Unavailable {
                    message: "missing".to_string(),
                },
                _ => ValidationStatus::Rejected {
                    message: "unsafe".to_string(),
                },
            };
            add_validation_diagnostic(&mut diagnostics, &request, &source, &status, 3);
        }
        assert_eq!(diagnostics.len(), 5);
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert_eq!(diagnostics[1].severity, Severity::Warning);
        assert!(diagnostics[0].message.contains("no compiler diagnostics"));
        assert!(diagnostics
            .iter()
            .all(|diagnostic| diagnostic.message.contains("source.c")));
    }

    #[test]
    fn adapter_errors_map_to_typed_core_categories() {
        let invalid = map_adapter_error(ClangAdapterError::InvalidRoot {
            path: "/bad".to_string(),
        });
        assert!(matches!(invalid, CoreError::InvalidRoot { .. }));

        let read = map_adapter_error(ClangAdapterError::Read {
            operation: "read",
            path: "/bad".to_string(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        });
        match read {
            CoreError::Read { .. } => {}
            other => panic!("expected read error, got {other}"),
        }

        let CoreError::Parse { .. } = map_adapter_error(ClangAdapterError::InvalidEntry {
            path: "/bad".to_string(),
            index: 0,
            message: "invalid".to_string(),
        }) else {
            panic!("expected parse error");
        };
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn outside_root_translation_unit_is_rejected() {
        let project = temp_dir();
        let outside = temp_dir();
        let external = outside.path().join("external.c");
        write(&external, "int external;");
        write_command_database(project.path(), &external, &["clang", &external.to_string_lossy()]);

        let resolution = resolve_without_validation(project.path())
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

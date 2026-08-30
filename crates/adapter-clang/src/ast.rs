use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    mem,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use reporigor_core::{
    AnalysisRequest, AnalysisSnapshot, CoreError, Diagnostic, FunctionRecord, Severity, SourceFile,
};
use serde::Deserialize;

use crate::validation::{run_bounded_capture, ProcessOutcome};
use crate::{
    sanitize_compile_command, ClangAdapter, ClangLanguage, ClangProject, CompileCommand, SanitizedCommand,
    ValidationStatus,
};

const AST_STDERR_LIMIT: usize = 64 * 1024;
const MAX_AST_LOCATION_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_AST_LINE_START_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Result of the JSON AST command for one compilation-database entry.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum AstDumpStatus {
    Analyzed,
    CommandFailed { exit_code: Option<i32>, stderr: String },
    Rejected { message: String },
    TimedOut { stderr: String, timeout: Duration },
    OutputTooLarge { limit: usize },
    Unavailable { message: String },
    InvalidJson { message: String },
    NotRun { message: String },
}

/// Native AST extraction provenance for one translation unit.
#[derive(Debug, Clone)]
#[must_use = "inspect the AST provenance and extracted functions"]
pub struct AstTranslationUnit {
    #[doc = "Zero-based index of the originating compilation-database entry."]
    pub command_index: usize,
    pub command: CompileCommand,
    #[doc = "Canonical source selected for AST extraction, when available."]
    pub source: Option<SourceFile>,
    pub language: Option<ClangLanguage>,
    #[doc = "Exact sanitized compiler invocation used for extraction."]
    pub invocation: Option<SanitizedCommand>,
    pub status: AstDumpStatus,
    /// Wall-clock time spent on the AST command.
    pub elapsed: Duration,
    pub stderr: String,
    /// Functions extracted from this translation unit.
    pub functions: Vec<FunctionRecord>,
}

/// Standard analysis output plus the compilation database, validation records,
/// and exact JSON AST command provenance used to produce it.
#[derive(Debug, Clone)]
#[must_use]
pub struct ClangAnalysis {
    pub translation_units: Vec<AstTranslationUnit>,
    pub snapshot: AnalysisSnapshot,
    pub project: ClangProject,
}

impl ClangAdapter {
    /// Extract functions and native cyclomatic complexity with Clang's JSON AST
    /// while retaining a detailed validation/AST provenance record.
    ///
    /// # Errors
    ///
    /// Returns an error when project discovery or compilation-database loading
    /// fails. Per-translation-unit compiler, timeout, size, and JSON failures are
    /// represented in the returned diagnostics and provenance instead.
    pub fn analyze_project_with_provenance(
        &self,
        request: &AnalysisRequest,
    ) -> Result<ClangAnalysis, CoreError> {
        let project = self.validate_project(request)?;
        Ok(AstAnalysisState::new(&project).analyze(self, project))
    }

    /// Extract native Clang functions and complexity into the shared analysis
    /// model. Use [`Self::analyze_project_with_provenance`] when exact commands
    /// and per-translation-unit statuses are also required.
    ///
    /// # Errors
    ///
    /// Returns an error when project discovery or compilation-database loading
    /// fails. Translation-unit failures are returned as snapshot diagnostics.
    pub fn analyze_project(&self, request: &AnalysisRequest) -> Result<AnalysisSnapshot, CoreError> {
        self.analyze_project_with_provenance(request)
            .map(|analysis| analysis.snapshot)
    }
}

struct AstAnalysisState {
    snapshot: AnalysisSnapshot,
    records: BTreeMap<(String, String), FunctionRecord>,
    translation_units: Vec<AstTranslationUnit>,
}

impl AstAnalysisState {
    fn new(project: &ClangProject) -> Self {
        Self {
            snapshot: AnalysisSnapshot {
                files: project.context.sources.clone(),
                backends: project.context.backends.clone(),
                diagnostics: project.context.diagnostics.clone(),
                ..AnalysisSnapshot::default()
            },
            records: BTreeMap::new(),
            translation_units: Vec::with_capacity(project.translation_units.len()),
        }
    }

    fn analyze(mut self, adapter: &ClangAdapter, project: ClangProject) -> ClangAnalysis {
        for unit in &project.translation_units {
            self.analyze_unit(adapter, &project, unit);
        }
        self.snapshot.functions = self.records.into_values().collect();
        ClangAnalysis {
            snapshot: self.snapshot,
            project,
            translation_units: self.translation_units,
        }
    }

    fn analyze_unit(
        &mut self,
        adapter: &ClangAdapter,
        project: &ClangProject,
        unit: &crate::TranslationUnit,
    ) {
        let Some(source) = unit.source.as_ref() else {
            self.translation_units
                .push(not_run(unit, "translation unit was not selected for analysis"));
            return;
        };
        let Some(language) = unit.language else {
            self.translation_units
                .push(not_run(unit, "translation unit language is unknown"));
            return;
        };
        if !validation_allows_ast(&unit.status) {
            self.record_not_run(unit);
            return;
        }
        let ast_unit = dump_translation_unit(&AstDumpRequest {
            command_index: unit.command_index,
            command: &unit.command,
            source,
            language,
            compiler: adapter.compiler(),
            timeout: adapter.timeout(),
            output_limit: adapter.ast_output_limit(),
            root: &project.context.root,
        });
        self.record_dump(source, ast_unit);
    }

    fn record_not_run(&mut self, unit: &crate::TranslationUnit) {
        self.snapshot.parse_errors = self.snapshot.parse_errors.saturating_add(1);
        self.translation_units.push(not_run(
            unit,
            "Clang syntax validation did not succeed; JSON AST was not requested",
        ));
    }

    fn record_dump(&mut self, source: &SourceFile, ast_unit: AstTranslationUnit) {
        if ast_unit.status != AstDumpStatus::Analyzed {
            self.snapshot.parse_errors = self.snapshot.parse_errors.saturating_add(1);
            self.snapshot
                .diagnostics
                .push(ast_diagnostic(source, ast_unit.command_index, &ast_unit.status));
        }
        for function in &ast_unit.functions {
            self.merge_function(function);
        }
        self.translation_units.push(ast_unit);
    }

    fn merge_function(&mut self, function: &FunctionRecord) {
        self.records
            .entry((function.file.clone(), function.stable_symbol.clone()))
            .and_modify(|record| record.complexity = record.complexity.max(function.complexity))
            .or_insert_with(|| function.clone());
    }
}

fn validation_allows_ast(status: &ValidationStatus) -> bool {
    matches!(
        status,
        ValidationStatus::Valid | ValidationStatus::NotValidated { .. }
    )
}

fn not_run(unit: &crate::TranslationUnit, message: &str) -> AstTranslationUnit {
    AstTranslationUnit {
        command_index: unit.command_index,
        command: unit.command.clone(),
        source: unit.source.clone(),
        language: unit.language,
        invocation: None,
        status: AstDumpStatus::NotRun {
            message: message.to_string(),
        },
        elapsed: Duration::ZERO,
        stderr: String::new(),
        functions: Vec::new(),
    }
}

struct AstDumpRequest<'a> {
    command_index: usize,
    command: &'a CompileCommand,
    source: &'a SourceFile,
    language: ClangLanguage,
    compiler: &'a Path,
    timeout: Duration,
    output_limit: usize,
    root: &'a Path,
}

fn dump_translation_unit(request: &AstDumpRequest<'_>) -> AstTranslationUnit {
    let mut invocation = match ast_dump_command(request.command, request.compiler, request.language) {
        Ok(invocation) => invocation,
        Err(error) => return rejected_dump(request, error.to_string()),
    };
    add_ast_dump_flags(&mut invocation);
    let started = Instant::now();
    let outcome = run_bounded_capture(
        &invocation,
        request.timeout,
        Some(request.output_limit),
        AST_STDERR_LIMIT,
    );
    let elapsed = started.elapsed();
    let output = interpret_outcome(outcome, request);
    completed_dump(request, Some(invocation), elapsed, output)
}

fn rejected_dump(request: &AstDumpRequest<'_>, message: String) -> AstTranslationUnit {
    completed_dump(
        request,
        None,
        Duration::ZERO,
        AstDumpOutput {
            status: AstDumpStatus::Rejected { message },
            stderr: String::new(),
            functions: Vec::new(),
        },
    )
}

struct AstDumpOutput {
    status: AstDumpStatus,
    stderr: String,
    functions: Vec<FunctionRecord>,
}

fn interpret_outcome(outcome: ProcessOutcome, request: &AstDumpRequest<'_>) -> AstDumpOutput {
    match outcome {
        ProcessOutcome::Exited {
            success,
            exit_code,
            stdout,
            stderr,
        } => interpret_exit(success, exit_code, &stdout, stderr, request),
        ProcessOutcome::TimedOut { stderr } => timed_out_dump(stderr, request.timeout),
        ProcessOutcome::Unavailable(message) => AstDumpOutput {
            status: AstDumpStatus::Unavailable {
                message: message.clone(),
            },
            stderr: message,
            functions: Vec::new(),
        },
    }
}

fn interpret_exit(
    success: bool,
    exit_code: Option<i32>,
    stdout: &crate::validation::CapturedOutput,
    stderr: crate::validation::CapturedOutput,
    request: &AstDumpRequest<'_>,
) -> AstDumpOutput {
    let stderr_text = annotate_truncation(stderr.text, stderr.truncated);
    if !success {
        return AstDumpOutput {
            status: AstDumpStatus::CommandFailed {
                exit_code,
                stderr: stderr_text.clone(),
            },
            stderr: stderr_text,
            functions: Vec::new(),
        };
    }
    if stdout.truncated {
        return AstDumpOutput {
            status: AstDumpStatus::OutputTooLarge {
                limit: request.output_limit,
            },
            stderr: stderr_text,
            functions: Vec::new(),
        };
    }
    parse_dump(&stdout.text, stderr_text, request)
}

fn parse_dump(json: &str, stderr: String, request: &AstDumpRequest<'_>) -> AstDumpOutput {
    match parse_ast(json) {
        Ok(ast) => AstDumpOutput {
            status: AstDumpStatus::Analyzed,
            stderr,
            functions: extract_functions(
                &ast,
                request.root,
                request.command,
                request.source,
                request.language,
            ),
        },
        Err(error) => AstDumpOutput {
            status: AstDumpStatus::InvalidJson {
                message: error.to_string(),
            },
            stderr,
            functions: Vec::new(),
        },
    }
}

fn timed_out_dump(stderr: crate::validation::CapturedOutput, timeout: Duration) -> AstDumpOutput {
    let stderr = annotate_truncation(stderr.text, stderr.truncated);
    AstDumpOutput {
        status: AstDumpStatus::TimedOut {
            timeout,
            stderr: stderr.clone(),
        },
        stderr,
        functions: Vec::new(),
    }
}

fn completed_dump(
    request: &AstDumpRequest<'_>,
    invocation: Option<SanitizedCommand>,
    elapsed: Duration,
    output: AstDumpOutput,
) -> AstTranslationUnit {
    AstTranslationUnit {
        command_index: request.command_index,
        command: request.command.clone(),
        source: Some(request.source.clone()),
        language: Some(request.language),
        invocation,
        status: output.status,
        elapsed,
        stderr: output.stderr,
        functions: output.functions,
    }
}

fn parse_ast(value: &str) -> Result<AstNode, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_str(value);
    deserializer.disable_recursion_limit();
    let ast = AstNode::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(ast)
}

fn ast_dump_command(
    command: &CompileCommand,
    compiler: &Path,
    language: ClangLanguage,
) -> Result<SanitizedCommand, crate::SanitizedCommandError> {
    sanitize_compile_command(command, compiler, language)
}

fn add_ast_dump_flags(invocation: &mut SanitizedCommand) {
    let insertion = 3.min(invocation.arguments.len());
    invocation.arguments.splice(
        insertion..insertion,
        ["-Xclang".to_string(), "-ast-dump=json".to_string()],
    );
}

fn annotate_truncation(mut value: String, truncated: bool) -> String {
    if truncated {
        if !value.is_empty() {
            value.push('\n');
        }
        value.push_str("[compiler output truncated]");
    }
    value
}

fn ast_diagnostic(source: &SourceFile, command_index: usize, status: &AstDumpStatus) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        backend: "clang".to_string(),
        message: ast_status_message(source, command_index, status),
        location: None,
        fallback_used: false,
    }
}

fn ast_status_message(source: &SourceFile, command_index: usize, status: &AstDumpStatus) -> String {
    match status {
        AstDumpStatus::Analyzed => format!("Clang analyzed {}", source.relative),
        AstDumpStatus::CommandFailed { exit_code, stderr } => format!(
            "Clang JSON AST failed for {} (database entry {command_index}, exit {:?}): {}",
            source.relative,
            exit_code,
            fallback_message(stderr)
        ),
        AstDumpStatus::TimedOut { timeout, stderr } => format!(
            "Clang JSON AST timed out for {} after {:.3}s (database entry {command_index}): {}",
            source.relative,
            timeout.as_secs_f64(),
            fallback_message(stderr)
        ),
        _ => other_ast_status_message(source, command_index, status),
    }
}

fn other_ast_status_message(source: &SourceFile, command_index: usize, status: &AstDumpStatus) -> String {
    let subject = &source.relative;
    match status {
        AstDumpStatus::Unavailable { message } => {
            format!("Clang JSON AST unavailable for {subject} (database entry {command_index}): {message}")
        }
        AstDumpStatus::Rejected { message } => format!(
            "refused unsafe Clang JSON AST command for {subject} (database entry {command_index}): {message}"
        ),
        AstDumpStatus::OutputTooLarge { limit } => format!(
            "Clang JSON AST for {subject} exceeded the {limit}-byte output limit (database entry {command_index})"
        ),
        AstDumpStatus::InvalidJson { message } => format!(
            "Clang returned invalid JSON AST for {subject} (database entry {command_index}): {message}"
        ),
        AstDumpStatus::NotRun { message } => format!(
            "Clang JSON AST was not run for {subject} (database entry {command_index}): {message}"
        ),
        AstDumpStatus::Analyzed | AstDumpStatus::CommandFailed { .. } | AstDumpStatus::TimedOut { .. } => {
            unreachable!("handled by ast_status_message")
        }
    }
}

fn fallback_message(value: &str) -> &str {
    if value.trim().is_empty() {
        "no compiler diagnostics"
    } else {
        value
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AstNode {
    id: Option<String>,
    kind: String,
    name: Option<String>,
    opcode: Option<String>,
    #[serde(rename = "type")]
    type_info: Option<AstType>,
    #[serde(rename = "mangledName")]
    mangled_name: Option<String>,
    #[serde(rename = "parentDeclContextId")]
    parent_context_id: Option<String>,
    loc: Option<AstPoint>,
    range: Option<AstRange>,
    inner: Vec<Self>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct AstType {
    #[serde(rename = "qualType")]
    qualified: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AstRange {
    begin: AstPoint,
    end: AstPoint,
}

#[derive(Debug, Default, Deserialize)]
#[doc = "Raw Clang source location with spelling and expansion provenance."]
#[serde(default)]
struct AstPoint {
    file: Option<String>,
    line: Option<u64>,
    offset: Option<u64>,
    #[serde(rename = "spellingLoc")]
    spelling: Option<Box<Self>>,
    #[serde(rename = "expansionLoc")]
    expansion: Option<Box<Self>>,
}

impl AstPoint {
    fn preferred(&self) -> &Self {
        self.expansion
            .as_deref()
            .or(self.spelling.as_deref())
            .map_or(self, Self::preferred)
    }

    fn file(&self) -> Option<&str> {
        self.preferred().file.as_deref().or(self.file.as_deref())
    }

    fn line(&self) -> Option<u64> {
        self.preferred().line.or(self.line)
    }

    fn offset(&self) -> Option<u64> {
        self.preferred().offset.or(self.offset)
    }
}

fn extract_functions(
    ast: &AstNode,
    root: &Path,
    command: &CompileCommand,
    source: &SourceFile,
    language: ClangLanguage,
) -> Vec<FunctionRecord> {
    let mut context_names = BTreeMap::new();
    collect_context_names(ast, &mut Vec::new(), &mut context_names);
    let mut extractor = FunctionExtractor {
        root,
        directory: &command.directory,
        default_source: &source.path,
        language,
        line_starts: BTreeMap::new(),
        line_start_cache_bytes: 0,
        context_names,
        records: Vec::new(),
    };
    extractor.visit(ast, &mut Vec::new());
    extractor
        .records
        .sort_by(reporigor_core::compare_function_records);
    extractor
        .records
        .dedup_by(|left, right| left.file == right.file && left.stable_symbol == right.stable_symbol);
    extractor.records
}

struct FunctionExtractor<'a> {
    root: &'a Path,
    directory: &'a Path,
    default_source: &'a Path,
    language: ClangLanguage,
    line_starts: BTreeMap<PathBuf, Vec<usize>>,
    line_start_cache_bytes: usize,
    context_names: BTreeMap<String, String>,
    records: Vec<FunctionRecord>,
}

impl FunctionExtractor<'_> {
    fn visit(&mut self, node: &AstNode, scope: &mut Vec<String>) {
        // Anonymous executable bodies do not have stable cross-backend names,
        // so neither they nor Clang's implicit lambda `operator()` children
        // become shared function records.
        if is_anonymous_function_kind(&node.kind) {
            return;
        }
        if is_function_kind(&node.kind) {
            self.record_function(node, scope);
            // Local declarations and local-class methods are outside the
            // canonical file/module/type-owned function metric domain.
            return;
        }

        let pushed = push_scope(node, scope);
        for child in &node.inner {
            self.visit(child, scope);
        }
        if pushed {
            scope.pop();
        }
    }

    fn record_function(&mut self, node: &AstNode, scope: &[String]) {
        if let Some(record) = self.function_record(node, scope) {
            self.records.push(record);
        }
    }

    fn function_record(&mut self, node: &AstNode, scope: &[String]) -> Option<FunctionRecord> {
        let body = node.inner.iter().find(|child| is_body_kind(&child.kind))?;
        let location = self.function_location(node)?;
        let name = self.function_name(scope, node)?;
        let stable_symbol = structural_symbol(node, &name);
        let mut record = FunctionRecord::new(
            self.language.core_language(),
            name,
            location.relative,
            location.start_line,
            location.end_line,
            1_u32.saturating_add(decision_count(body)),
        );
        record.stable_symbol = stable_symbol;
        record.coverage_excluded_ranges = self.nested_coverage_ranges(body, &location.path);
        Some(record)
    }

    fn function_location(&mut self, node: &AstNode) -> Option<FunctionLocation> {
        let path = self.function_path(node)?;
        let relative = path
            .strip_prefix(self.root)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/");
        let (start_line, end_line) = self.function_lines(node, &path);
        Some(FunctionLocation {
            path,
            relative,
            start_line,
            end_line,
        })
    }

    fn function_lines(&mut self, node: &AstNode, path: &Path) -> (u32, u32) {
        let range = node.range.as_ref();
        let start = range
            .and_then(|value| self.point_line(&value.begin, path))
            .or_else(|| node.loc.as_ref().and_then(|value| self.point_line(value, path)))
            .unwrap_or(1);
        let end = range
            .and_then(|value| self.point_line(&value.end, path))
            .unwrap_or(start)
            .max(start);
        (start, end)
    }

    fn function_name(&self, scope: &[String], node: &AstNode) -> Option<String> {
        let raw_name = node.name.as_deref()?.trim();
        if raw_name.is_empty() {
            return None;
        }
        if raw_name.contains("::") {
            return Some(raw_name.to_string());
        }
        let owner = self.contextual_scope(node, scope);
        Some(owner.map_or_else(|| raw_name.to_string(), |owner| format!("{owner}::{raw_name}")))
    }

    fn contextual_scope(&self, node: &AstNode, scope: &[String]) -> Option<String> {
        let owner = if scope.is_empty() {
            node.parent_context_id
                .as_ref()
                .and_then(|id| self.context_names.get(id))
                .cloned()
        } else {
            Some(scope.join("::"))
        };
        owner.filter(|value| !value.is_empty())
    }

    fn nested_coverage_ranges(&mut self, root: &AstNode, path: &Path) -> Vec<(u32, u32)> {
        let mut output = Vec::new();
        collect_nested_ranges(self, root, path, &mut output);
        output.sort_unstable();
        output.dedup();
        output
    }

    fn function_path(&self, node: &AstNode) -> Option<PathBuf> {
        let location = node
            .loc
            .as_ref()
            .and_then(AstPoint::file)
            .or_else(|| node.range.as_ref().and_then(|range| range.begin.file()));
        let path = source_path(location, self.directory, self.default_source)?;
        let canonical = path.canonicalize().ok()?;
        canonical.strip_prefix(self.root).ok()?;
        Some(canonical)
    }

    fn point_line(&mut self, point: &AstPoint, path: &Path) -> Option<u32> {
        if let Some(line) = point.line() {
            return u32::try_from(line).ok();
        }
        let offset = usize::try_from(point.offset()?).ok()?;
        self.ensure_line_starts(path)?;
        let starts = self.line_starts.get(path)?;
        u32::try_from(starts.partition_point(|start| *start <= offset)).ok()
    }

    fn ensure_line_starts(&mut self, path: &Path) -> Option<()> {
        if self.line_starts.contains_key(path) {
            return Some(());
        }
        let remaining = MAX_AST_LINE_START_CACHE_BYTES.saturating_sub(self.line_start_cache_bytes);
        let (starts, cache_bytes) = bounded_line_starts(self.root, path, remaining)?;
        self.line_start_cache_bytes = self.line_start_cache_bytes.checked_add(cache_bytes)?;
        self.line_starts.insert(path.to_path_buf(), starts);
        Some(())
    }
}

struct FunctionLocation {
    path: PathBuf,
    relative: String,
    start_line: u32,
    end_line: u32,
}

fn structural_symbol(node: &AstNode, name: &str) -> String {
    let signature = node
        .type_info
        .as_ref()
        .and_then(|value| value.qualified.as_deref())
        .map(normalize_signature)
        .filter(|value| !value.is_empty())
        .or_else(|| node.mangled_name.clone());
    signature.map_or_else(|| name.to_string(), |value| format!("{name}[type:{value}]"))
}

fn normalize_signature(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collect_nested_ranges(
    extractor: &mut FunctionExtractor<'_>,
    node: &AstNode,
    path: &Path,
    output: &mut Vec<(u32, u32)>,
) {
    for child in &node.inner {
        if is_executable_kind(&child.kind) {
            push_nested_range(output, child, extractor, path);
        } else {
            collect_nested_ranges(extractor, child, path, output);
        }
    }
}

fn push_nested_range(
    output: &mut Vec<(u32, u32)>,
    node: &AstNode,
    extractor: &mut FunctionExtractor<'_>,
    path: &Path,
) {
    let Some(range) = node.range.as_ref() else {
        return;
    };
    let Some(start) = extractor.point_line(&range.begin, path) else {
        return;
    };
    let Some(end) = extractor.point_line(&range.end, path) else {
        return;
    };
    output.push((start, end.max(start)));
}

fn is_executable_kind(kind: &str) -> bool {
    is_function_kind(kind) || is_anonymous_function_kind(kind)
}

fn source_path(location: Option<&str>, directory: &Path, default_source: &Path) -> Option<PathBuf> {
    let Some(file) = location else {
        return Some(default_source.to_path_buf());
    };
    if file.starts_with('<') {
        return None;
    }
    let path = Path::new(file);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        directory.join(path)
    })
}

fn bounded_line_starts(root: &Path, path: &Path, max_cache_bytes: usize) -> Option<(Vec<usize>, usize)> {
    let file = bounded_ast_source(root, path)?;
    let max_starts = maximum_line_starts(max_cache_bytes)?;
    let starts = read_line_starts(file, max_starts)?;
    let cache_bytes = starts.len().checked_mul(mem::size_of::<usize>())?;
    Some((starts, cache_bytes))
}

fn bounded_ast_source(root: &Path, path: &Path) -> Option<File> {
    let canonical = path.canonicalize().ok()?;
    canonical.strip_prefix(root).ok()?;
    let file = File::open(canonical).ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.is_file() && metadata.len() <= MAX_AST_LOCATION_SOURCE_BYTES).then_some(file)
}

fn maximum_line_starts(max_cache_bytes: usize) -> Option<usize> {
    let maximum = max_cache_bytes.checked_div(mem::size_of::<usize>())?;
    (maximum > 0).then_some(maximum)
}

fn read_line_starts(mut file: File, maximum: usize) -> Option<Vec<usize>> {
    let mut starts = vec![0];
    let mut bytes_read = 0_u64;
    let mut buffer = [0_u8; 8 * 1024];
    let mut limited = file
        .by_ref()
        .take(MAX_AST_LOCATION_SOURCE_BYTES.saturating_add(1));
    while let LineChunk::Data { count, chunk_start } =
        read_line_chunk(&mut limited, &mut buffer, &mut bytes_read)?
    {
        append_line_starts(&buffer[..count], chunk_start, maximum, &mut starts)?;
    }
    Some(starts)
}

enum LineChunk {
    End,
    Data { count: usize, chunk_start: u64 },
}

fn read_line_chunk(reader: &mut impl Read, buffer: &mut [u8], bytes_read: &mut u64) -> Option<LineChunk> {
    let count = reader.read(buffer).ok()?;
    if count == 0 {
        return Some(LineChunk::End);
    }
    let chunk_start = *bytes_read;
    *bytes_read = bytes_read.checked_add(count as u64)?;
    if *bytes_read > MAX_AST_LOCATION_SOURCE_BYTES {
        return None;
    }
    Some(LineChunk::Data { count, chunk_start })
}

fn append_line_starts(chunk: &[u8], chunk_start: u64, maximum: usize, starts: &mut Vec<usize>) -> Option<()> {
    for (index, byte) in chunk.iter().enumerate() {
        if *byte == b'\n' {
            append_line_start(index, chunk_start, maximum, starts)?;
        }
    }
    Some(())
}

fn append_line_start(index: usize, chunk_start: u64, maximum: usize, starts: &mut Vec<usize>) -> Option<()> {
    if starts.len() >= maximum {
        return None;
    }
    let offset = chunk_start
        .checked_add(u64::try_from(index).ok()?)?
        .checked_add(1)?;
    starts.push(usize::try_from(offset).ok()?);
    Some(())
}

fn collect_context_names(node: &AstNode, scope: &mut Vec<String>, contexts: &mut BTreeMap<String, String>) {
    let pushed = push_scope(node, scope);
    if is_scope_kind(&node.kind) {
        if let Some(id) = &node.id {
            contexts.insert(id.clone(), scope.join("::"));
        }
    }
    for child in &node.inner {
        collect_context_names(child, scope, contexts);
    }
    if pushed {
        scope.pop();
    }
}

fn push_scope(node: &AstNode, scope: &mut Vec<String>) -> bool {
    match node
        .name
        .as_ref()
        .filter(|name| is_scope_kind(&node.kind) && scope.last() != Some(*name))
    {
        Some(name) => {
            scope.push(name.clone());
            true
        }
        None => false,
    }
}

fn is_function_kind(kind: &str) -> bool {
    crate::word_list_contains(r"FunctionDecl CXXMethodDecl ObjCMethodDecl", kind)
}

fn is_anonymous_function_kind(kind: &str) -> bool {
    matches!(kind, "LambdaExpr" | "BlockExpr" | "BlockDecl")
}

fn is_body_kind(kind: &str) -> bool {
    ["CompoundStmt", "CXXTryStmt", "CoroutineBodyStmt", "SEHTryStmt"].contains(&kind)
}

fn is_scope_kind(kind: &str) -> bool {
    crate::word_list_contains(
        r"NamespaceDecl CXXRecordDecl ClassTemplateSpecializationDecl ObjCInterfaceDecl ObjCImplementationDecl ObjCCategoryDecl ObjCCategoryImplDecl",
        kind,
    )
}

fn decision_count(node: &AstNode) -> u32 {
    if is_function_kind(&node.kind) || is_anonymous_function_kind(&node.kind) {
        return 0;
    }
    let own = u32::from(is_decision_node(node));
    node.inner
        .iter()
        .fold(own, |count, child| count.saturating_add(decision_count(child)))
}

fn is_decision_node(node: &AstNode) -> bool {
    crate::word_list_contains(
        r"IfStmt ForStmt WhileStmt DoStmt CXXForRangeStmt ObjCForCollectionStmt CaseStmt ConditionalOperator BinaryConditionalOperator CXXCatchStmt ObjCAtCatchStmt SEHExceptStmt",
        &node.kind,
    ) || (node.kind == "BinaryOperator" && matches!(node.opcode.as_deref(), Some("&&" | "||")))
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::process::Command;

    use reporigor_core::Language;

    use super::*;
    use crate::{
        test_support::{
            compilation_entry, source_file, temp_dir, write, write_executable, write_file,
            write_json_database,
        },
        CommandOrigin,
    };

    fn fixture(extension: &str, language: ClangLanguage, json: &str) -> Vec<FunctionRecord> {
        let temp = temp_dir();
        let source = temp.path().join(format!("sample.{extension}"));
        write(&source, "line1\nline2\nline3\nline4\nline5\n");
        let source = source
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonical source: {error}"));
        let ast: AstNode =
            serde_json::from_str(json).unwrap_or_else(|error| panic!("parse fixture AST: {error}"));
        let arguments = vec!["clang".to_string(), source.display().to_string()];
        let command = CompileCommand {
            directory: temp
                .path()
                .canonicalize()
                .unwrap_or_else(|error| panic!("canonical temp: {error}")),
            file: source.clone(),
            arguments: arguments.clone(),
            output: None,
            origin: CommandOrigin::Arguments(arguments),
        };
        let source_file = source_file(source, &format!("sample.{extension}"), language.core_language());
        let root = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonical root: {error}"));
        extract_functions(&ast, &root, &command, &source_file, language)
    }

    fn analyze(adapter: &ClangAdapter, root: &Path) -> ClangAnalysis {
        adapter
            .analyze_project_with_provenance(&AnalysisRequest::new(root.to_path_buf()))
            .unwrap_or_else(|error| panic!("analyze project: {error}"))
    }

    fn message_status(kind: &str, message: &str) -> AstDumpStatus {
        match kind {
            "unavailable" => AstDumpStatus::Unavailable {
                message: message.to_string(),
            },
            "rejected" => AstDumpStatus::Rejected {
                message: message.to_string(),
            },
            "invalid-json" => AstDumpStatus::InvalidJson {
                message: message.to_string(),
            },
            _ => AstDumpStatus::NotRun {
                message: message.to_string(),
            },
        }
    }

    fn write_ast_database(root: &Path, cases: &str) {
        let entries = cases.lines().map(|case| ast_database_entry(root, case)).collect();
        write_json_database(root, &serde_json::Value::Array(entries));
    }

    fn ast_database_entry(root: &Path, case: &str) -> serde_json::Value {
        let Some((file, arguments)) = case.split_once('|') else {
            panic!("invalid AST database fixture: {case}");
        };
        compilation_entry(root, Path::new(file), crate::test_support::owned_words(arguments))
    }

    #[test]
    fn location_line_fallback_is_contained_and_size_bounded() {
        let root = temp_dir();
        let canonical_root = root
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonical root: {error}"));
        let small = canonical_root.join("small.c");
        write(&small, b"one\ntwo\nthree");
        assert_eq!(
            bounded_line_starts(&canonical_root, &small, MAX_AST_LINE_START_CACHE_BYTES)
                .map(|(starts, _cache_bytes)| starts),
            Some(vec![0, 4, 8])
        );
        assert_eq!(
            bounded_line_starts(
                &canonical_root,
                &small,
                2_usize.saturating_mul(mem::size_of::<usize>()),
            ),
            None
        );

        let sparse = canonical_root.join("sparse.c");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&sparse)
            .and_then(|file| file.set_len(MAX_AST_LOCATION_SOURCE_BYTES.saturating_add(1)))
            .unwrap_or_else(|error| panic!("create sparse source: {error}"));
        assert_eq!(
            bounded_line_starts(&canonical_root, &sparse, MAX_AST_LINE_START_CACHE_BYTES),
            None
        );

        let outside = temp_dir();
        let outside_source = write_file(outside.path(), "outside.c", b"outside\n");
        assert_eq!(
            bounded_line_starts(&canonical_root, &outside_source, MAX_AST_LINE_START_CACHE_BYTES),
            None
        );
        assert_eq!(
            bounded_line_starts(&canonical_root, &canonical_root, MAX_AST_LINE_START_CACHE_BYTES),
            None
        );
    }

    #[test]
    fn extracts_supported_language_functions_and_decisions() {
        let cases = [
            (
                "c",
                ClangLanguage::C,
                Language::C,
                "check",
                (1, 5),
                r#"{"kind":"TranslationUnitDecl","inner":[{"kind":"FunctionDecl","name":"check","loc":{"line":1,"offset":0},"range":{"begin":{"line":1,"offset":0},"end":{"line":5,"offset":24}},"inner":[{"kind":"CompoundStmt","inner":[{"kind":"IfStmt","inner":[{"kind":"BinaryOperator","opcode":"&&"}]}]}]}]}"#,
            ),
            (
                "cpp",
                ClangLanguage::Cpp,
                Language::Cpp,
                "Widget::run",
                (2, 5),
                r#"{"kind":"TranslationUnitDecl","inner":[{"kind":"CXXRecordDecl","name":"Widget","inner":[{"kind":"CXXMethodDecl","name":"run","loc":{"line":2,"offset":6},"range":{"begin":{"line":2,"offset":6},"end":{"line":5,"offset":24}},"inner":[{"kind":"CompoundStmt","inner":[{"kind":"ForStmt"},{"kind":"ConditionalOperator"}]}]}]}]}"#,
            ),
            (
                "m",
                ClangLanguage::ObjectiveC,
                Language::ObjectiveC,
                "Controller::handle:",
                (2, 5),
                r#"{"kind":"TranslationUnitDecl","inner":[{"kind":"ObjCImplementationDecl","name":"Controller","inner":[{"kind":"ObjCMethodDecl","name":"handle:","loc":{"line":2,"offset":6},"range":{"begin":{"line":2,"offset":6},"end":{"line":5,"offset":24}},"inner":[{"kind":"CompoundStmt","inner":[{"kind":"ObjCForCollectionStmt"},{"kind":"ObjCAtCatchStmt"}]}]}]}]}"#,
            ),
        ];

        for (extension, clang_language, language, name, lines, json) in cases {
            let functions = fixture(extension, clang_language, json);
            assert_eq!(functions.len(), 1);
            assert_eq!(functions[0].language, language);
            assert_eq!(functions[0].name, name);
            assert_eq!(functions[0].complexity, 3);
            assert_eq!((functions[0].start_line, functions[0].end_line), lines);
        }
    }

    fn assert_overload_symbols(functions: &[FunctionRecord]) {
        assert_eq!(functions.len(), 2);
        assert_eq!(functions[0].name, functions[1].name);
        assert_ne!(functions[0].stable_symbol, functions[1].stable_symbol);
        assert!(functions
            .iter()
            .all(|function| !function.stable_symbol.contains("line")));
    }

    fn assert_nested_boundaries(functions: &[FunctionRecord]) {
        let [function] = functions else {
            panic!("nested executables leaked: {functions:?}");
        };
        assert_eq!(
            (
                function.name.as_str(),
                function.complexity,
                function.coverage_excluded_ranges.as_slice()
            ),
            ("outer", 2, &[(3, 4)][..])
        );
    }

    #[test]
    fn cpp_symbols_and_nested_complexity_boundaries_are_stable() {
        let overloads = fixture(
            "cpp",
            ClangLanguage::Cpp,
            r#"{
                "kind":"TranslationUnitDecl",
                "inner":[{
                    "kind":"CXXRecordDecl","name":"Widget","inner":[
                        {
                            "kind":"CXXMethodDecl","name":"run","type":{"qualType":"int (int)"},
                            "loc":{"line":1,"offset":0},
                            "range":{"begin":{"line":1,"offset":0},"end":{"line":1,"offset":8}},
                            "inner":[{"kind":"CompoundStmt"}]
                        },
                        {
                            "kind":"CXXMethodDecl","name":"run","type":{"qualType":"int (double)"},
                            "loc":{"line":1,"offset":18},
                            "range":{"begin":{"line":1,"offset":18},"end":{"line":1,"offset":24}},
                            "inner":[{"kind":"CompoundStmt"}]
                        }
                    ]
                }]
            }"#,
        );
        assert_overload_symbols(&overloads);

        let nested = fixture(
            "cpp",
            ClangLanguage::Cpp,
            r#"{
                "kind":"TranslationUnitDecl",
                "inner":[{
                    "kind":"FunctionDecl","name":"outer",
                    "loc":{"line":1,"offset":0},
                    "range":{"begin":{"line":1,"offset":0},"end":{"line":5,"offset":24}},
                    "inner":[{"kind":"CompoundStmt","inner":[
                        {"kind":"IfStmt"},
                        {"kind":"LambdaExpr","range":{"begin":{"line":3,"offset":8},"end":{"line":4,"offset":16}},"inner":[{
                            "kind":"CXXRecordDecl","name":"(lambda)","inner":[{
                                "kind":"CXXMethodDecl","name":"operator()","inner":[{
                                    "kind":"CompoundStmt","inner":[{"kind":"WhileStmt"}]
                                }]
                            }]
                        }]},
                        {"kind":"FunctionDecl","name":"local","inner":[{
                            "kind":"CompoundStmt","inner":[{"kind":"ForStmt"}]
                        }]}
                    ]}]
                }]
            }"#,
        );
        assert_nested_boundaries(&nested);
    }

    #[test]
    fn ast_diagnostics_cover_every_provenance_status() {
        let source = source_file("/project/source.c", "source.c", Language::C);
        let mut statuses = vec![
            AstDumpStatus::Analyzed,
            AstDumpStatus::CommandFailed {
                exit_code: Some(7),
                stderr: String::new(),
            },
            AstDumpStatus::TimedOut {
                timeout: Duration::from_millis(25),
                stderr: "timeout detail".to_string(),
            },
            AstDumpStatus::OutputTooLarge { limit: 128 },
        ];
        statuses.extend(
            "unavailable|missing compiler\nrejected|unsafe argument\ninvalid-json|bad json\nnot-run|not selected"
                .lines()
                .map(|case| {
                    let (kind, message) = case.split_once('|').unwrap_or(("not-run", case));
                    message_status(kind, message)
                }),
        );
        let messages = statuses
            .iter()
            .map(|status| ast_diagnostic(&source, 3, status).message)
            .collect::<Vec<_>>();

        assert!(messages.iter().all(|message| message.contains("source.c")));
        assert!(messages
            .iter()
            .any(|message| message.contains("no compiler diagnostics")));
        assert!(messages.iter().any(|message| message.contains("128-byte")));
        assert!(
            std::panic::catch_unwind(|| other_ast_status_message(&source, 3, &AstDumpStatus::Analyzed))
                .is_err()
        );
    }

    #[test]
    fn installed_clang_extracts_c_cpp_and_objective_c_functions_end_to_end() {
        if !Command::new("clang")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }

        let temp = temp_dir();
        write(
            temp.path().join("sample.c"),
            "int check(int a, int b) { if (a && b) return 1; return 0; }\n",
        );
        write(
            temp.path().join("sample.cpp"),
            "class Widget { public: int run(int n); };\nint Widget::run(int n) { for (int i=0;i<n;i++) n--; return n > 0 ? 1 : 0; }\n",
        );
        write(
            temp.path().join("sample.m"),
            "@interface Greeter\n- (int)choose:(int)x;\n@end\n@implementation Greeter\n- (int)choose:(int)x { if (x) return 1; return 0; }\n@end\n",
        );
        write_ast_database(
            temp.path(),
            "sample.c|clang -c sample.c\nsample.cpp|clang++ -std=c++20 -c sample.cpp\nsample.m|clang -Wno-objc-root-class -c sample.m",
        );

        let analysis = analyze(
            &ClangAdapter::default().with_timeout(Duration::from_secs(10)),
            temp.path(),
        );
        assert!(
            analysis
                .translation_units
                .iter()
                .all(|unit| unit.status == AstDumpStatus::Analyzed),
            "AST failures: {:?}",
            analysis.translation_units
        );
        let functions = &analysis.snapshot.functions;
        let summaries = functions
            .iter()
            .map(|function| {
                format!(
                    "{}|{:?}|{}",
                    function.name, function.language, function.complexity
                )
            })
            .collect::<Vec<_>>();
        for expected in "check|C|3\nWidget::run|Cpp|3\nGreeter::choose:|ObjectiveC|2".lines() {
            assert!(summaries.iter().any(|summary| summary == expected));
        }
    }

    #[cfg(unix)]
    #[test]
    fn ast_timeout_retains_the_exact_invocation_and_diagnostic() {
        let temp = temp_dir();
        let compiler = temp.path().join("fake-clang");
        write_executable(
            &compiler,
            concat!(
                "#!/bin/sh\n",
                "if [ \"$1\" = \"--version\" ]; then echo 'fake clang 1.0'; exit 0; fi\n",
                "for arg in \"$@\"; do if [ \"$arg\" = \"-ast-dump=json\" ]; then exec sleep 2; fi; done\n",
                "exit 0\n",
            ),
        );
        write(temp.path().join("sample.c"), "int check(void) { return 1; }\n");
        write_ast_database(temp.path(), "sample.c|clang -c sample.c");

        let started = Instant::now();
        let adapter = ClangAdapter::new(&compiler).with_timeout(Duration::from_millis(250));
        let analysis = analyze(&adapter, temp.path());
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert_eq!(analysis.snapshot.parse_errors, 1);
        let unit = analysis
            .translation_units
            .first()
            .unwrap_or_else(|| panic!("missing AST provenance"));
        assert!(
            matches!(unit.status, AstDumpStatus::TimedOut { .. }),
            "unexpected AST status: {:?}",
            unit.status
        );
        assert!(unit.invocation.as_ref().is_some_and(|invocation| invocation
            .arguments
            .iter()
            .any(|argument| argument == "-ast-dump=json")));
        assert!(analysis
            .snapshot
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("JSON AST timed out")));
    }
}

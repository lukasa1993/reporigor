use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::mem;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
pub enum AstDumpStatus {
    Analyzed,
    CommandFailed { exit_code: Option<i32>, stderr: String },
    TimedOut { timeout: Duration, stderr: String },
    Unavailable { message: String },
    Rejected { message: String },
    OutputTooLarge { limit: usize },
    InvalidJson { message: String },
    NotRun { message: String },
}

/// Native AST extraction provenance for one translation unit.
#[derive(Debug, Clone)]
pub struct AstTranslationUnit {
    pub command_index: usize,
    pub command: CompileCommand,
    pub source: Option<SourceFile>,
    pub language: Option<ClangLanguage>,
    pub invocation: Option<SanitizedCommand>,
    pub status: AstDumpStatus,
    pub elapsed: Duration,
    pub stderr: String,
    pub functions: Vec<FunctionRecord>,
}

/// Standard analysis output plus the compilation database, validation records,
/// and exact JSON AST command provenance used to produce it.
#[derive(Debug, Clone)]
pub struct ClangAnalysis {
    pub snapshot: AnalysisSnapshot,
    pub project: ClangProject,
    pub translation_units: Vec<AstTranslationUnit>,
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
        let mut snapshot = AnalysisSnapshot {
            files: project.context.sources.clone(),
            backends: project.context.backends.clone(),
            diagnostics: project.context.diagnostics.clone(),
            ..AnalysisSnapshot::default()
        };
        let mut records = BTreeMap::<FunctionKey, FunctionRecord>::new();
        let mut ast_units = Vec::with_capacity(project.translation_units.len());

        for unit in &project.translation_units {
            let Some(source) = unit.source.as_ref() else {
                ast_units.push(not_run(unit, "translation unit was not selected for analysis"));
                continue;
            };
            let Some(language) = unit.language else {
                ast_units.push(not_run(unit, "translation unit language is unknown"));
                continue;
            };
            if !validation_allows_ast(&unit.status) {
                snapshot.parse_errors = snapshot.parse_errors.saturating_add(1);
                ast_units.push(not_run(
                    unit,
                    "Clang syntax validation did not succeed; JSON AST was not requested",
                ));
                continue;
            }

            let ast_unit = dump_translation_unit(
                unit.command_index,
                &unit.command,
                source,
                language,
                self.compiler(),
                self.timeout(),
                self.ast_output_limit(),
                &project.context.root,
            );
            if ast_unit.status != AstDumpStatus::Analyzed {
                snapshot.parse_errors = snapshot.parse_errors.saturating_add(1);
                snapshot
                    .diagnostics
                    .push(ast_diagnostic(source, unit.command_index, &ast_unit.status));
            }
            for function in &ast_unit.functions {
                let key = FunctionKey::from(function);
                records
                    .entry(key)
                    .and_modify(|record| record.complexity = record.complexity.max(function.complexity))
                    .or_insert_with(|| function.clone());
            }
            ast_units.push(ast_unit);
        }

        snapshot.functions = records.into_values().collect();
        Ok(ClangAnalysis {
            snapshot,
            project,
            translation_units: ast_units,
        })
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

#[allow(clippy::too_many_arguments)]
fn dump_translation_unit(
    command_index: usize,
    command: &CompileCommand,
    source: &SourceFile,
    language: ClangLanguage,
    compiler: &Path,
    timeout: Duration,
    output_limit: usize,
    root: &Path,
) -> AstTranslationUnit {
    let mut invocation = match ast_dump_command(command, compiler, language) {
        Ok(invocation) => invocation,
        Err(error) => {
            return AstTranslationUnit {
                command_index,
                command: command.clone(),
                source: Some(source.clone()),
                language: Some(language),
                invocation: None,
                status: AstDumpStatus::Rejected {
                    message: error.to_string(),
                },
                elapsed: Duration::ZERO,
                stderr: String::new(),
                functions: Vec::new(),
            };
        }
    };
    add_ast_dump_flags(&mut invocation);
    let started = Instant::now();
    let outcome = run_bounded_capture(&invocation, timeout, Some(output_limit), AST_STDERR_LIMIT);
    let elapsed = started.elapsed();

    let (status, stderr, functions) = match outcome {
        ProcessOutcome::Exited {
            success: false,
            exit_code,
            stderr,
            ..
        } => (
            AstDumpStatus::CommandFailed {
                exit_code,
                stderr: annotate_truncation(stderr.text.clone(), stderr.truncated),
            },
            annotate_truncation(stderr.text, stderr.truncated),
            Vec::new(),
        ),
        ProcessOutcome::Exited {
            success: true,
            stdout,
            stderr,
            ..
        } if stdout.truncated => (
            AstDumpStatus::OutputTooLarge { limit: output_limit },
            annotate_truncation(stderr.text, stderr.truncated),
            Vec::new(),
        ),
        ProcessOutcome::Exited {
            success: true,
            stdout,
            stderr,
            ..
        } => {
            let stderr_text = annotate_truncation(stderr.text, stderr.truncated);
            match parse_ast(&stdout.text) {
                Ok(ast) => (
                    AstDumpStatus::Analyzed,
                    stderr_text,
                    extract_functions(&ast, root, command, source, language),
                ),
                Err(error) => (
                    AstDumpStatus::InvalidJson {
                        message: error.to_string(),
                    },
                    stderr_text,
                    Vec::new(),
                ),
            }
        }
        ProcessOutcome::TimedOut { stderr } => (
            AstDumpStatus::TimedOut {
                timeout,
                stderr: annotate_truncation(stderr.text.clone(), stderr.truncated),
            },
            annotate_truncation(stderr.text, stderr.truncated),
            Vec::new(),
        ),
        ProcessOutcome::Unavailable(message) => (
            AstDumpStatus::Unavailable {
                message: message.clone(),
            },
            message,
            Vec::new(),
        ),
    };

    AstTranslationUnit {
        command_index,
        command: command.clone(),
        source: Some(source.clone()),
        language: Some(language),
        invocation: Some(invocation),
        status,
        elapsed,
        stderr,
        functions,
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
    let message = match status {
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
        AstDumpStatus::Unavailable { message } => format!(
            "Clang JSON AST unavailable for {} (database entry {command_index}): {message}",
            source.relative
        ),
        AstDumpStatus::Rejected { message } => format!(
            "refused unsafe Clang JSON AST command for {} (database entry {command_index}): {message}",
            source.relative
        ),
        AstDumpStatus::OutputTooLarge { limit } => format!(
            "Clang JSON AST for {} exceeded the {limit}-byte output limit (database entry {command_index})",
            source.relative
        ),
        AstDumpStatus::InvalidJson { message } => format!(
            "Clang returned invalid JSON AST for {} (database entry {command_index}): {message}",
            source.relative
        ),
        AstDumpStatus::NotRun { message } => format!(
            "Clang JSON AST was not run for {} (database entry {command_index}): {message}",
            source.relative
        ),
    };
    Diagnostic {
        severity: Severity::Error,
        backend: "clang".to_string(),
        message,
        location: None,
        fallback_used: false,
    }
}

fn fallback_message(value: &str) -> &str {
    if value.trim().is_empty() {
        "no compiler diagnostics"
    } else {
        value
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct FunctionKey {
    file: String,
    start_line: u32,
    end_line: u32,
    name: String,
}

impl From<&FunctionRecord> for FunctionKey {
    fn from(record: &FunctionRecord) -> Self {
        Self {
            file: record.file.clone(),
            start_line: record.start_line,
            end_line: record.end_line,
            name: record.name.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct AstNode {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    opcode: Option<String>,
    #[serde(default, rename = "parentDeclContextId")]
    parent_context_id: Option<String>,
    #[serde(default)]
    loc: Option<AstPoint>,
    #[serde(default)]
    range: Option<AstRange>,
    #[serde(default)]
    inner: Vec<Self>,
}

#[derive(Debug, Deserialize)]
struct AstRange {
    begin: AstPoint,
    end: AstPoint,
}

#[derive(Debug, Deserialize)]
struct AstPoint {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default, rename = "spellingLoc")]
    spelling: Option<Box<Self>>,
    #[serde(default, rename = "expansionLoc")]
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
    extractor.records.sort_by(|left, right| {
        (&left.file, left.start_line, left.end_line, &left.name).cmp(&(
            &right.file,
            right.start_line,
            right.end_line,
            &right.name,
        ))
    });
    extractor.records.dedup_by(|left, right| {
        left.file == right.file
            && left.start_line == right.start_line
            && left.end_line == right.end_line
            && left.name == right.name
    });
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
            if let Some(record) = self.function_record(node, scope) {
                self.records.push(record);
            }
            // Local declarations and local-class methods are outside the
            // canonical file/module/type-owned function metric domain.
            return;
        }

        let pushed = if is_scope_kind(&node.kind) {
            node.name.as_ref().is_some_and(|name| {
                if scope.last() == Some(name) {
                    false
                } else {
                    scope.push(name.clone());
                    true
                }
            })
        } else {
            false
        };
        for child in &node.inner {
            self.visit(child, scope);
        }
        if pushed {
            scope.pop();
        }
    }

    fn function_record(&mut self, node: &AstNode, scope: &[String]) -> Option<FunctionRecord> {
        let body = node.inner.iter().find(|child| is_body_kind(&child.kind))?;
        let raw_name = node.name.as_deref()?.trim();
        if raw_name.is_empty() {
            return None;
        }
        let path = self.function_path(node)?;
        let relative = path
            .strip_prefix(self.root)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/");
        let range = node.range.as_ref();
        let start_line = range
            .and_then(|value| self.point_line(&value.begin, &path))
            .or_else(|| node.loc.as_ref().and_then(|value| self.point_line(value, &path)))
            .unwrap_or(1);
        let end_line = range
            .and_then(|value| self.point_line(&value.end, &path))
            .unwrap_or(start_line)
            .max(start_line);
        let contextual_scope = if scope.is_empty() {
            node.parent_context_id
                .as_ref()
                .and_then(|id| self.context_names.get(id))
                .cloned()
        } else {
            Some(scope.join("::"))
        };
        let name = if raw_name.contains("::") {
            raw_name.to_string()
        } else if let Some(owner) = contextual_scope.as_deref().filter(|owner| !owner.is_empty()) {
            format!("{owner}::{raw_name}")
        } else {
            raw_name.to_string()
        };
        Some(FunctionRecord {
            language: self.language.core_language(),
            name,
            file: relative,
            start_line,
            end_line,
            complexity: 1_u32.saturating_add(decision_count(body)),
            coverage: None,
            crap: None,
        })
    }

    fn function_path(&self, node: &AstNode) -> Option<PathBuf> {
        let location_file = node
            .loc
            .as_ref()
            .and_then(AstPoint::file)
            .or_else(|| node.range.as_ref().and_then(|range| range.begin.file()));
        let path = match location_file {
            Some(file) if file.starts_with('<') => return None,
            Some(file) => {
                let path = Path::new(file);
                if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    self.directory.join(path)
                }
            }
            None => self.default_source.to_path_buf(),
        };
        let canonical = path.canonicalize().ok()?;
        canonical.strip_prefix(self.root).ok()?;
        Some(canonical)
    }

    fn point_line(&mut self, point: &AstPoint, path: &Path) -> Option<u32> {
        if let Some(line) = point.line() {
            return u32::try_from(line).ok();
        }
        let offset = usize::try_from(point.offset()?).ok()?;
        if !self.line_starts.contains_key(path) {
            let remaining = MAX_AST_LINE_START_CACHE_BYTES.saturating_sub(self.line_start_cache_bytes);
            let (starts, cache_bytes) = bounded_line_starts(self.root, path, remaining)?;
            self.line_start_cache_bytes = self.line_start_cache_bytes.checked_add(cache_bytes)?;
            self.line_starts.insert(path.to_path_buf(), starts);
        }
        let starts = self.line_starts.get(path)?;
        u32::try_from(starts.partition_point(|start| *start <= offset)).ok()
    }
}

fn bounded_line_starts(root: &Path, path: &Path, max_cache_bytes: usize) -> Option<(Vec<usize>, usize)> {
    let canonical = path.canonicalize().ok()?;
    canonical.strip_prefix(root).ok()?;
    let mut file = File::open(&canonical).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_AST_LOCATION_SOURCE_BYTES {
        return None;
    }

    let max_starts = max_cache_bytes.checked_div(mem::size_of::<usize>())?;
    if max_starts == 0 {
        return None;
    }
    let mut starts = vec![0];
    let mut bytes_read = 0_u64;
    let mut buffer = [0_u8; 8 * 1024];
    let mut limited = file
        .by_ref()
        .take(MAX_AST_LOCATION_SOURCE_BYTES.saturating_add(1));
    loop {
        let count = limited.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        bytes_read = bytes_read.checked_add(u64::try_from(count).ok()?)?;
        if bytes_read > MAX_AST_LOCATION_SOURCE_BYTES {
            return None;
        }
        let chunk_start = bytes_read.checked_sub(u64::try_from(count).ok()?)?;
        for (index, byte) in buffer[..count].iter().enumerate() {
            if *byte == b'\n' {
                if starts.len() >= max_starts {
                    return None;
                }
                let start = chunk_start
                    .checked_add(u64::try_from(index).ok()?)?
                    .checked_add(1)?;
                starts.push(usize::try_from(start).ok()?);
            }
        }
    }
    let cache_bytes = starts.len().checked_mul(mem::size_of::<usize>())?;
    Some((starts, cache_bytes))
}

fn collect_context_names(node: &AstNode, scope: &mut Vec<String>, contexts: &mut BTreeMap<String, String>) {
    let pushed = if is_scope_kind(&node.kind) {
        node.name.as_ref().is_some_and(|name| {
            if scope.last() == Some(name) {
                false
            } else {
                scope.push(name.clone());
                true
            }
        })
    } else {
        false
    };
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

fn is_function_kind(kind: &str) -> bool {
    matches!(kind, "FunctionDecl" | "CXXMethodDecl" | "ObjCMethodDecl")
}

fn is_anonymous_function_kind(kind: &str) -> bool {
    matches!(kind, "LambdaExpr" | "BlockExpr" | "BlockDecl")
}

fn is_body_kind(kind: &str) -> bool {
    matches!(
        kind,
        "CompoundStmt" | "CXXTryStmt" | "CoroutineBodyStmt" | "SEHTryStmt"
    )
}

fn is_scope_kind(kind: &str) -> bool {
    matches!(
        kind,
        "NamespaceDecl"
            | "CXXRecordDecl"
            | "ClassTemplateSpecializationDecl"
            | "ObjCInterfaceDecl"
            | "ObjCImplementationDecl"
            | "ObjCCategoryDecl"
            | "ObjCCategoryImplDecl"
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
    matches!(
        node.kind.as_str(),
        "IfStmt"
            | "ForStmt"
            | "WhileStmt"
            | "DoStmt"
            | "CXXForRangeStmt"
            | "ObjCForCollectionStmt"
            | "CaseStmt"
            | "ConditionalOperator"
            | "BinaryConditionalOperator"
            | "CXXCatchStmt"
            | "ObjCAtCatchStmt"
            | "SEHExceptStmt"
    ) || (node.kind == "BinaryOperator" && matches!(node.opcode.as_deref(), Some("&&" | "||")))
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::process::Command;

    use reporigor_core::Language;
    use tempfile::tempdir;

    use super::*;
    use crate::CommandOrigin;

    fn fixture(extension: &str, language: ClangLanguage, json: &str) -> Vec<FunctionRecord> {
        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        let source = temp.path().join(format!("sample.{extension}"));
        fs::write(&source, "line1\nline2\nline3\nline4\nline5\n")
            .unwrap_or_else(|error| panic!("write source: {error}"));
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
        let source_file = SourceFile {
            path: source,
            relative: format!("sample.{extension}"),
            language: language.core_language(),
            generated: false,
            test: false,
        };
        let root = temp
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonical root: {error}"));
        extract_functions(&ast, &root, &command, &source_file, language)
    }

    #[test]
    fn location_line_fallback_is_contained_and_size_bounded() {
        let root = tempdir().unwrap_or_else(|error| panic!("root tempdir: {error}"));
        let canonical_root = root
            .path()
            .canonicalize()
            .unwrap_or_else(|error| panic!("canonical root: {error}"));
        let small = canonical_root.join("small.c");
        fs::write(&small, b"one\ntwo\nthree").unwrap_or_else(|error| panic!("write small source: {error}"));
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

        let outside = tempdir().unwrap_or_else(|error| panic!("outside tempdir: {error}"));
        let outside_source = outside.path().join("outside.c");
        fs::write(&outside_source, b"outside\n")
            .unwrap_or_else(|error| panic!("write outside source: {error}"));
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
    fn extracts_c_function_and_logical_decisions() {
        let functions = fixture(
            "c",
            ClangLanguage::C,
            r#"{
                "kind":"TranslationUnitDecl",
                "inner":[{
                    "kind":"FunctionDecl","name":"check",
                    "loc":{"line":1,"offset":0},
                    "range":{"begin":{"line":1,"offset":0},"end":{"line":5,"offset":24}},
                    "inner":[{"kind":"CompoundStmt","inner":[
                        {"kind":"IfStmt","inner":[
                            {"kind":"BinaryOperator","opcode":"&&"}
                        ]}
                    ]}]
                }]
            }"#,
        );
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].language, Language::C);
        assert_eq!(functions[0].name, "check");
        assert_eq!(functions[0].complexity, 3);
        assert_eq!((functions[0].start_line, functions[0].end_line), (1, 5));
    }

    #[test]
    fn extracts_qualified_cpp_method_and_loop_decisions() {
        let functions = fixture(
            "cpp",
            ClangLanguage::Cpp,
            r#"{
                "kind":"TranslationUnitDecl",
                "inner":[{
                    "kind":"CXXRecordDecl","name":"Widget","inner":[{
                        "kind":"CXXMethodDecl","name":"run",
                        "loc":{"line":2,"offset":6},
                        "range":{"begin":{"line":2,"offset":6},"end":{"line":5,"offset":24}},
                        "inner":[{"kind":"CompoundStmt","inner":[
                            {"kind":"ForStmt"},
                            {"kind":"ConditionalOperator"}
                        ]}]
                    }]
                }]
            }"#,
        );
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].language, Language::Cpp);
        assert_eq!(functions[0].name, "Widget::run");
        assert_eq!(functions[0].complexity, 3);
    }

    #[test]
    fn cpp_lambdas_and_local_declarations_are_unreported_complexity_boundaries() {
        let functions = fixture(
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
                        {"kind":"LambdaExpr","inner":[{
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
        assert_eq!(functions.len(), 1, "nested executables leaked: {functions:?}");
        assert_eq!(functions[0].name, "outer");
        assert_eq!(functions[0].complexity, 2);
    }

    #[test]
    fn extracts_objective_c_method_and_collection_loop() {
        let functions = fixture(
            "m",
            ClangLanguage::ObjectiveC,
            r#"{
                "kind":"TranslationUnitDecl",
                "inner":[{
                    "kind":"ObjCImplementationDecl","name":"Controller","inner":[{
                        "kind":"ObjCMethodDecl","name":"handle:",
                        "loc":{"line":2,"offset":6},
                        "range":{"begin":{"line":2,"offset":6},"end":{"line":5,"offset":24}},
                        "inner":[{"kind":"CompoundStmt","inner":[
                            {"kind":"ObjCForCollectionStmt"},
                            {"kind":"ObjCAtCatchStmt"}
                        ]}]
                    }]
                }]
            }"#,
        );
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].language, Language::ObjectiveC);
        assert_eq!(functions[0].name, "Controller::handle:");
        assert_eq!(functions[0].complexity, 3);
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

        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        fs::write(
            temp.path().join("sample.c"),
            "int check(int a, int b) { if (a && b) return 1; return 0; }\n",
        )
        .unwrap_or_else(|error| panic!("write C source: {error}"));
        fs::write(
            temp.path().join("sample.cpp"),
            "class Widget { public: int run(int n); };\nint Widget::run(int n) { for (int i=0;i<n;i++) n--; return n > 0 ? 1 : 0; }\n",
        )
        .unwrap_or_else(|error| panic!("write C++ source: {error}"));
        fs::write(
            temp.path().join("sample.m"),
            "@interface Greeter\n- (int)choose:(int)x;\n@end\n@implementation Greeter\n- (int)choose:(int)x { if (x) return 1; return 0; }\n@end\n",
        )
        .unwrap_or_else(|error| panic!("write Objective-C source: {error}"));
        let directory = serde_json::to_value(temp.path())
            .unwrap_or_else(|error| panic!("serialize project path: {error}"));
        let database = serde_json::json!([
            {
                "directory": directory,
                "file": "sample.c",
                "arguments": ["clang", "-c", "sample.c"]
            },
            {
                "directory": directory,
                "file": "sample.cpp",
                "arguments": ["clang++", "-std=c++20", "-c", "sample.cpp"]
            },
            {
                "directory": directory,
                "file": "sample.m",
                "arguments": ["clang", "-Wno-objc-root-class", "-c", "sample.m"]
            }
        ]);
        fs::write(
            temp.path().join("compile_commands.json"),
            serde_json::to_vec(&database)
                .unwrap_or_else(|error| panic!("serialize compilation database: {error}")),
        )
        .unwrap_or_else(|error| panic!("write compilation database: {error}"));

        let analysis = ClangAdapter::default()
            .with_timeout(Duration::from_secs(10))
            .analyze_project_with_provenance(&AnalysisRequest::new(temp.path().to_path_buf()))
            .unwrap_or_else(|error| panic!("analyze project: {error}"));
        assert!(
            analysis
                .translation_units
                .iter()
                .all(|unit| unit.status == AstDumpStatus::Analyzed),
            "AST failures: {:?}",
            analysis.translation_units
        );
        let functions = &analysis.snapshot.functions;
        assert!(functions
            .iter()
            .any(|function| function.name == "check" && function.complexity == 3));
        assert!(functions.iter().any(|function| {
            function.name == "Widget::run" && function.language == Language::Cpp && function.complexity == 3
        }));
        assert!(functions.iter().any(|function| {
            function.name == "Greeter::choose:"
                && function.language == Language::ObjectiveC
                && function.complexity == 2
        }));
    }

    #[cfg(unix)]
    #[test]
    fn ast_timeout_retains_the_exact_invocation_and_diagnostic() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap_or_else(|error| panic!("tempdir: {error}"));
        fs::write(temp.path().join("sample.c"), "int check(void) { return 1; }\n")
            .unwrap_or_else(|error| panic!("write source: {error}"));
        let compiler = temp.path().join("fake-clang");
        fs::write(
            &compiler,
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'fake clang 1.0'; exit 0; fi\nfor arg in \"$@\"; do if [ \"$arg\" = \"-ast-dump=json\" ]; then exec sleep 2; fi; done\nexit 0\n",
        )
        .unwrap_or_else(|error| panic!("write compiler: {error}"));
        let mut permissions = fs::metadata(&compiler)
            .unwrap_or_else(|error| panic!("compiler metadata: {error}"))
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&compiler, permissions)
            .unwrap_or_else(|error| panic!("compiler permissions: {error}"));
        let database = serde_json::json!([{
            "directory": temp.path(),
            "file": "sample.c",
            "arguments": ["clang", "-c", "sample.c"]
        }]);
        fs::write(
            temp.path().join("compile_commands.json"),
            serde_json::to_vec(&database)
                .unwrap_or_else(|error| panic!("serialize compilation database: {error}")),
        )
        .unwrap_or_else(|error| panic!("write compilation database: {error}"));

        let started = Instant::now();
        let analysis = ClangAdapter::new(&compiler)
            .with_timeout(Duration::from_millis(250))
            .analyze_project_with_provenance(&AnalysisRequest::new(temp.path().to_path_buf()))
            .unwrap_or_else(|error| panic!("analyze project: {error}"));
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

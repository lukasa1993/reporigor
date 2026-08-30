//! Deterministic generic syntax analysis backed by pinned Tree-sitter grammars.
//!
//! This adapter intentionally owns the grammar dependencies directly. It never
//! downloads parsers at runtime, so the same binary works in offline CI and
//! produces deterministic syntax-level results.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use indexmap::IndexSet;
use reporigor_core::{
    stable_id, AnalysisRequest, BackendInfo, Capability, CoreError, CoverageSpan, Diagnostic, FileAnalysis,
    FunctionRecord, Language, MutationCandidate, Severity, SourceBudget, SourceFile, SourceLocation,
    SymbolVisibility, SyntaxBackend, TokenRecord,
};
use tree_sitter::{Language as TreeSitterLanguage, Node, Parser};

const BACKEND_ID: &str = "tree-sitter-generic";

const MAX_TREE_SITTER_SOURCE_BYTES: usize = u32::MAX as usize;

const COMMENT_TYPES: &str = "comment|line_comment|block_comment|multiline_comment|html_comment";
const STRING_TYPES: &str = "string_literal|raw_string_literal|char_literal|string|raw_string|ansi_c_string|translated_string|interpreted_string_literal|line_string_literal|multiline_string_literal|template_string|regex|heredoc_body";
const NUMBER_TYPES: &str = "number_literal|integer_literal|float_literal|integer|float|real_literal|number";
const IDENTIFIER_TYPES: &str = "identifier|field_identifier|type_identifier|simple_identifier|word|property_identifier|function_identifier|namespace_identifier|variable_name";
const NAME_TYPES: &str = "identifier|field_identifier|type_identifier|simple_identifier|word|operator_name|destructor_name|qualified_identifier|scoped_identifier|function_identifier|method_selector|keyword_selector";
const SKIPPED_MUTATION_ANCESTORS: &str = "comment|line_comment|block_comment|documentation_comment|string_literal|raw_string_literal|char_literal|heredoc_body|heredoc_redirect|word|string|raw_string|ansi_c_string|translated_string|interpreted_string_literal|template_string|template_literal|template_substitution|regex|regex_pattern|line_string_literal|multi_line_string_literal|multiline_string_literal|interpolated_string_expression|string_interpolation|jsx_text";
const NON_EXPRESSION_MUTATION_ANCESTORS: &str = "literal_type";
const BINARY_CONTEXTS: &str = "binary_expression|logical_expression|boolean_expression|test_expression|arithmetic_expression|compound_expression|comparison_expression|comparison_operator|binary_operator|boolean_operator|additive_expression|multiplicative_expression|equality_expression|conjunction_expression|disjunction_expression|infix_expression";
const OPERATOR_WRAPPERS: &str = "custom_operator|test_operator";
const BOOLEAN_LITERAL_TYPES: &str = "true|false|boolean|boolean_literal";
const PARSE_ROOT_TYPES: &str = "module|program|source_file|translation_unit";
const BINDING_IDENTIFIER_TYPES: &str =
    "identifier|simple_identifier|variable_name|shorthand_property_identifier_pattern";
const PYTHON_COMPREHENSION_TYPES: &str =
    "list_comprehension|set_comprehension|dictionary_comprehension|generator_expression";
const C_FAMILY_OWNER_TYPES: &str = "class_specifier|struct_specifier|union_specifier|namespace_definition";
const SWIFT_OWNER_TYPES: &str =
    "class_declaration|struct_declaration|enum_declaration|actor_declaration|extension_declaration";

type NameResolver = for<'tree> fn(Node<'tree>, &[u8]) -> Option<String>;
type NodeResolver = for<'tree> fn(Node<'tree>) -> Option<Node<'tree>>;
type FileAnalysisResult = Result<FileAnalysis, CoreError>;

const SPECIAL_NAME_RESOLVERS: [NameResolver; Language::ALL.len()] = [
    no_special_function_name,
    no_special_function_name,
    no_special_function_name,
    objective_c_function_name,
    no_special_function_name,
    no_special_function_name,
    swift_function_name,
    typescript_function_name,
];
const DECLARATION_OWNER_RESOLVERS: [NodeResolver; Language::ALL.len()] = [
    no_declaration_owner,
    c_family_declaration_owner,
    c_family_declaration_owner,
    objective_c_declaration_owner,
    python_declaration_owner,
    rust_declaration_owner,
    swift_declaration_owner,
    typescript_declaration_owner,
];

/// Syntax-only fallback backend shared by every supported language.
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeSitterBackend;

impl TreeSitterBackend {
    pub const CAPABILITIES: [Capability; 6] = [
        Capability::Syntax,
        Capability::Functions,
        Capability::Complexity,
        Capability::Tokens,
        Capability::Mutations,
        Capability::ParseValidation,
    ];

    /// Creates a stateless generic syntax backend.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl SyntaxBackend for TreeSitterBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo::new(BACKEND_ID, env!("CARGO_PKG_VERSION"), false, Self::CAPABILITIES)
    }

    fn supports(&self, language: Language) -> bool {
        Language::ALL.contains(&language)
    }

    fn analyze_file(
        &self,
        root: &Path,
        source: &SourceFile,
        request: &AnalysisRequest,
    ) -> FileAnalysisResult {
        ensure_supported(*self, source.language)?;
        let path = source_path(root, source);
        let bytes = read_source(&path, request.max_source_bytes)?;
        let tree = parse_source(source, &path, &bytes)?;
        let root_node = tree.root_node();
        let issues = parse_issues(root_node, &bytes, source);
        reject_disallowed_parse_issues(&path, &issues, request.allow_parse_errors)?;
        let diagnostics = issues.iter().map(ParseIssue::diagnostic).collect();
        let functions = analyzed_functions(root_node, &bytes, source, !issues.is_empty());
        let tokens = normalized_tokens(root_node, &bytes, source.language);
        let mutations = enumerate_mutations(root_node, &bytes, source);

        Ok(FileAnalysis {
            source: source.clone(),
            backend: self.info(),
            functions,
            tokens,
            mutations,
            diagnostics,
            parse_errors: issues.len(),
        })
    }
}

fn ensure_supported(backend: TreeSitterBackend, language: Language) -> Result<(), CoreError> {
    if backend.supports(language) {
        return Ok(());
    }
    Err(CoreError::BackendUnavailable {
        backend: BACKEND_ID.to_string(),
        message: format!("{language} is not supported"),
    })
}

fn parse_source(source: &SourceFile, path: &Path, bytes: &[u8]) -> Result<tree_sitter::Tree, CoreError> {
    let mut parser = Parser::new();
    parser
        .set_language(&grammar_for(source))
        .map_err(|error| backend_error(format!("failed to load {} grammar: {error}", source.language)))?;
    parser.parse(bytes, None).ok_or_else(|| CoreError::Parse {
        path: path.display().to_string(),
        message: "Tree-sitter cancelled parsing before producing a tree".to_string(),
    })
}

fn reject_disallowed_parse_issues(
    path: &Path,
    issues: &[ParseIssue],
    allow_parse_errors: bool,
) -> Result<(), CoreError> {
    if issues.is_empty() || allow_parse_errors {
        return Ok(());
    }
    Err(CoreError::Parse {
        path: path.display().to_string(),
        message: summarize_parse_issues(issues),
    })
}

fn analyzed_functions(
    root: Node<'_>,
    bytes: &[u8],
    source: &SourceFile,
    parse_recovered: bool,
) -> Vec<FunctionRecord> {
    let mut functions = extract_functions(root, bytes, source);
    if parse_recovered {
        for function in &mut functions {
            function.structural_metrics_reliable = false;
        }
    }
    functions
}

fn source_path(root: &Path, source: &SourceFile) -> PathBuf {
    if source.path.is_absolute() {
        source.path.clone()
    } else {
        root.join(&source.path)
    }
}

fn read_source(path: &Path, max_source_bytes: usize) -> Result<Vec<u8>, CoreError> {
    let metadata = validated_source_metadata(path)?;
    let (effective_limit, limit) = validated_source_limit(path, metadata.len(), max_source_bytes)?;
    let bytes = read_bounded_source(path, metadata.len(), limit)?;
    let bytes = validate_source_length(path, bytes, effective_limit, max_source_bytes)?;
    validate_source_encoding(path, bytes)
}

fn validated_source_metadata(path: &Path) -> Result<std::fs::Metadata, CoreError> {
    let metadata = std::fs::metadata(path).map_err(|source| source_read_error(path, source))?;
    if metadata.is_file() {
        return Ok(metadata);
    }
    Err(backend_error(format!(
        "source {} is not a regular file",
        path.display()
    )))
}

fn validated_source_limit(
    path: &Path,
    source_bytes: u64,
    max_source_bytes: usize,
) -> Result<(usize, u64), CoreError> {
    let mut budget = SourceBudget::new(max_source_bytes)?;
    budget.observe(path, source_bytes)?;
    let effective_limit = max_source_bytes.min(MAX_TREE_SITTER_SOURCE_BYTES);
    let limit = u64::try_from(effective_limit).unwrap_or(u64::MAX);
    if source_bytes > limit {
        return Err(backend_error(format!(
            "source {} exceeds Tree-sitter's {effective_limit}-byte parser limit",
            path.display()
        )));
    }
    Ok((effective_limit, limit))
}

fn read_bounded_source(path: &Path, source_bytes: u64, limit: u64) -> Result<Vec<u8>, CoreError> {
    let capacity_limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let initial_capacity = usize::try_from(source_bytes)
        .unwrap_or(capacity_limit)
        .min(capacity_limit);
    let file = File::open(path).map_err(|source| source_read_error(path, source))?;
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| source_read_error(path, source))?;
    Ok(bytes)
}

fn source_read_error(path: &Path, source: std::io::Error) -> CoreError {
    CoreError::Read {
        path: path.display().to_string(),
        source,
    }
}

fn validate_source_length(
    path: &Path,
    bytes: Vec<u8>,
    effective_limit: usize,
    max_source_bytes: usize,
) -> Result<Vec<u8>, CoreError> {
    if bytes.len() <= effective_limit {
        return Ok(bytes);
    }
    Err(source_length_error(
        path,
        bytes.len(),
        effective_limit,
        max_source_bytes,
    ))
}

fn source_length_error(
    path: &Path,
    actual_bytes: usize,
    effective_limit: usize,
    max_source_bytes: usize,
) -> CoreError {
    if effective_limit == max_source_bytes {
        return CoreError::source_too_large(
            path,
            u64::try_from(actual_bytes).unwrap_or(u64::MAX),
            max_source_bytes,
        );
    }
    backend_error(format!(
        "source {} exceeds Tree-sitter's {effective_limit}-byte parser limit",
        path.display()
    ))
}

fn backend_error(message: String) -> CoreError {
    CoreError::Backend {
        backend: BACKEND_ID.to_string(),
        message,
    }
}

fn validate_source_encoding(path: &Path, bytes: Vec<u8>) -> Result<Vec<u8>, CoreError> {
    if let Err(error) = std::str::from_utf8(&bytes) {
        return Err(CoreError::InvalidSourceEncoding {
            path: path.display().to_string(),
            valid_up_to: error.valid_up_to(),
        });
    }
    Ok(bytes)
}

fn grammar_for(source: &SourceFile) -> TreeSitterLanguage {
    const GRAMMARS: [fn() -> TreeSitterLanguage; 8] = [
        || tree_sitter_bash::LANGUAGE.into(),
        || tree_sitter_c::LANGUAGE.into(),
        || tree_sitter_cpp::LANGUAGE.into(),
        || tree_sitter_objc::LANGUAGE.into(),
        || tree_sitter_python::LANGUAGE.into(),
        || tree_sitter_rust::LANGUAGE.into(),
        || tree_sitter_swift::LANGUAGE.into(),
        || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    ];
    if source.language == Language::TypeScript && is_tsx(source) {
        return tree_sitter_typescript::LANGUAGE_TSX.into();
    }
    GRAMMARS[language_index(source.language)]()
}

fn language_index(language: Language) -> usize {
    Language::ALL
        .iter()
        .position(|candidate| *candidate == language)
        .unwrap_or_default()
}

fn is_tsx(source: &SourceFile) -> bool {
    path_has_extension(&source.path, "tsx") || path_has_extension(Path::new(&source.relative), "tsx")
}

fn path_has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

#[derive(Debug)]
struct ParseIssue {
    missing: bool,
    kind: String,
    location: SourceLocation,
}

impl ParseIssue {
    fn diagnostic(&self) -> Diagnostic {
        let message = if self.missing {
            format!("missing syntax node `{}`", self.kind)
        } else {
            format!("syntax error node `{}`", self.kind)
        };
        Diagnostic {
            severity: Severity::Error,
            backend: BACKEND_ID.to_string(),
            message,
            location: Some(self.location.clone()),
            fallback_used: false,
        }
    }
}

fn parse_issues(root: Node<'_>, bytes: &[u8], source: &SourceFile) -> Vec<ParseIssue> {
    if !root.has_error() {
        return Vec::new();
    }

    let candidates = walk_nodes(root)
        .into_iter()
        .filter(|node| node.is_error() || node.is_missing())
        .map(|node| ParseIssue {
            missing: node.is_missing(),
            kind: node.kind().to_string(),
            location: node_location(node, bytes, &source.relative),
        })
        .collect::<Vec<_>>();
    let mut issues = Vec::new();
    let mut seen = IndexSet::new();
    for candidate in candidates {
        let key = (
            candidate.missing,
            candidate.kind.clone(),
            candidate.location.file.clone(),
            candidate.location.start_line,
            candidate.location.start_column,
            candidate.location.end_line,
            candidate.location.end_column,
        );
        if seen.insert(key) {
            issues.push(candidate);
        }
    }

    if issues.is_empty() {
        issues.push(ParseIssue {
            missing: false,
            kind: "unknown".to_string(),
            location: node_location(root, bytes, &source.relative),
        });
    }
    issues
}

fn summarize_parse_issues(issues: &[ParseIssue]) -> String {
    let mut details = issues
        .iter()
        .take(5)
        .map(|issue| {
            let description = if issue.missing {
                format!("missing `{}`", issue.kind)
            } else {
                issue.kind.clone()
            };
            format!(
                "line {}, column {}: {description}",
                issue.location.start_line, issue.location.start_column
            )
        })
        .collect::<Vec<_>>();
    if issues.len() > details.len() {
        details.push(format!("{} additional error(s)", issues.len() - details.len()));
    }
    format!("source contains syntax-tree errors ({})", details.join(", "))
}

fn node_location(node: Node<'_>, source: &[u8], file: &str) -> SourceLocation {
    let start = node.start_position();
    let end = node.end_position();
    SourceLocation {
        file: file.to_string(),
        start_line: one_based(start.row),
        start_column: unicode_scalar_column(source, node.start_byte(), start.column),
        end_line: one_based(end.row),
        end_column: unicode_scalar_column(source, node.end_byte(), end.column),
    }
}

fn unicode_scalar_column(source: &[u8], absolute_byte: usize, byte_column: usize) -> u32 {
    let end = absolute_byte.min(source.len());
    let start = end.saturating_sub(byte_column);
    one_based(validated_utf8(&source[start..end]).chars().count())
}

fn one_based(value: usize) -> u32 {
    u32::try_from(value.saturating_add(1)).unwrap_or(u32::MAX)
}

fn extract_functions(root: Node<'_>, source: &[u8], file: &SourceFile) -> Vec<FunctionRecord> {
    let function_types = reported_function_types(file.language);
    let boundaries = function_boundary_types(file.language);
    let mut functions = walk_nodes(root)
        .into_iter()
        .filter(|node| is_kind(node.kind(), function_types))
        .filter(|node| !node.has_error())
        // Function metrics describe declarations owned by the file/module/type,
        // not executable declarations created inside another function. This is
        // also the common denominator of the native Rust and Clang adapters.
        .filter(|node| !has_ancestor_kind(*node, boundaries))
        .map(|node| function_record(node, source, file))
        .collect::<Vec<_>>();

    if file.language == Language::Bash {
        let top_level = named_children(root)
            .into_iter()
            .filter(|node| !is_kind(node.kind(), function_types))
            .filter(|node| !is_kind(node.kind(), COMMENT_TYPES))
            .filter(|node| !node.has_error())
            .collect::<Vec<_>>();
        if let (Some(start), Some(end)) = (top_level.first(), top_level.last()) {
            let normalized = normalized_function_data(root, source, file.language);
            let script_status = function_status(file, true);
            functions.push(FunctionRecord {
                language: file.language,
                name: "<script>".to_string(),
                file: file.relative.clone(),
                start_line: one_based(start.start_position().row),
                end_line: one_based(end.end_position().row),
                complexity: complexity(root, source, file.language),
                stable_symbol: "<script>()".to_string(),
                nesting_depth: nesting_depth(root, file.language),
                statement_count: statement_count(root, file.language),
                parameter_count: 0,
                references: normalized.references,
                coverage_span: node_coverage_span(root),
                coverage_excluded_ranges: nested_coverage_ranges(root, file.language),
                coverage_excluded_spans: nested_coverage_spans(root, file.language),
                normalized_tokens: normalized.tokens,
                visibility: SymbolVisibility::Unknown,
                structural_metrics_reliable: script_status.reliable,
                production: script_status.production,
                entry_point: script_status.entry_point,
                package: None,
                coverage: None,
                crap: None,
            });
        }
    }

    disambiguate_duplicate_symbols(&mut functions);
    functions.sort_by(|left, right| (left.start_line, &left.name).cmp(&(right.start_line, &right.name)));
    functions
}

fn function_record(node: Node<'_>, source: &[u8], file: &SourceFile) -> FunctionRecord {
    let name = function_name(node, source, file.language);
    let normalized = normalized_function_data(node, source, file.language);
    let parameter_count = parameter_nodes(node, source, file.language).len();
    let stable_symbol = stable_function_symbol(
        node,
        source,
        file.language,
        &name,
        &normalized.locals,
        &normalized.tokens,
    );
    let callable_status = function_status(file, is_entry_point(&name));
    FunctionRecord {
        language: file.language,
        name: name.clone(),
        file: file.relative.clone(),
        start_line: one_based(node.start_position().row),
        end_line: one_based(node.end_position().row),
        complexity: complexity(node, source, file.language),
        stable_symbol,
        nesting_depth: nesting_depth(node, file.language),
        statement_count: statement_count(node, file.language),
        parameter_count: u32::try_from(parameter_count).unwrap_or(u32::MAX),
        references: normalized.references,
        coverage_span: node_coverage_span(node),
        coverage_excluded_ranges: nested_coverage_ranges(node, file.language),
        coverage_excluded_spans: nested_coverage_spans(node, file.language),
        normalized_tokens: normalized.tokens,
        visibility: function_visibility(node, source, file.language),
        structural_metrics_reliable: callable_status.reliable,
        production: callable_status.production,
        entry_point: callable_status.entry_point,
        package: None,
        coverage: None,
        crap: None,
    }
}

struct NormalizedFunctionData {
    locals: BTreeSet<String>,
    tokens: Vec<String>,
    references: BTreeSet<String>,
}

fn normalized_function_data(node: Node<'_>, source: &[u8], language: Language) -> NormalizedFunctionData {
    let locals = local_bindings(node, source, language);
    let normalization = NormalizationContext::new(source, language, &locals);
    NormalizedFunctionData {
        tokens: normalization.function_tokens(node),
        references: normalization.references(node),
        locals,
    }
}

struct FunctionStatus {
    reliable: bool,
    production: bool,
    entry_point: bool,
}

fn function_status(file: &SourceFile, entry_point: bool) -> FunctionStatus {
    let production = !file.test && !file.generated;
    FunctionStatus {
        reliable: !file.generated,
        production,
        entry_point: production && entry_point,
    }
}

fn nested_coverage_ranges(root: Node<'_>, language: Language) -> Vec<(u32, u32)> {
    let mut ranges = nested_function_nodes(root, language)
        .into_iter()
        .map(|child| {
            (
                one_based(child.start_position().row),
                one_based(child.end_position().row),
            )
        })
        .collect::<Vec<_>>();
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

fn nested_function_nodes(root: Node<'_>, language: Language) -> Vec<Node<'_>> {
    let mut nested = Vec::new();
    visit_descendants(root, |child| {
        if is_kind(child.kind(), function_boundary_types(language)) {
            nested.push(child);
            false
        } else {
            true
        }
    });
    nested
}

fn nested_coverage_spans(root: Node<'_>, language: Language) -> Vec<CoverageSpan> {
    let mut spans = nested_function_nodes(root, language)
        .into_iter()
        .map(node_coverage_span)
        .collect::<Vec<_>>();
    spans.sort_unstable();
    spans.dedup();
    spans
}

fn node_coverage_span(node: Node<'_>) -> CoverageSpan {
    let start = node.start_position();
    let end = node.end_position();
    CoverageSpan {
        start_line: one_based(start.row),
        start_column: one_based(start.column),
        end_line: one_based(end.row),
        end_column: one_based(end.column),
    }
}

fn function_name(node: Node<'_>, source: &[u8], language: Language) -> String {
    if node.kind() == "lambda_expression" {
        return "<lambda>".to_string();
    }
    if node.kind() == "closure_expression" {
        return "<closure>".to_string();
    }
    if let Some(name) = special_function_name(node, source, language) {
        return name;
    }

    let name = first_name_node(node)
        .map(|name| node_text(name, source).trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "<anonymous>".to_string());
    if matches!(language, Language::C | Language::Cpp) {
        return name;
    }
    qualify_function_name(name, qualified_owner(node, source, language), language)
}

fn special_function_name(node: Node<'_>, source: &[u8], language: Language) -> Option<String> {
    SPECIAL_NAME_RESOLVERS[language_index(language)](node, source)
}

fn no_special_function_name(_node: Node<'_>, _source: &[u8]) -> Option<String> {
    None
}

fn typescript_function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "arrow_function" | "function_expression") {
        return None;
    }
    node.parent()
        .and_then(|parent| parent.child_by_field_name("name"))
        .map(|name| node_text(name, source).trim().to_string())
}

fn objective_c_function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "method_definition" {
        return None;
    }
    objective_c_selector(node, source)
}

fn swift_function_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let base = match node.kind() {
        "init_declaration" => "init",
        "deinit_declaration" => "deinit",
        "subscript_declaration" => "subscript",
        _ => return None,
    };
    Some(
        qualified_owner(node, source, Language::Swift)
            .map_or_else(|| base.to_string(), |owner| format!("{owner}.{base}")),
    )
}

fn qualify_function_name(name: String, owner: Option<String>, language: Language) -> String {
    let Some(owner) = owner else {
        return name;
    };
    if name.starts_with(&owner) {
        name
    } else {
        let separator = if language == Language::Rust { "::" } else { "." };
        format!("{owner}{separator}{name}")
    }
}

fn first_name_node(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(name) = node.child_by_field_name("name") {
        return Some(name);
    }
    if let Some(declarator) = node.child_by_field_name("declarator") {
        if let Some(name) = declarator_name(declarator) {
            return Some(name);
        }
    }
    if let Some(selector) = node.child_by_field_name("selector") {
        return Some(selector);
    }
    walk_nodes(node)
        .into_iter()
        .find(|candidate| is_kind(candidate.kind(), NAME_TYPES))
}

fn declarator_name(declarator: Node<'_>) -> Option<Node<'_>> {
    // An outer C-family declarator also contains parameter identifiers. Prefer
    // the function declarator's own declarator before the general fallback.
    walk_nodes(declarator)
        .into_iter()
        .filter(|candidate| candidate.kind() == "function_declarator")
        .filter_map(|candidate| candidate.child_by_field_name("declarator"))
        .find_map(|inner| {
            walk_nodes(inner)
                .into_iter()
                .rfind(|candidate| is_kind(candidate.kind(), NAME_TYPES))
        })
        .or_else(|| {
            walk_nodes(declarator)
                .into_iter()
                .find(|candidate| is_kind(candidate.kind(), NAME_TYPES))
        })
}

fn objective_c_selector(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = node_text(node, source);
    let signature = text.split('{').next().unwrap_or(text.as_ref());
    let bytes = signature.as_bytes();
    let parts = objective_c_selector_parts(bytes);
    if !parts.is_empty() {
        return Some(format!("{}:", parts.join(":")));
    }

    objective_c_unary_selector(bytes)
}

fn objective_c_selector_parts(bytes: &[u8]) -> Vec<String> {
    bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b':')
        .filter_map(|(offset, _)| objective_c_selector_part(bytes, offset))
        .collect()
}

fn objective_c_selector_part(bytes: &[u8], colon: usize) -> Option<String> {
    let mut end = colon;
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && objective_c_identifier_byte(bytes[start - 1]) {
        start -= 1;
    }
    (start < end).then(|| validated_utf8(&bytes[start..end]).to_string())
}

fn objective_c_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn objective_c_unary_selector(bytes: &[u8]) -> Option<String> {
    let start = objective_c_unary_start(bytes)?;
    let end = objective_c_identifier_end(bytes, start);
    (start < end).then(|| validated_utf8(&bytes[start..end]).to_string())
}

fn objective_c_unary_start(bytes: &[u8]) -> Option<usize> {
    let marker = bytes.iter().position(|byte| matches!(*byte, b'-' | b'+'))?;
    let close_paren = bytes[marker..]
        .iter()
        .position(|byte| *byte == b')')
        .map(|offset| marker + offset + 1)?;
    let mut start = close_paren;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    Some(start)
}

fn objective_c_identifier_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && objective_c_identifier_byte(bytes[end]) {
        end += 1;
    }
    end
}

fn qualified_owner(node: Node<'_>, source: &[u8], language: Language) -> Option<String> {
    let separator = owner_separator(language);
    let mut owners = Vec::new();
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        if let Some(owner) = rust_implementation_owner(candidate, source, language) {
            owners.push(owner);
        }
        if let Some(owner) = declaration_owner(candidate, source, language) {
            owners.push(owner);
        }
        parent = candidate.parent();
    }
    owners.reverse();
    (!owners.is_empty()).then(|| owners.join(separator))
}

fn rust_implementation_owner(node: Node<'_>, source: &[u8], language: Language) -> Option<String> {
    if language != Language::Rust || node.kind() != "impl_item" {
        return None;
    }
    let implementation = nonempty_field_text(node, "type", source)?;
    Some(nonempty_field_text(node, "trait", source).map_or_else(
        || implementation.clone(),
        |r#trait| format!("{implementation} as {trait}"),
    ))
}

fn declaration_owner(node: Node<'_>, source: &[u8], language: Language) -> Option<String> {
    DECLARATION_OWNER_RESOLVERS[language_index(language)](node)
        .and_then(|owner| nonempty_node_text(owner, source))
}

fn no_declaration_owner(_node: Node<'_>) -> Option<Node<'_>> {
    None
}

fn rust_declaration_owner(node: Node<'_>) -> Option<Node<'_>> {
    declaration_name(node, "mod_item")
}

fn typescript_declaration_owner(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(node.kind(), "class_declaration" | "interface_declaration") {
        return node.child_by_field_name("name");
    }
    if matches!(
        node.kind(),
        "internal_module" | "module" | "namespace_declaration"
    ) {
        return node.child_by_field_name("name").or_else(|| first_name_node(node));
    }
    None
}

fn python_declaration_owner(node: Node<'_>) -> Option<Node<'_>> {
    declaration_name(node, "class_definition")
}

fn declaration_name<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    (node.kind() == kind)
        .then(|| node.child_by_field_name("name"))
        .flatten()
}

fn objective_c_declaration_owner(node: Node<'_>) -> Option<Node<'_>> {
    declaration_owner_for_kinds(node, "class_implementation|class_interface", true)
}

fn c_family_declaration_owner(node: Node<'_>) -> Option<Node<'_>> {
    declaration_owner_for_kinds(node, C_FAMILY_OWNER_TYPES, false)
}

fn swift_declaration_owner(node: Node<'_>) -> Option<Node<'_>> {
    declaration_owner_for_kinds(node, SWIFT_OWNER_TYPES, false)
}

fn declaration_owner_for_kinds<'tree>(
    node: Node<'tree>,
    kinds: &str,
    fallback_to_first_name: bool,
) -> Option<Node<'tree>> {
    is_kind(node.kind(), kinds)
        .then(|| {
            node.child_by_field_name("name")
                .or_else(|| fallback_to_first_name.then(|| first_name_node(node)).flatten())
        })
        .flatten()
}

fn nonempty_field_text(node: Node<'_>, field: &str, source: &[u8]) -> Option<String> {
    node.child_by_field_name(field)
        .and_then(|owner| nonempty_node_text(owner, source))
}

fn nonempty_node_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    let value = node_text(node, source).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn owner_separator(language: Language) -> &'static str {
    if matches!(language, Language::Rust | Language::Cpp) {
        "::"
    } else {
        "."
    }
}

fn stable_function_symbol(
    node: Node<'_>,
    source: &[u8],
    language: Language,
    name: &str,
    locals: &BTreeSet<String>,
    normalized_tokens: &[String],
) -> String {
    let symbol_name = qualified_owner(node, source, language).map_or_else(
        || name.to_string(),
        |owner| {
            if name.starts_with(&owner) {
                name.to_string()
            } else {
                let separator = owner_separator(language);
                let owner_leaf = owner.rsplit(separator).next().unwrap_or(owner.as_str());
                if name.starts_with(&format!("{owner_leaf}{separator}")) {
                    owner.strip_suffix(owner_leaf).map_or_else(
                        || format!("{owner}{separator}{name}"),
                        |prefix| format!("{prefix}{name}"),
                    )
                } else {
                    format!("{owner}{separator}{name}")
                }
            }
        },
    );
    let parameters = parameter_nodes(node, source, language);
    let normalization = NormalizationContext::new(source, language, locals);
    let mut signature = Vec::new();
    for parameter in parameters {
        signature.push(normalization.tokens(parameter).join(" "));
    }
    let signature = signature.join(", ");
    if name.starts_with('<') {
        let evidence = normalized_tokens.join("\u{1f}");
        let digest = stable_id("adapter.function.anonymous", "anonymous", name, &evidence);
        return format!("{}:{}({signature})", name.trim_end_matches('>'), &digest[..12]);
    }
    format!("{symbol_name}({signature})")
}

fn disambiguate_duplicate_symbols(functions: &mut [FunctionRecord]) {
    let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, function) in functions.iter().enumerate() {
        groups
            .entry(function.stable_symbol.clone())
            .or_default()
            .push(index);
    }
    for (symbol, indices) in groups {
        if indices.len() < 2 {
            continue;
        }
        disambiguate_symbol_group(functions, &symbol, indices);
    }
}

fn disambiguate_symbol_group(functions: &mut [FunctionRecord], symbol: &str, indices: Vec<usize>) {
    let mut occurrences = BTreeMap::<String, usize>::new();
    for index in indices {
        let function = &functions[index];
        let evidence = function.normalized_tokens.join("\u{1f}");
        let digest = stable_id("adapter.function.duplicate", &function.file, symbol, &evidence);
        let short = digest[..12].to_string();
        let suffix = duplicate_symbol_suffix(&mut occurrences, short);
        functions[index].stable_symbol = format!("{symbol}#{suffix}");
    }
}

fn duplicate_symbol_suffix(occurrences: &mut BTreeMap<String, usize>, short: String) -> String {
    let occurrence = occurrences.entry(short.clone()).or_default();
    let suffix = if *occurrence == 0 {
        short
    } else {
        format!("{short}:{}", occurrence.saturating_add(1))
    };
    *occurrence = occurrence.saturating_add(1);
    suffix
}

fn parameter_nodes<'tree>(node: Node<'tree>, source: &[u8], language: Language) -> Vec<Node<'tree>> {
    if let Some(parameters) = node.child_by_field_name("parameters") {
        return parameter_children(parameters, source, source_language_void_parameter(language));
    }

    let signature_end = signature_end(node);
    if let Some(parameters) = walk_nodes(node).into_iter().find(|candidate| {
        candidate.end_byte() <= signature_end
            && matches!(
                candidate.kind(),
                "parameters" | "formal_parameters" | "parameter_list" | "parameter_clause"
            )
    }) {
        return parameter_children(parameters, source, source_language_void_parameter(language));
    }

    direct_children(node)
        .into_iter()
        .filter(|child| matches!(child.kind(), "parameter" | "method_parameter"))
        .collect()
}

fn source_language_void_parameter(language: Language) -> bool {
    matches!(language, Language::C | Language::Cpp | Language::ObjectiveC)
}

fn parameter_children<'tree>(
    parameters: Node<'tree>,
    source: &[u8],
    c_family_void: bool,
) -> Vec<Node<'tree>> {
    let children = named_children(parameters)
        .into_iter()
        .filter(|child| !is_kind(child.kind(), COMMENT_TYPES))
        .collect::<Vec<_>>();
    if c_family_void && children.len() == 1 && matches!(node_text(children[0], source).trim(), "void") {
        return Vec::new();
    }
    children
}

fn local_bindings(node: Node<'_>, source: &[u8], language: Language) -> BTreeSet<String> {
    let mut locals = BTreeSet::new();
    for parameter in parameter_nodes(node, source, language) {
        collect_parameter_binding(parameter, source, language, &mut locals);
    }

    {
        let mut collector = BindingCollector {
            source,
            language,
            locals: &mut locals,
        };
        for candidate in owned_nodes(node, language) {
            collector.collect(candidate);
        }
    }

    if language == Language::Python {
        collect_python_local_bindings(node, source, &mut locals);
    }
    locals
}

struct BindingCollector<'source, 'locals> {
    source: &'source [u8],
    language: Language,
    locals: &'locals mut BTreeSet<String>,
}

impl BindingCollector<'_, '_> {
    fn collect(&mut self, candidate: Node<'_>) {
        if self.collect_c_declaration(candidate) {
            return;
        }
        if let Some(root) = local_binding_root(candidate, self.language) {
            collect_binding_identifiers(root, self.source, self.locals);
        }
    }

    fn collect_c_declaration(&mut self, candidate: Node<'_>) -> bool {
        if !matches!(self.language, Language::C | Language::Cpp | Language::ObjectiveC)
            || candidate.kind() != "declaration"
        {
            return false;
        }
        let mut cursor = candidate.walk();
        for declarator in candidate.children_by_field_name("declarator", &mut cursor) {
            let binding = declarator
                .child_by_field_name("declarator")
                .filter(|_| declarator.kind() == "init_declarator")
                .unwrap_or(declarator);
            collect_c_declarator_bindings(binding, self.source, self.locals);
        }
        true
    }
}

fn local_binding_root(candidate: Node<'_>, language: Language) -> Option<Node<'_>> {
    const ALL: u8 = u8::MAX;
    const BASH: u8 = 1 << (Language::Bash as usize);
    const PYTHON: u8 = 1 << (Language::Python as usize);
    const SWIFT: u8 = 1 << (Language::Swift as usize);
    const RUST_SWIFT_TYPESCRIPT: u8 =
        (1 << (Language::Rust as usize)) | SWIFT | (1 << (Language::TypeScript as usize));
    const RULES: &[(u8, &str, &str)] = &[
        (ALL, "let_declaration", "pattern|name"),
        (ALL, "variable_declarator", "name"),
        (SWIFT, "property_declaration", "name"),
        (BASH, "variable_assignment", "name"),
        (PYTHON, "assignment|named_expression", "left"),
        (PYTHON, "except_clause|as_pattern|aliased_import", "alias"),
        (PYTHON, "for_statement", "pattern|left"),
        (
            RUST_SWIFT_TYPESCRIPT,
            "for_statement|for_in_statement|for_expression",
            "pattern|left",
        ),
        (ALL, "catch_clause", "parameter|name"),
    ];
    let language_flag = 1 << language_index(language);
    RULES
        .iter()
        .find(|(languages, kinds, _)| languages & language_flag != 0 && is_kind(candidate.kind(), *kinds))
        .and_then(|(_, _, fields)| {
            fields
                .split('|')
                .find_map(|field| candidate.child_by_field_name(field))
        })
}

fn collect_python_local_bindings(node: Node<'_>, source: &[u8], locals: &mut BTreeSet<String>) {
    for statement in python_owned_nodes(node, "import_statement|import_from_statement") {
        collect_python_import_bindings(statement, source, locals);
    }
    collect_python_nested_function_bindings(node, source, locals);
    for declaration in python_owned_nodes(node, "global_statement|nonlocal_statement") {
        for identifier in walk_nodes(declaration)
            .into_iter()
            .filter(|candidate| is_binding_identifier(candidate.kind()))
        {
            locals.remove(node_text(identifier, source).trim());
        }
    }
}

fn python_owned_nodes<'tree>(root: Node<'tree>, kinds: &str) -> Vec<Node<'tree>> {
    owned_nodes(root, Language::Python)
        .into_iter()
        .filter(|candidate| is_kind(candidate.kind(), kinds))
        .collect()
}

fn collect_python_import_bindings(statement: Node<'_>, source: &[u8], locals: &mut BTreeSet<String>) {
    let mut cursor = statement.walk();
    for imported in statement.children_by_field_name("name", &mut cursor) {
        if let Some(binding) = python_import_binding(statement.kind(), imported) {
            collect_binding_identifiers(binding, source, locals);
        }
    }
}

fn python_import_binding<'tree>(statement_kind: &str, imported: Node<'tree>) -> Option<Node<'tree>> {
    if imported.kind() == "aliased_import" {
        return imported.child_by_field_name("alias");
    }
    let identifiers = walk_nodes(imported)
        .into_iter()
        .filter(|candidate| is_binding_identifier(candidate.kind()))
        .collect::<Vec<_>>();
    if statement_kind == "import_statement" {
        identifiers.first().copied()
    } else {
        identifiers.last().copied()
    }
}

fn collect_python_nested_function_bindings(root: Node<'_>, source: &[u8], locals: &mut BTreeSet<String>) {
    visit_descendants(root, |child| {
        if let Some(name) = python_nested_function_name(root, child) {
            collect_binding_identifiers(name, source, locals);
        }
        !python_nested_boundary(root, child)
    });
}

fn python_nested_function_name<'tree>(root: Node<'tree>, child: Node<'tree>) -> Option<Node<'tree>> {
    if child == root || child.kind() != "function_definition" {
        return None;
    }
    child.child_by_field_name("name")
}

fn python_nested_boundary(root: Node<'_>, child: Node<'_>) -> bool {
    child != root && is_kind(child.kind(), function_boundary_types(Language::Python))
}

fn collect_parameter_binding(
    parameter: Node<'_>,
    source: &[u8],
    language: Language,
    locals: &mut BTreeSet<String>,
) {
    if matches!(language, Language::C | Language::Cpp | Language::ObjectiveC) {
        if let Some(declarator) = parameter.child_by_field_name("declarator") {
            collect_c_declarator_bindings(declarator, source, locals);
            return;
        }
    }
    for field in ["name", "pattern", "declarator"] {
        if let Some(binding) = parameter.child_by_field_name(field) {
            collect_binding_identifiers(binding, source, locals);
            return;
        }
    }
    if is_binding_identifier(parameter.kind()) {
        collect_binding_identifiers(parameter, source, locals);
    }
}

fn collect_c_declarator_bindings(node: Node<'_>, source: &[u8], locals: &mut BTreeSet<String>) {
    for binding in c_declarator_bindings(node) {
        collect_binding_identifiers(binding, source, locals);
    }
}

fn c_declarator_bindings(node: Node<'_>) -> Vec<Node<'_>> {
    if is_binding_identifier(node.kind()) {
        return vec![node];
    }
    if node.kind() == "structured_binding_declarator" {
        return named_children(node)
            .into_iter()
            .filter(|child| is_binding_identifier(child.kind()))
            .collect();
    }
    if let Some(declarator) = node.child_by_field_name("declarator") {
        return c_declarator_bindings(declarator);
    }
    if node.kind() == "parenthesized_declarator" {
        return named_children(node)
            .into_iter()
            .flat_map(c_declarator_bindings)
            .collect();
    }
    Vec::new()
}

fn collect_binding_identifiers(node: Node<'_>, source: &[u8], locals: &mut BTreeSet<String>) {
    if let Some(identifier) = binding_identifier(node, source) {
        locals.insert(identifier);
        return;
    }
    for child in named_children(node)
        .into_iter()
        .filter(|child| !child.kind().contains("type") && child.kind() != "property_identifier")
    {
        collect_binding_identifiers(child, source, locals);
    }
}

fn binding_identifier(node: Node<'_>, source: &[u8]) -> Option<String> {
    if !is_binding_identifier(node.kind()) {
        return None;
    }
    let identifier = node_text(node, source);
    let identifier = identifier.trim().trim_start_matches("r#");
    (!identifier.is_empty()).then(|| identifier.to_string())
}

fn is_binding_identifier(kind: &str) -> bool {
    is_kind(kind, BINDING_IDENTIFIER_TYPES)
}

fn owned_nodes(root: Node<'_>, language: Language) -> Vec<Node<'_>> {
    traversed_nodes(root, |child| {
        child != root && is_kind(child.kind(), function_boundary_types(language))
    })
}

#[derive(Clone, Copy)]
struct NormalizationContext<'source> {
    source: &'source [u8],
    language: Language,
    locals: &'source BTreeSet<String>,
}

impl<'source> NormalizationContext<'source> {
    const fn new(source: &'source [u8], language: Language, locals: &'source BTreeSet<String>) -> Self {
        Self {
            source,
            language,
            locals,
        }
    }

    fn function_tokens(self, node: Node<'_>) -> Vec<String> {
        self.tokens(node.child_by_field_name("body").unwrap_or(node))
    }

    fn tokens(self, root: Node<'_>) -> Vec<String> {
        let mut tokens = Vec::new();
        self.collect_node(root, root, &mut tokens);
        tokens
    }

    fn collect_node(self, node: Node<'_>, root: Node<'_>, tokens: &mut Vec<String>) {
        if skip_normalized_node(node, root, self.language) {
            return;
        }
        if let Some(token) = self.leaf(node) {
            tokens.push(token);
            return;
        }
        for child in direct_children(node) {
            self.collect_node(child, root, tokens);
        }
    }

    fn leaf(self, node: Node<'_>) -> Option<String> {
        if is_literal_node(node, self.source, self.language) {
            return Some("LITERAL".to_string());
        }
        if node.child_count() != 0 {
            return None;
        }
        let text = node_text(node, self.source);
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(self.normalized_leaf(node, text))
    }

    fn normalized_leaf(self, node: Node<'_>, text: &str) -> String {
        let identifier = text.trim_start_matches("r#");
        if is_kind(node.kind(), IDENTIFIER_TYPES) && self.is_local(node, identifier) {
            "LOCAL".to_string()
        } else {
            text.to_string()
        }
    }

    fn is_local(self, node: Node<'_>, identifier: &str) -> bool {
        self.locals.contains(identifier)
            || (self.language == Language::Python
                && python_comprehension_binding_in_scope(node, self.source, identifier))
            || (matches!(self.language, Language::C | Language::Cpp | Language::ObjectiveC)
                && c_parameter_binding_at(node, self.source, identifier))
    }

    fn references(self, node: Node<'_>) -> BTreeSet<String> {
        let mut references = BTreeSet::new();
        let root = node.child_by_field_name("body").unwrap_or(node);
        self.collect_identifiers(root, &mut references);
        self.collect_calls(root, &mut references);
        references
    }

    fn collect_identifiers(self, root: Node<'_>, references: &mut BTreeSet<String>) {
        let identifiers = owned_nodes(root, self.language)
            .into_iter()
            .filter(|candidate| candidate.child_count() == 0)
            .filter(|candidate| is_kind(candidate.kind(), IDENTIFIER_TYPES));
        extend_references(references, identifiers, |identifier_node| {
            let identifier = node_text(identifier_node, self.source);
            let identifier = identifier.trim().trim_start_matches("r#");
            (!identifier.is_empty() && !self.is_local(identifier_node, identifier))
                .then(|| identifier.to_string())
        });
    }

    fn collect_calls(self, root: Node<'_>, references: &mut BTreeSet<String>) {
        let calls = owned_nodes(root, self.language)
            .into_iter()
            .filter(|candidate| is_call_kind(candidate.kind()));
        extend_references(references, calls, |call| self.call_reference(call));
    }

    fn call_reference(self, call: Node<'_>) -> Option<String> {
        let target_node = call_target(call)?;
        let target = node_text(target_node, self.source);
        let target = target.trim();
        if !valid_call_target(target) {
            return None;
        }
        let root = call_target_root(target);
        if root.is_empty() || self.is_local(target_node, root) {
            return None;
        }
        Some(target.to_string())
    }
}

fn skip_normalized_node(node: Node<'_>, root: Node<'_>, language: Language) -> bool {
    [
        node.is_error(),
        node.is_missing(),
        is_kind(node.kind(), COMMENT_TYPES),
        node != root && is_kind(node.kind(), function_boundary_types(language)),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn python_comprehension_binding_in_scope(node: Node<'_>, source: &[u8], identifier: &str) -> bool {
    ancestors(node).any(|candidate| {
        is_python_comprehension(candidate.kind())
            && python_comprehension_name_visible(PythonVisibilityQuery {
                comprehension: candidate,
                occurrence: node,
                source,
                identifier,
            })
    })
}

#[derive(Clone, Copy)]
struct PythonVisibilityQuery<'tree, 'source> {
    comprehension: Node<'tree>,
    occurrence: Node<'tree>,
    source: &'source [u8],
    identifier: &'source str,
}

fn python_comprehension_name_visible(query: PythonVisibilityQuery<'_, '_>) -> bool {
    if query
        .comprehension
        .child_by_field_name("body")
        .is_some_and(|body| node_contains(body, query.occurrence))
    {
        return named_children(query.comprehension)
            .into_iter()
            .filter(|child| child.kind() == "for_in_clause")
            .any(|clause| python_for_clause_binds(clause, query.source, query.identifier));
    }

    python_comprehension_clause_visibility(query)
}

fn python_comprehension_clause_visibility(query: PythonVisibilityQuery<'_, '_>) -> bool {
    let mut visible = false;
    for child in named_children(query.comprehension) {
        if let Some(result) = python_child_visibility(child, query, &mut visible) {
            return result;
        }
    }
    false
}

fn python_child_visibility(
    child: Node<'_>,
    query: PythonVisibilityQuery<'_, '_>,
    visible: &mut bool,
) -> Option<bool> {
    if child.kind() != "for_in_clause" {
        return node_contains(child, query.occurrence).then_some(*visible);
    }
    match python_clause_occurrence(child, query) {
        PythonClauseOccurrence::Outside { binds } => {
            *visible |= binds;
            None
        }
        PythonClauseOccurrence::Binding { binds } => Some(*visible || binds),
        PythonClauseOccurrence::Iterable => Some(*visible),
    }
}

enum PythonClauseOccurrence {
    Outside { binds: bool },
    Binding { binds: bool },
    Iterable,
}

fn python_clause_occurrence(
    clause: Node<'_>,
    query: PythonVisibilityQuery<'_, '_>,
) -> PythonClauseOccurrence {
    let binds = python_for_clause_binds(clause, query.source, query.identifier);
    if clause
        .child_by_field_name("left")
        .is_some_and(|left| node_contains(left, query.occurrence))
    {
        return PythonClauseOccurrence::Binding { binds };
    }
    let mut cursor = clause.walk();
    if clause
        .children_by_field_name("right", &mut cursor)
        .any(|right| node_contains(right, query.occurrence))
    {
        return PythonClauseOccurrence::Iterable;
    }
    PythonClauseOccurrence::Outside { binds }
}

fn python_for_clause_binds(clause: Node<'_>, source: &[u8], identifier: &str) -> bool {
    let Some(left) = clause.child_by_field_name("left") else {
        return false;
    };
    let mut bindings = BTreeSet::new();
    collect_binding_identifiers(left, source, &mut bindings);
    bindings.contains(identifier)
}

fn is_python_comprehension(kind: &str) -> bool {
    is_kind(kind, PYTHON_COMPREHENSION_TYPES)
}

fn c_parameter_binding_at(node: Node<'_>, source: &[u8], identifier: &str) -> bool {
    for candidate in ancestors(node) {
        if candidate.kind() == "parameter_declaration" {
            return candidate
                .child_by_field_name("declarator")
                .into_iter()
                .flat_map(c_declarator_bindings)
                .any(|binding| {
                    binding == node
                        && node_text(binding, source).trim().trim_start_matches("r#") == identifier
                });
        }
    }
    false
}

fn ancestors(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    std::iter::successors(node.parent(), Node::parent)
}

fn node_contains(container: Node<'_>, candidate: Node<'_>) -> bool {
    container.start_byte() <= candidate.start_byte() && candidate.end_byte() <= container.end_byte()
}

fn is_literal_node(node: Node<'_>, source: &[u8], language: Language) -> bool {
    if literal_syntax_kind(node.kind()) {
        return true;
    }
    if language == Language::Bash && bash_word_is_literal(node, source) {
        return true;
    }
    node.child_count() == 0
        && matches!(
            node_text(node, source).trim(),
            "true" | "false" | "True" | "False" | "None" | "null" | "nil" | "YES" | "NO"
        )
}

fn literal_syntax_kind(kind: &str) -> bool {
    [
        is_kind(kind, STRING_TYPES),
        is_kind(kind, NUMBER_TYPES),
        is_kind(kind, BOOLEAN_LITERAL_TYPES),
        kind.contains("literal"),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn bash_word_is_literal(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "word" {
        return false;
    }
    bash_word_is_numeric(node_text(node, source).trim()) || bash_word_is_command_argument(node)
}

fn bash_word_is_numeric(text: &str) -> bool {
    !text.is_empty()
        && text
            .trim_start_matches(['+', '-'])
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
}

fn bash_word_is_command_argument(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "command"
            && named_children(parent)
                .into_iter()
                .find(|child| child.kind() != "variable_assignment")
                .is_some_and(|command| command != node)
    })
}

fn is_call_kind(kind: &str) -> bool {
    is_kind(
        kind,
        "call_expression|call|command|invocation_expression|macro_invocation|constructor_expression",
    )
}

fn extend_references<'tree>(
    references: &mut BTreeSet<String>,
    nodes: impl IntoIterator<Item = Node<'tree>>,
    mut resolve: impl FnMut(Node<'tree>) -> Option<String>,
) {
    references.extend(nodes.into_iter().filter_map(&mut resolve));
}

fn call_target(call: Node<'_>) -> Option<Node<'_>> {
    ["function", "callee", "name", "macro", "command"]
        .into_iter()
        .find_map(|field| call.child_by_field_name(field))
        .or_else(|| named_children(call).into_iter().next())
}

fn valid_call_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 256
        && !target.chars().any(char::is_whitespace)
        && target.chars().all(|character| {
            character.is_alphanumeric() || matches!(character, '_' | ':' | '.' | '!' | '$' | '-' | '>' | '?')
        })
}

fn call_target_root(target: &str) -> &str {
    target
        .trim_start_matches(['&', '*'])
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .next()
        .unwrap_or_default()
}

fn function_visibility(node: Node<'_>, source: &[u8], language: Language) -> SymbolVisibility {
    let signature_end = signature_end(node);
    let signature = validated_utf8(&source[node.start_byte()..signature_end.min(source.len())]);
    explicit_visibility(signature).unwrap_or_else(|| language_visibility(signature, language))
}

fn signature_end(node: Node<'_>) -> usize {
    node.child_by_field_name("body")
        .map_or(node.end_byte(), |body| body.start_byte())
}

fn explicit_visibility(signature: &str) -> Option<SymbolVisibility> {
    const KEYWORDS: &[(&str, SymbolVisibility)] = &[
        ("private", SymbolVisibility::Private),
        ("fileprivate", SymbolVisibility::Private),
        ("protected", SymbolVisibility::Private),
        ("public", SymbolVisibility::Public),
        ("open", SymbolVisibility::Public),
        ("export", SymbolVisibility::Public),
    ];
    KEYWORDS
        .iter()
        .find(|(keyword, _)| contains_word(signature, keyword))
        .map(|(_, visibility)| *visibility)
}

fn language_visibility(signature: &str, language: Language) -> SymbolVisibility {
    type Resolver = fn(&str) -> SymbolVisibility;
    const RESOLVERS: [Resolver; 8] = [
        unknown_visibility,
        c_family_visibility,
        c_family_visibility,
        c_family_visibility,
        unknown_visibility,
        rust_visibility,
        swift_visibility,
        unknown_visibility,
    ];
    RESOLVERS[language_index(language)](signature)
}

fn rust_visibility(signature: &str) -> SymbolVisibility {
    if ["pub(crate", "pub(super", "pub(in"]
        .into_iter()
        .any(|prefix| signature.contains(prefix))
    {
        return SymbolVisibility::Crate;
    }
    if contains_word(signature, "pub") {
        SymbolVisibility::Public
    } else {
        SymbolVisibility::Private
    }
}

fn c_family_visibility(signature: &str) -> SymbolVisibility {
    if contains_word(signature, "static") {
        SymbolVisibility::Private
    } else {
        SymbolVisibility::Unknown
    }
}

fn swift_visibility(_signature: &str) -> SymbolVisibility {
    SymbolVisibility::Crate
}

fn unknown_visibility(_signature: &str) -> SymbolVisibility {
    SymbolVisibility::Unknown
}

fn contains_word(value: &str, word: &str) -> bool {
    value
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .any(|candidate| candidate == word)
}

fn is_entry_point(name: &str) -> bool {
    let basename = name
        .rsplit("::")
        .next()
        .unwrap_or(name)
        .rsplit('.')
        .next()
        .unwrap_or(name);
    basename == "main"
}

fn nesting_depth(node: Node<'_>, language: Language) -> u32 {
    let mut maximum = 0_u32;
    let mut stack = direct_children(node)
        .into_iter()
        .map(|child| (child, 0_u32))
        .collect::<Vec<_>>();
    while let Some((candidate, parent_depth)) = stack.pop() {
        if is_kind(candidate.kind(), function_boundary_types(language)) {
            continue;
        }
        let depth = if is_nesting_node(candidate.kind(), language) {
            parent_depth.saturating_add(1)
        } else {
            parent_depth
        };
        maximum = maximum.max(depth);
        stack.extend(direct_children(candidate).into_iter().map(|child| (child, depth)));
    }
    maximum
}

fn is_nesting_node(kind: &str, language: Language) -> bool {
    let kinds = match language {
        Language::Bash => {
            "if_statement|for_statement|c_style_for_statement|while_statement|case_statement|conditional_expression"
        }
        Language::C | Language::Cpp | Language::ObjectiveC => {
            "if_statement|for_statement|while_statement|do_statement|switch_statement|try_statement|catch_clause|conditional_expression"
        }
        Language::Python => {
            "if_statement|for_statement|while_statement|try_statement|with_statement|match_statement|conditional_expression|list_comprehension|set_comprehension|dictionary_comprehension|generator_expression"
        }
        Language::Rust => "if_expression|for_expression|while_expression|loop_expression|match_expression",
        Language::Swift => {
            "if_statement|guard_statement|for_statement|while_statement|repeat_while_statement|switch_statement|do_statement|catch_clause|ternary_expression"
        }
        Language::TypeScript => {
            "if_statement|for_statement|for_in_statement|while_statement|do_statement|switch_statement|try_statement|catch_clause|ternary_expression"
        }
    };
    is_kind(kind, kinds)
}

fn statement_count(node: Node<'_>, language: Language) -> u32 {
    let count = owned_nodes(node, language)
        .into_iter()
        .skip(1)
        .filter(|candidate| is_statement_node(*candidate, language))
        .count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn is_statement_node(node: Node<'_>, language: Language) -> bool {
    type StatementPredicate = for<'tree> fn(Node<'tree>) -> bool;
    const PREDICATES: [StatementPredicate; 8] = [
        bash_statement,
        c_family_statement,
        c_family_statement,
        c_family_statement,
        python_statement,
        rust_statement,
        swift_statement,
        typescript_statement,
    ];
    PREDICATES[language_index(language)](node)
}

fn bash_statement(node: Node<'_>) -> bool {
    node.kind().ends_with("_statement")
        || matches!(
            node.kind(),
            "command" | "variable_assignment" | "redirected_statement"
        )
}

fn c_family_statement(node: Node<'_>) -> bool {
    is_kind(
        node.kind(),
        "if_statement|for_statement|while_statement|do_statement|switch_statement|case_statement|return_statement|break_statement|continue_statement|goto_statement|expression_statement|labeled_statement|try_statement|throw_statement|declaration",
    )
}

fn python_statement(node: Node<'_>) -> bool {
    node.kind().ends_with("_statement") && node.kind() != "block"
}

fn rust_statement(node: Node<'_>) -> bool {
    node.parent().is_some_and(|parent| parent.kind() == "block")
        && !matches!(
            node.kind(),
            "attribute_item" | "inner_attribute_item" | "function_item" | "struct_item" | "enum_item"
        )
}

fn swift_statement(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|parent| matches!(parent.kind(), "statements" | "function_body"))
}

fn typescript_statement(node: Node<'_>) -> bool {
    (node.kind().ends_with("_statement") && node.kind() != "statement_block")
        || matches!(node.kind(), "lexical_declaration" | "variable_declaration")
}

fn complexity(node: Node<'_>, source: &[u8], language: Language) -> u32 {
    let mut total = 1_u32;
    let mut stack = direct_children(node);
    while let Some(candidate) = stack.pop() {
        if is_kind(candidate.kind(), function_boundary_types(language)) {
            continue;
        }
        total = total.saturating_add(decision_increment(candidate, language));
        let children = direct_children(candidate);
        let operators = children
            .iter()
            .filter(|child| matches!(node_text(**child, source).trim(), "&&" | "||" | "and" | "or"))
            .count();
        total = total.saturating_add(u32::try_from(operators).unwrap_or(u32::MAX));
        stack.extend(children);
    }
    total
}

fn decision_increment(node: Node<'_>, language: Language) -> u32 {
    if language != Language::Rust {
        return u32::from(is_kind(node.kind(), decision_types(language)));
    }
    match node.kind() {
        // The first arm is the existing path. Every later arm, including a
        // wildcard/default arm, adds one independent match alternative.
        "match_arm" => u32::from(
            node.prev_named_sibling()
                .is_some_and(|sibling| sibling.kind() == "match_arm"),
        ),
        "match_pattern" => u32::from(node.child_by_field_name("condition").is_some()),
        "let_declaration" => u32::from(node.child_by_field_name("alternative").is_some()),
        "try_expression" => 1,
        _ => u32::from(is_kind(node.kind(), decision_types(language))),
    }
}

fn normalized_tokens(root: Node<'_>, source: &[u8], language: Language) -> Vec<TokenRecord> {
    let mut tokens = Vec::new();
    for node in leaf_nodes(root).filter(|node| !in_error_region(*node)) {
        append_normalized_token(&mut tokens, node, source, language);
    }
    tokens
}

fn append_normalized_token(tokens: &mut Vec<TokenRecord>, node: Node<'_>, source: &[u8], language: Language) {
    let Some(normalized) = normalized_token(node, source, language) else {
        return;
    };
    tokens.push(TokenRecord {
        value: normalized.into_owned(),
        line: one_based(node.start_position().row),
        index: tokens.len(),
    });
}

fn normalized_token<'source>(
    node: Node<'_>,
    source: &'source [u8],
    language: Language,
) -> Option<Cow<'source, str>> {
    if is_kind(node.kind(), COMMENT_TYPES) || has_ancestor_kind(node, COMMENT_TYPES) {
        return None;
    }
    let normalized = normalized_token_value(node, source, language);
    (!normalized.trim().is_empty()).then_some(normalized)
}

fn normalized_token_value<'source>(
    node: Node<'_>,
    source: &'source [u8],
    language: Language,
) -> Cow<'source, str> {
    if token_is_literal(node, source, language) {
        return Cow::Borrowed("LITERAL");
    }
    node_text(node, source)
}

fn token_is_literal(node: Node<'_>, source: &[u8], language: Language) -> bool {
    [
        is_literal_node(node, source, language),
        has_ancestor_kind(node, STRING_TYPES),
        has_ancestor_kind(node, NUMBER_TYPES),
    ]
    .into_iter()
    .any(std::convert::identity)
}

fn enumerate_mutations(root: Node<'_>, source: &[u8], file: &SourceFile) -> Vec<MutationCandidate> {
    let mut candidates = leaf_nodes(root)
        .filter(|node| !in_mutation_error_region(*node))
        .filter_map(|node| {
            mutation_replacement(node, source, file.language).map(|replacement| (node, replacement))
        })
        .map(|(node, replacement)| {
            MutationCandidate::new(
                file.language,
                &file.relative,
                (
                    one_based(node.start_position().row),
                    unicode_scalar_column(source, node.start_byte(), node.start_position().column),
                ),
                node_text(node, source),
                replacement,
                node.start_byte()..node.end_byte(),
            )
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        (
            left.start_byte,
            Reverse(left.end_byte.saturating_sub(left.start_byte)),
            &left.replacement,
        )
            .cmp(&(
                right.start_byte,
                Reverse(right.end_byte.saturating_sub(right.start_byte)),
                &right.replacement,
            ))
    });
    let mut selected = Vec::new();
    let mut last_end = None;
    for candidate in candidates {
        if last_end.is_some_and(|end| candidate.start_byte < end) {
            continue;
        }
        last_end = Some(candidate.end_byte);
        selected.push(candidate);
    }
    selected
}

fn leaf_nodes(root: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    walk_nodes(root)
        .into_iter()
        .filter(|node| node.child_count() == 0)
}

fn mutation_replacement(node: Node<'_>, source: &[u8], language: Language) -> Option<&'static str> {
    if has_ancestor_kind(node, SKIPPED_MUTATION_ANCESTORS)
        || has_ancestor_kind(node, NON_EXPRESSION_MUTATION_ANCESTORS)
    {
        return None;
    }
    let text = node_text(node, source);
    let replacement = language_replacement(language, text.as_ref())
        .or_else(|| common_replacement(text.as_ref()))
        .or_else(|| arithmetic_replacement(text.as_ref()))?;

    if requires_binary_context(text.as_ref()) {
        return valid_binary_context(node).then_some(replacement);
    }

    valid_boolean_literal_context(node, language).then_some(replacement)
}

fn requires_binary_context(value: &str) -> bool {
    value == "||" || is_kind(value, "==|!=|>|<|>=|<=|&&|or|and|-eq|-ne|-gt|-ge|-lt|-le|+|-|*|/")
}

const fn arithmetic_replacement(value: &str) -> Option<&'static str> {
    match value.as_bytes() {
        b"+" => Some("-"),
        b"-" => Some("+"),
        b"*" => Some("/"),
        b"/" => Some("*"),
        _ => None,
    }
}

fn valid_binary_context(node: Node<'_>) -> bool {
    let Some(context) = binary_context(node) else {
        return false;
    };
    if context.has_error() || in_mutation_error_region(context) {
        return false;
    }
    binary_context_has_operands(context, node)
}

fn binary_context(node: Node<'_>) -> Option<Node<'_>> {
    let parent = node.parent()?;
    if is_kind(parent.kind(), BINARY_CONTEXTS) {
        return Some(parent);
    }
    wrapped_binary_context(parent)
}

fn wrapped_binary_context(parent: Node<'_>) -> Option<Node<'_>> {
    if !is_kind(parent.kind(), OPERATOR_WRAPPERS) {
        return None;
    }
    let grandparent = parent.parent()?;
    is_kind(grandparent.kind(), BINARY_CONTEXTS).then_some(grandparent)
}

fn binary_context_has_operands(context: Node<'_>, operator: Node<'_>) -> bool {
    let mut cursor = context.walk();
    let mut before = false;
    let mut after = false;
    for child in context.children(&mut cursor).filter(|child| !child.is_extra()) {
        before |= !child.is_missing() && child.end_byte() <= operator.start_byte();
        after |= !child.is_missing() && child.start_byte() >= operator.end_byte();
    }
    before && after
}

fn valid_boolean_literal_context(node: Node<'_>, language: Language) -> bool {
    if in_mutation_error_region(node) {
        return false;
    }
    if is_kind(node.kind(), BOOLEAN_LITERAL_TYPES)
        || node
            .parent()
            .is_some_and(|parent| is_kind(parent.kind(), BOOLEAN_LITERAL_TYPES))
    {
        return true;
    }

    language == Language::ObjectiveC && node.kind() == "identifier"
}

fn common_replacement(value: &str) -> Option<&'static str> {
    replacement_from_encoding(
        value,
        concat!(
            "==\u{1f}!=\u{1e}!=\u{1f}==\u{1e}>\u{1f}<=\u{1e}<\u{1f}>=\u{1e}",
            ">=\u{1f}<\u{1e}<=\u{1f}>\u{1e}&&\u{1f}||\u{1e}||\u{1f}&&",
        ),
    )
    .or_else(|| replacement_from_encoding(value, "true\u{1f}false\u{1e}false\u{1f}true"))
}

fn language_replacement(language: Language, value: &str) -> Option<&'static str> {
    const REPLACEMENTS: [&str; 8] = [
        "-eq\u{1f}-ne\u{1e}-ne\u{1f}-eq\u{1e}-gt\u{1f}-le\u{1e}-ge\u{1f}-lt\u{1e}-lt\u{1f}-ge\u{1e}-le\u{1f}-gt",
        "",
        "",
        "YES\u{1f}NO\u{1e}NO\u{1f}YES",
        "True\u{1f}False\u{1e}False\u{1f}True\u{1e}and\u{1f}or\u{1e}or\u{1f}and",
        "",
        "",
        "",
    ];
    replacement_from_encoding(value, REPLACEMENTS[language_index(language)])
}

fn replacement_from_encoding(value: &str, replacements: &'static str) -> Option<&'static str> {
    replacements.split('\u{1e}').find_map(|pair| {
        let (original, replacement) = pair.split_once('\u{1f}')?;
        (original == value).then_some(replacement)
    })
}

fn has_ancestor_kind<K: KindSet + ?Sized>(node: Node<'_>, kinds: &K) -> bool {
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        if is_kind(candidate.kind(), kinds) {
            return true;
        }
        parent = candidate.parent();
    }
    false
}

fn in_error_region(node: Node<'_>) -> bool {
    if node.is_error() || node.is_missing() {
        return true;
    }
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        if candidate.is_error() || candidate.is_missing() {
            return true;
        }
        parent = candidate.parent();
    }
    false
}

fn in_mutation_error_region(node: Node<'_>) -> bool {
    let mut candidate = Some(node);
    while let Some(current) = candidate {
        if current.is_error() || current.is_missing() {
            return true;
        }
        if current.has_error() && !is_kind(current.kind(), PARSE_ROOT_TYPES) {
            return true;
        }
        candidate = current.parent();
    }
    false
}

fn node_text<'source>(node: Node<'_>, source: &'source [u8]) -> Cow<'source, str> {
    Cow::Borrowed(validated_utf8(&source[node.byte_range()]))
}

fn validated_utf8(bytes: &[u8]) -> &str {
    // `read_source` validates the complete source before Tree-sitter sees it,
    // and syntax-node ranges preserve UTF-8 boundaries. Keep extraction
    // lossless so distinct invalid byte sequences can never collapse to the
    // same replacement character in tokens or mutation candidates.
    std::str::from_utf8(bytes).unwrap_or_default()
}

fn walk_nodes(root: Node<'_>) -> Vec<Node<'_>> {
    traversed_nodes(root, |_| false)
}

fn visit_descendants<'tree>(root: Node<'tree>, mut descend: impl FnMut(Node<'tree>) -> bool) {
    let mut stack = direct_children(root);
    stack.reverse();
    while let Some(node) = stack.pop() {
        if descend(node) {
            stack.extend(direct_children(node).into_iter().rev());
        }
    }
}

fn traversed_nodes<'tree>(root: Node<'tree>, stop_before: impl Fn(Node<'tree>) -> bool) -> Vec<Node<'tree>> {
    let mut nodes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        nodes.push(node);
        stack.extend(
            direct_children(node)
                .into_iter()
                .rev()
                .filter(|child| !stop_before(*child)),
        );
    }
    nodes
}

macro_rules! child_collector {
    ($name:ident, $method:ident) => {
        fn $name(node: Node<'_>) -> Vec<Node<'_>> {
            let mut cursor = node.walk();
            node.$method(&mut cursor).collect()
        }
    };
}

child_collector!(direct_children, children);
child_collector!(named_children, named_children);

trait KindSet {
    fn contains_kind(&self, value: &str) -> bool;
}

macro_rules! kind_set {
    ($kind:ty, $body:expr) => {
        impl KindSet for $kind {
            fn contains_kind(&self, value: &str) -> bool {
                $body(self, value)
            }
        }
    };
}

kind_set!(str, |set: &str, value| set.split('|').any(|kind| kind == value));
kind_set!([&str], |set: &[&str], value| set.contains(&value));

fn is_kind<K: KindSet + ?Sized>(value: &str, kinds: &K) -> bool {
    kinds.contains_kind(value)
}

/// Function-like nodes that become shared `FunctionRecord`s.
///
/// Rust closures and C++ lambdas are deliberately absent: the native adapters
/// cannot give those anonymous executable bodies stable cross-backend names.
/// They remain complexity boundaries through `function_boundary_types`.
const fn reported_function_types(language: Language) -> &'static [&'static str] {
    match language {
        Language::Bash | Language::C | Language::Cpp | Language::Python => &["function_definition"],
        Language::ObjectiveC => &["function_definition", "method_definition"],
        Language::Rust => &["function_item"],
        Language::Swift => &[
            "function_declaration",
            "init_declaration",
            "deinit_declaration",
            "subscript_declaration",
        ],
        Language::TypeScript => &[
            "function_declaration",
            "generator_function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
        ],
    }
}

/// Nodes whose decisions execute independently from an enclosing function.
///
/// This is intentionally broader than `reported_function_types`: anonymous
/// closures/lambdas are not reported, but their decisions must never inflate
/// the complexity of the function that constructs them.
const fn function_boundary_types(language: Language) -> &'static [&'static str] {
    match language {
        Language::Cpp => &["function_definition", "lambda_expression"],
        Language::ObjectiveC => &["function_definition", "method_definition", "block_literal"],
        Language::Python => &["function_definition", "lambda"],
        Language::Rust => &["function_item", "closure_expression"],
        _ => reported_function_types(language),
    }
}

fn decision_types(language: Language) -> &'static str {
    const TYPES: &str = "if_statement|elif_clause|for_statement|c_style_for_statement|while_statement|case_item|conditional_expression
if_statement|for_statement|while_statement|do_statement|case_statement|conditional_expression
if_statement|for_statement|while_statement|do_statement|case_statement|conditional_expression|catch_clause
if_statement|for_statement|while_statement|do_statement|case_statement|conditional_expression|catch_clause
if_statement|elif_clause|for_statement|while_statement|except_clause|case_clause|conditional_expression|list_comprehension|set_comprehension|dictionary_comprehension|generator_expression
if_expression|for_expression|while_expression|loop_expression|catch_clause|conditional_expression
if_statement|guard_statement|for_statement|while_statement|repeat_while_statement|switch_entry|catch_clause|ternary_expression
if_statement|for_statement|for_in_statement|while_statement|do_statement|switch_case|catch_clause|ternary_expression|conditional_type";
    TYPES.lines().nth(language_index(language)).unwrap_or_default()
}

//! Deterministic generic syntax analysis backed by pinned Tree-sitter grammars.
//!
//! This adapter intentionally owns the grammar dependencies directly. It never
//! downloads parsers at runtime, so the same binary works in offline CI and
//! produces deterministic syntax-level results.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use indexmap::IndexSet;
use reporigor_core::{
    AnalysisRequest, BackendCapabilities, BackendInfo, Capability, CoreError, Diagnostic, FileAnalysis,
    FunctionRecord, Language, MutationCandidate, Severity, SourceBudget, SourceFile, SourceLocation,
    SyntaxBackend, TokenRecord,
};
use tree_sitter::{Language as TreeSitterLanguage, Node, Parser};

const BACKEND_ID: &str = "tree-sitter-generic";

const MAX_TREE_SITTER_SOURCE_BYTES: usize = u32::MAX as usize;

const COMMENT_TYPES: &[&str] = &[
    "comment",
    "line_comment",
    "block_comment",
    "multiline_comment",
    "html_comment",
];
const STRING_TYPES: &[&str] = &[
    "string_literal",
    "raw_string_literal",
    "char_literal",
    "string",
    "raw_string",
    "ansi_c_string",
    "translated_string",
    "interpreted_string_literal",
    "line_string_literal",
    "multiline_string_literal",
    "template_string",
    "regex",
    "heredoc_body",
];
const NUMBER_TYPES: &[&str] = &[
    "number_literal",
    "integer_literal",
    "float_literal",
    "integer",
    "float",
    "real_literal",
    "number",
];
const IDENTIFIER_TYPES: &[&str] = &[
    "identifier",
    "field_identifier",
    "type_identifier",
    "simple_identifier",
    "word",
    "property_identifier",
    "function_identifier",
    "namespace_identifier",
];
const NAME_TYPES: &[&str] = &[
    "identifier",
    "field_identifier",
    "type_identifier",
    "simple_identifier",
    "word",
    "operator_name",
    "destructor_name",
    "qualified_identifier",
    "scoped_identifier",
    "function_identifier",
    "method_selector",
    "keyword_selector",
];
const SKIPPED_MUTATION_ANCESTORS: &[&str] = &[
    "comment",
    "line_comment",
    "block_comment",
    "documentation_comment",
    "string_literal",
    "raw_string_literal",
    "char_literal",
    "heredoc_body",
    "heredoc_redirect",
    "word",
    "string",
    "raw_string",
    "ansi_c_string",
    "translated_string",
    "interpreted_string_literal",
    "template_string",
    "template_literal",
    "template_substitution",
    "regex",
    "regex_pattern",
    "line_string_literal",
    "multi_line_string_literal",
    "multiline_string_literal",
    "interpolated_string_expression",
    "string_interpolation",
    "jsx_text",
];
const NON_EXPRESSION_MUTATION_ANCESTORS: &[&str] = &["literal_type"];
const BINARY_CONTEXTS: &[&str] = &[
    "binary_expression",
    "logical_expression",
    "boolean_expression",
    "test_expression",
    "arithmetic_expression",
    "compound_expression",
    "comparison_expression",
    "comparison_operator",
    "binary_operator",
    "boolean_operator",
    "additive_expression",
    "multiplicative_expression",
    "equality_expression",
    "conjunction_expression",
    "disjunction_expression",
    "infix_expression",
];
const OPERATOR_WRAPPERS: &[&str] = &["custom_operator", "test_operator"];
const BOOLEAN_LITERAL_TYPES: &[&str] = &["true", "false", "boolean", "boolean_literal"];
const PARSE_ROOT_TYPES: &[&str] = &["module", "program", "source_file", "translation_unit"];

/// Syntax-only fallback backend shared by every supported language.
#[derive(Debug, Clone, Copy, Default)]
pub struct TreeSitterBackend;

impl TreeSitterBackend {
    /// Creates a stateless generic syntax backend.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl SyntaxBackend for TreeSitterBackend {
    fn info(&self) -> BackendInfo {
        BackendInfo {
            id: BACKEND_ID.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            native: false,
            capabilities: BackendCapabilities::new([
                Capability::Syntax,
                Capability::Functions,
                Capability::Complexity,
                Capability::Tokens,
                Capability::Mutations,
                Capability::ParseValidation,
            ]),
        }
    }

    fn supports(&self, language: Language) -> bool {
        Language::ALL.contains(&language)
    }

    fn analyze_file(
        &self,
        root: &Path,
        source: &SourceFile,
        request: &AnalysisRequest,
    ) -> Result<FileAnalysis, CoreError> {
        if !self.supports(source.language) {
            return Err(CoreError::BackendUnavailable {
                backend: BACKEND_ID.to_string(),
                message: format!("{} is not supported", source.language),
            });
        }

        let path = source_path(root, source);
        let bytes = read_source(&path, request.max_source_bytes)?;
        let mut parser = Parser::new();
        let grammar = grammar_for(source);
        parser
            .set_language(&grammar)
            .map_err(|error| CoreError::Backend {
                backend: BACKEND_ID.to_string(),
                message: format!("failed to load {} grammar: {error}", source.language),
            })?;
        let tree = parser.parse(&bytes, None).ok_or_else(|| CoreError::Parse {
            path: path.display().to_string(),
            message: "Tree-sitter cancelled parsing before producing a tree".to_string(),
        })?;
        let root_node = tree.root_node();
        let issues = parse_issues(root_node, &bytes, source);

        if !issues.is_empty() && !request.allow_parse_errors {
            return Err(CoreError::Parse {
                path: path.display().to_string(),
                message: summarize_parse_issues(&issues),
            });
        }

        let diagnostics = issues.iter().map(ParseIssue::diagnostic).collect();
        let functions = extract_functions(root_node, &bytes, source);
        let tokens = normalized_tokens(root_node, &bytes);
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

fn source_path(root: &Path, source: &SourceFile) -> PathBuf {
    if source.path.is_absolute() {
        source.path.clone()
    } else {
        root.join(&source.path)
    }
}

fn read_source(path: &Path, max_source_bytes: usize) -> Result<Vec<u8>, CoreError> {
    let metadata = std::fs::metadata(path).map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(CoreError::Backend {
            backend: BACKEND_ID.to_string(),
            message: format!("source {} is not a regular file", path.display()),
        });
    }
    let mut budget = SourceBudget::new(max_source_bytes)?;
    budget.observe(path, metadata.len())?;
    let effective_limit = max_source_bytes.min(MAX_TREE_SITTER_SOURCE_BYTES);
    let limit = u64::try_from(effective_limit).unwrap_or(u64::MAX);
    if metadata.len() > limit {
        return Err(CoreError::Backend {
            backend: BACKEND_ID.to_string(),
            message: format!(
                "source {} exceeds Tree-sitter's {effective_limit}-byte parser limit",
                path.display()
            ),
        });
    }

    let initial_capacity = usize::try_from(metadata.len())
        .unwrap_or(effective_limit)
        .min(effective_limit);
    let file = File::open(path).map_err(|source| CoreError::Read {
        path: path.display().to_string(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(initial_capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| CoreError::Read {
            path: path.display().to_string(),
            source,
        })?;
    if bytes.len() > effective_limit {
        let actual = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if effective_limit == max_source_bytes {
            return Err(CoreError::source_too_large(path, actual, max_source_bytes));
        }
        return Err(CoreError::Backend {
            backend: BACKEND_ID.to_string(),
            message: format!(
                "source {} exceeds Tree-sitter's {effective_limit}-byte parser limit",
                path.display()
            ),
        });
    }
    if let Err(error) = std::str::from_utf8(&bytes) {
        return Err(CoreError::InvalidSourceEncoding {
            path: path.display().to_string(),
            valid_up_to: error.valid_up_to(),
        });
    }
    Ok(bytes)
}

fn grammar_for(source: &SourceFile) -> TreeSitterLanguage {
    match source.language {
        Language::Bash => tree_sitter_bash::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::ObjectiveC => tree_sitter_objc::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Swift => tree_sitter_swift::LANGUAGE.into(),
        Language::TypeScript if is_tsx(source) => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
    }
}

fn is_tsx(source: &SourceFile) -> bool {
    source
        .path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("tsx"))
        || Path::new(&source.relative)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("tsx"))
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
            .filter(|node| !node.has_error())
            .collect::<Vec<_>>();
        if let (Some(start), Some(end)) = (top_level.first(), top_level.last()) {
            functions.push(FunctionRecord {
                language: file.language,
                name: "<script>".to_string(),
                file: file.relative.clone(),
                start_line: one_based(start.start_position().row),
                end_line: one_based(end.end_position().row),
                complexity: complexity(root, source, file.language),
                coverage: None,
                crap: None,
            });
        }
    }

    functions.sort_by(|left, right| (left.start_line, &left.name).cmp(&(right.start_line, &right.name)));
    functions
}

fn function_record(node: Node<'_>, source: &[u8], file: &SourceFile) -> FunctionRecord {
    FunctionRecord {
        language: file.language,
        name: function_name(node, source, file.language),
        file: file.relative.clone(),
        start_line: one_based(node.start_position().row),
        end_line: one_based(node.end_position().row),
        complexity: complexity(node, source, file.language),
        coverage: None,
        crap: None,
    }
}

fn function_name(node: Node<'_>, source: &[u8], language: Language) -> String {
    let line = one_based(node.start_position().row);
    if node.kind() == "lambda_expression" {
        return format!("<lambda@{line}>");
    }
    if node.kind() == "closure_expression" {
        return format!("<closure@{line}>");
    }
    if language == Language::TypeScript && matches!(node.kind(), "arrow_function" | "function_expression") {
        if let Some(name) = node
            .parent()
            .and_then(|parent| parent.child_by_field_name("name"))
        {
            return node_text(name, source).trim().to_string();
        }
    }
    if language == Language::ObjectiveC && node.kind() == "method_definition" {
        if let Some(selector) = objective_c_selector(node, source) {
            return selector;
        }
    }
    if language == Language::Swift
        && matches!(
            node.kind(),
            "init_declaration" | "deinit_declaration" | "subscript_declaration"
        )
    {
        let base = match node.kind() {
            "init_declaration" => "init",
            "deinit_declaration" => "deinit",
            _ => "subscript",
        };
        return qualified_owner(node, source, language)
            .map_or_else(|| base.to_string(), |owner| format!("{owner}.{base}"));
    }

    let name = first_name_node(node)
        .map(|name| node_text(name, source).trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| format!("<function@{line}>"));
    let Some(owner) = qualified_owner(node, source, language) else {
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
        // A C-family outer declarator also contains parameter identifiers. Use
        // the function declarator's declarator field before falling back.
        for function_declarator in walk_nodes(declarator)
            .into_iter()
            .filter(|candidate| candidate.kind() == "function_declarator")
        {
            if let Some(inner) = function_declarator.child_by_field_name("declarator") {
                if let Some(name) = walk_nodes(inner)
                    .into_iter()
                    .rfind(|candidate| is_kind(candidate.kind(), NAME_TYPES))
                {
                    return Some(name);
                }
            }
        }
        if let Some(name) = walk_nodes(declarator)
            .into_iter()
            .find(|candidate| is_kind(candidate.kind(), NAME_TYPES))
        {
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

fn objective_c_selector(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = node_text(node, source);
    let signature = text.split('{').next().unwrap_or(text.as_ref());
    let bytes = signature.as_bytes();
    let mut parts = Vec::new();
    for (colon, byte) in bytes.iter().enumerate() {
        if *byte != b':' {
            continue;
        }
        let mut end = colon;
        while end > 0 && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
            start -= 1;
        }
        if start < end {
            parts.push(validated_utf8(&bytes[start..end]).to_string());
        }
    }
    if !parts.is_empty() {
        return Some(format!("{}:", parts.join(":")));
    }

    let marker = bytes.iter().position(|byte| matches!(*byte, b'-' | b'+'))?;
    let close_paren = bytes[marker..]
        .iter()
        .position(|byte| *byte == b')')
        .map(|offset| marker + offset + 1)?;
    let mut start = close_paren;
    while start < bytes.len() && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    let mut end = start;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    (start < end).then(|| validated_utf8(&bytes[start..end]).to_string())
}

fn qualified_owner(node: Node<'_>, source: &[u8], language: Language) -> Option<String> {
    let mut parent = node.parent();
    while let Some(candidate) = parent {
        let owner = match language {
            Language::Rust if candidate.kind() == "impl_item" => candidate
                .child_by_field_name("type")
                .or_else(|| candidate.child_by_field_name("trait")),
            Language::TypeScript
                if matches!(candidate.kind(), "class_declaration" | "interface_declaration") =>
            {
                candidate.child_by_field_name("name")
            }
            Language::Swift
                if matches!(
                    candidate.kind(),
                    "class_declaration"
                        | "struct_declaration"
                        | "enum_declaration"
                        | "actor_declaration"
                        | "extension_declaration"
                ) =>
            {
                candidate.child_by_field_name("name")
            }
            _ => None,
        };
        if let Some(owner) = owner {
            let value = node_text(owner, source).trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
        parent = candidate.parent();
    }
    None
}

fn complexity(node: Node<'_>, source: &[u8], language: Language) -> u32 {
    let mut total = 1_u32;
    let mut stack = direct_children(node);
    while let Some(candidate) = stack.pop() {
        if is_kind(candidate.kind(), function_boundary_types(language)) {
            continue;
        }
        total = total.saturating_add(u32::from(is_kind(candidate.kind(), decision_types(language))));
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

fn normalized_tokens(root: Node<'_>, source: &[u8]) -> Vec<TokenRecord> {
    let mut tokens = Vec::new();
    for node in walk_nodes(root)
        .into_iter()
        .filter(|node| node.child_count() == 0)
        .filter(|node| !in_error_region(*node))
    {
        if is_kind(node.kind(), COMMENT_TYPES) || has_ancestor_kind(node, COMMENT_TYPES) {
            continue;
        }
        let normalized = if is_kind(node.kind(), STRING_TYPES) || has_ancestor_kind(node, STRING_TYPES) {
            Cow::Borrowed("STR")
        } else if is_kind(node.kind(), NUMBER_TYPES) || has_ancestor_kind(node, NUMBER_TYPES) {
            Cow::Borrowed("NUM")
        } else if is_kind(node.kind(), IDENTIFIER_TYPES) {
            Cow::Borrowed("ID")
        } else {
            node_text(node, source)
        };
        if normalized.trim().is_empty() {
            continue;
        }
        tokens.push(TokenRecord {
            value: normalized.into_owned(),
            line: one_based(node.start_position().row),
            index: tokens.len(),
        });
    }
    tokens
}

fn enumerate_mutations(root: Node<'_>, source: &[u8], file: &SourceFile) -> Vec<MutationCandidate> {
    let mut candidates = walk_nodes(root)
        .into_iter()
        .filter(|node| node.child_count() == 0)
        .filter(|node| !in_mutation_error_region(*node))
        .filter_map(|node| {
            mutation_replacement(node, source, file.language).map(|replacement| (node, replacement))
        })
        .map(|(node, replacement)| MutationCandidate {
            id: 0,
            language: file.language,
            file: file.relative.clone(),
            line: one_based(node.start_position().row),
            column: unicode_scalar_column(source, node.start_byte(), node.start_position().column),
            original: node_text(node, source).into_owned(),
            replacement: replacement.to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
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
    matches!(
        value,
        "==" | "!="
            | ">"
            | "<"
            | ">="
            | "<="
            | "&&"
            | "||"
            | "and"
            | "or"
            | "-eq"
            | "-ne"
            | "-gt"
            | "-ge"
            | "-lt"
            | "-le"
            | "+"
            | "-"
            | "*"
            | "/"
    )
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
    let Some(parent) = node.parent() else {
        return false;
    };
    let context = if is_kind(parent.kind(), BINARY_CONTEXTS) {
        parent
    } else if is_kind(parent.kind(), OPERATOR_WRAPPERS) {
        let Some(grandparent) = parent.parent() else {
            return false;
        };
        if !is_kind(grandparent.kind(), BINARY_CONTEXTS) {
            return false;
        }
        grandparent
    } else {
        return false;
    };

    if context.has_error() || in_mutation_error_region(context) {
        return false;
    }

    let mut cursor = context.walk();
    let mut before = false;
    let mut after = false;
    for child in context.children(&mut cursor).filter(|child| !child.is_extra()) {
        before |= !child.is_missing() && child.end_byte() <= node.start_byte();
        after |= !child.is_missing() && child.start_byte() >= node.end_byte();
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

const fn common_replacement(value: &str) -> Option<&'static str> {
    match value.as_bytes() {
        b"==" => Some("!="),
        b"!=" => Some("=="),
        b">" => Some("<="),
        b"<" => Some(">="),
        b">=" => Some("<"),
        b"<=" => Some(">"),
        b"&&" => Some("||"),
        b"||" => Some("&&"),
        b"true" => Some("false"),
        b"false" => Some("true"),
        _ => None,
    }
}

const fn language_replacement(language: Language, value: &str) -> Option<&'static str> {
    match (language, value.as_bytes()) {
        (Language::Python, b"True") => Some("False"),
        (Language::Python, b"False") => Some("True"),
        (Language::Python, b"and") => Some("or"),
        (Language::Python, b"or") => Some("and"),
        (Language::ObjectiveC, b"YES") => Some("NO"),
        (Language::ObjectiveC, b"NO") => Some("YES"),
        (Language::Bash, b"-eq") => Some("-ne"),
        (Language::Bash, b"-ne") => Some("-eq"),
        (Language::Bash, b"-gt") => Some("-le"),
        (Language::Bash, b"-ge") => Some("-lt"),
        (Language::Bash, b"-lt") => Some("-ge"),
        (Language::Bash, b"-le") => Some("-gt"),
        _ => None,
    }
}

fn has_ancestor_kind(node: Node<'_>, kinds: &[&str]) -> bool {
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
    let mut nodes = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        nodes.push(node);
        stack.extend(direct_children(node).into_iter().rev());
    }
    nodes
}

fn direct_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn is_kind(value: &str, kinds: &[&str]) -> bool {
    kinds.contains(&value)
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
        Language::Rust => &["function_item", "closure_expression"],
        _ => reported_function_types(language),
    }
}

const fn decision_types(language: Language) -> &'static [&'static str] {
    match language {
        Language::TypeScript => &[
            "if_statement",
            "for_statement",
            "for_in_statement",
            "while_statement",
            "do_statement",
            "switch_case",
            "catch_clause",
            "ternary_expression",
            "conditional_type",
        ],
        Language::Python => &[
            "if_statement",
            "elif_clause",
            "for_statement",
            "while_statement",
            "except_clause",
            "case_clause",
            "conditional_expression",
            "list_comprehension",
            "set_comprehension",
            "dictionary_comprehension",
            "generator_expression",
        ],
        Language::Rust => &[
            "if_expression",
            "for_expression",
            "while_expression",
            "loop_expression",
            "match_arm",
            "catch_clause",
            "conditional_expression",
        ],
        Language::Swift => &[
            "if_statement",
            "guard_statement",
            "for_statement",
            "while_statement",
            "repeat_while_statement",
            "switch_entry",
            "catch_clause",
            "ternary_expression",
        ],
        Language::ObjectiveC | Language::Cpp => &[
            "if_statement",
            "for_statement",
            "while_statement",
            "do_statement",
            "case_statement",
            "conditional_expression",
            "catch_clause",
        ],
        Language::Bash => &[
            "if_statement",
            "elif_clause",
            "for_statement",
            "c_style_for_statement",
            "while_statement",
            "case_item",
            "conditional_expression",
        ],
        Language::C => &[
            "if_statement",
            "for_statement",
            "while_statement",
            "do_statement",
            "case_statement",
            "conditional_expression",
        ],
    }
}

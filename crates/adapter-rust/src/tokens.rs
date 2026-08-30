use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use reporigor_core::TokenRecord;
use rustc_lexer::TokenKind;

use crate::scope::CfgContext;
use crate::syntax::inactive_file_ranges;

/// Normalize one function/statement fragment while preserving non-local names.
///
/// Only bindings owned by the function become `LOCAL`, all literals become
/// `LITERAL`, and referenced types/functions/modules retain their spelling.
pub(crate) fn normalize_fragment(
    source: &str,
    range: Range<usize>,
    locals: &BTreeSet<String>,
) -> Vec<String> {
    normalize_fragment_excluding(source, range, locals, &[])
}

pub(crate) fn normalize_fragment_excluding(
    source: &str,
    range: Range<usize>,
    locals: &BTreeSet<String>,
    excluded_ranges: &[Range<usize>],
) -> Vec<String> {
    let Some(fragment) = source.get(range.clone()) else {
        return Vec::new();
    };
    let mut offset = range.start;
    let mut output = Vec::new();
    for raw in rustc_lexer::tokenize(fragment) {
        if let Some(value) = fragment_token(source, &raw, &mut offset, locals, excluded_ranges) {
            output.push(value);
        }
    }
    output
}

fn fragment_token(
    source: &str,
    raw: &rustc_lexer::Token,
    offset: &mut usize,
    locals: &BTreeSet<String>,
    excluded_ranges: &[Range<usize>],
) -> Option<String> {
    let (text, token_range) = consume_token(source, raw, offset)?;
    (!range_excluded(excluded_ranges, &token_range))
        .then(|| normalized_fragment_token(raw.kind, text, locals))
        .flatten()
        .filter(|value| !value.is_empty())
}

fn consume_token<'a>(
    source: &'a str,
    raw: &rustc_lexer::Token,
    offset: &mut usize,
) -> Option<(&'a str, Range<usize>)> {
    let end = offset.saturating_add(raw.len);
    let range = *offset..end;
    let text = source.get(range.clone())?;
    *offset = end;
    Some((text, range))
}

fn normalized_fragment_token(kind: TokenKind, text: &str, locals: &BTreeSet<String>) -> Option<String> {
    match kind {
        TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { .. } => None,
        TokenKind::Ident | TokenKind::RawIdent => Some(normalized_fragment_identifier(text, locals)),
        TokenKind::Literal { .. } => Some("LITERAL".to_string()),
        _ => Some(text.to_string()),
    }
}

fn normalized_fragment_identifier(text: &str, locals: &BTreeSet<String>) -> String {
    let identifier = text.trim_start_matches("r#");
    if matches!(identifier, "true" | "false") {
        "LITERAL".to_string()
    } else if locals.contains(identifier) {
        "LOCAL".to_string()
    } else {
        identifier.to_string()
    }
}

pub(crate) fn range_excluded(excluded_ranges: &[Range<usize>], range: &Range<usize>) -> bool {
    excluded_ranges
        .iter()
        .any(|excluded| excluded.start <= range.start && range.end <= excluded.end)
}

pub(crate) fn identifier_counts(source: &str, excluded_ranges: &[Range<usize>]) -> BTreeMap<String, u32> {
    let mut offset = rustc_lexer::strip_shebang(source).unwrap_or(0);
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for raw in rustc_lexer::tokenize(&source[offset..]) {
        let end = offset.saturating_add(raw.len);
        let Some(text) = source.get(offset..end) else {
            break;
        };
        let range = offset..end;
        offset = end;
        if range_excluded(excluded_ranges, &range)
            || !matches!(raw.kind, TokenKind::Ident | TokenKind::RawIdent)
        {
            continue;
        }
        let identifier = text.trim_start_matches("r#");
        let count = counts.entry(identifier.to_string()).or_default();
        *count = count.saturating_add(1);
    }
    counts
}

pub(crate) fn normalize_source(source: &str, excluded_ranges: &[Range<usize>]) -> Vec<TokenRecord> {
    let shebang = rustc_lexer::strip_shebang(source).unwrap_or(0);
    let mut offset = shebang;
    let mut line = 1_usize.saturating_add(source[..shebang].bytes().filter(|byte| *byte == b'\n').count());
    let mut output = Vec::new();
    for raw in rustc_lexer::tokenize(&source[shebang..]) {
        let Some(token) = source_token(source, &raw, &mut offset, &mut line, excluded_ranges) else {
            continue;
        };
        if let Some(value) = token.value {
            output.push(TokenRecord {
                value,
                line: token.line,
                index: output.len(),
            });
        }
    }
    output
}

struct SourceToken {
    value: Option<String>,
    line: u32,
}

fn source_token(
    source: &str,
    raw: &rustc_lexer::Token,
    offset: &mut usize,
    line: &mut usize,
    excluded_ranges: &[Range<usize>],
) -> Option<SourceToken> {
    let (text, token_range) = consume_token(source, raw, offset)?;
    let start_line = *line;
    *line = line.saturating_add(text.bytes().filter(|byte| *byte == b'\n').count());
    (!range_excluded(excluded_ranges, &token_range)).then(|| SourceToken {
        value: normalized_source_token(raw.kind, text).filter(|value| !value.trim().is_empty()),
        line: u32::try_from(start_line).unwrap_or(u32::MAX),
    })
}

fn normalized_source_token(kind: TokenKind, text: &str) -> Option<String> {
    if ignored_source_token(kind) {
        return None;
    }
    Some(normalized_nontrivia_source_token(kind, text))
}

fn ignored_source_token(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { .. }
    )
}

fn normalized_nontrivia_source_token(kind: TokenKind, text: &str) -> String {
    match kind {
        TokenKind::Ident if matches!(text, "true" | "false") => "LITERAL".into(),
        TokenKind::Literal { .. } => "LITERAL".into(),
        TokenKind::Lifetime { .. } => "LIFETIME".into(),
        _ => text.to_string(),
    }
}

pub(crate) fn normalize(syntax: &syn::File, source: &str, cfg: &CfgContext) -> Vec<TokenRecord> {
    normalize_source(source, &inactive_file_ranges(syntax, cfg))
}

#[cfg(test)]
mod tests {
    use crate::scope::CfgContext;

    use super::*;

    fn normalized(source: &str) -> Vec<TokenRecord> {
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        normalize(&syntax, source, &CfgContext::synthetic(false))
    }

    #[test]
    fn exact_tokens_preserve_identifiers_normalize_literals_and_discard_comments() {
        let source = "fn sample(value: i32) { let other = value + 42; let enabled = true; /* == false */ }\n";
        let tokens = normalized(source);
        let values: Vec<_> = tokens.iter().map(|token| token.value.as_str()).collect();
        assert!(values.starts_with(&["fn", "sample", "(", "value", ":", "i32"]));
        assert!(values.contains(&"LITERAL"));
        assert!(!values.contains(&"true"));
        assert!(!values.contains(&"false"));
    }

    #[test]
    fn inactive_cfg_ranges_are_not_tokenized() {
        let source = "fn active() {}\n#[cfg(any())]\nfn hidden() { let special = 99; }\n";
        let tokens = normalized(source);
        assert_eq!(tokens.iter().filter(|token| token.value == "fn").count(), 1);
        assert!(!tokens.iter().any(|token| token.value == "NUM"));
    }

    #[test]
    fn function_normalization_changes_only_locals_and_literals() {
        let source = "{ let local = service::load(42); local + external }";
        let normalized = normalize_fragment(source, 0..source.len(), &BTreeSet::from(["local".to_string()]));
        assert!(
            normalized
                .iter()
                .filter(|token| token.as_str() == "LOCAL")
                .count()
                >= 2
        );
        assert!(normalized.iter().any(|token| token == "LITERAL"));
        assert!(normalized.iter().any(|token| token == "service"));
        assert!(normalized.iter().any(|token| token == "load"));
        assert!(normalized.iter().any(|token| token == "external"));
    }
}

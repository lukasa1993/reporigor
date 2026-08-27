use std::ops::Range;

use reporigor_core::TokenRecord;
use rustc_lexer::{LiteralKind, TokenKind};

use crate::scope::CfgContext;
use crate::syntax::inactive_file_ranges;

fn is_keyword(text: &str) -> bool {
    matches!(
        text,
        "_" | "as"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "dyn"
            | "abstract"
            | "become"
            | "box"
            | "do"
            | "final"
            | "gen"
            | "macro"
            | "macro_rules"
            | "override"
            | "priv"
            | "raw"
            | "safe"
            | "typeof"
            | "union"
            | "unsized"
            | "virtual"
            | "yield"
            | "try"
    )
}

fn normalize_literal(kind: LiteralKind) -> &'static str {
    match kind {
        LiteralKind::Int { .. } | LiteralKind::Float { .. } => "NUM",
        LiteralKind::Char { .. }
        | LiteralKind::Byte { .. }
        | LiteralKind::Str { .. }
        | LiteralKind::ByteStr { .. }
        | LiteralKind::RawStr { .. }
        | LiteralKind::RawByteStr { .. } => "STR",
    }
}

pub(crate) fn normalize_source(source: &str, excluded_ranges: &[Range<usize>]) -> Vec<TokenRecord> {
    let shebang = rustc_lexer::strip_shebang(source).unwrap_or(0);
    let mut offset = shebang;
    let mut line = 1_usize.saturating_add(source[..shebang].bytes().filter(|byte| *byte == b'\n').count());
    let mut output = Vec::new();
    for raw in rustc_lexer::tokenize(&source[shebang..]) {
        let end = offset.saturating_add(raw.len);
        let Some(text) = source.get(offset..end) else {
            break;
        };
        let start_line = line;
        line = line.saturating_add(text.bytes().filter(|byte| *byte == b'\n').count());
        let token_range = offset..end;
        offset = end;
        if excluded_ranges
            .iter()
            .any(|excluded| excluded.start <= token_range.start && token_range.end <= excluded.end)
        {
            continue;
        }
        let normalized = match raw.kind {
            TokenKind::Whitespace | TokenKind::LineComment | TokenKind::BlockComment { .. } => None,
            TokenKind::Ident => Some(if is_keyword(text) {
                text.to_string()
            } else {
                "ID".into()
            }),
            TokenKind::RawIdent => Some("ID".into()),
            TokenKind::Literal { kind, .. } => Some(normalize_literal(kind).into()),
            TokenKind::Lifetime { .. } => Some("LIFETIME".into()),
            _ => Some(text.to_string()),
        };
        if let Some(value) = normalized.filter(|value| !value.trim().is_empty()) {
            output.push(TokenRecord {
                value,
                line: u32::try_from(start_line).unwrap_or(u32::MAX),
                index: output.len(),
            });
        }
    }
    output
}

pub(crate) fn normalize(syntax: &syn::File, source: &str, cfg: &CfgContext) -> Vec<TokenRecord> {
    normalize_source(source, &inactive_file_ranges(syntax, cfg))
}

#[cfg(test)]
mod tests {
    use crate::scope::CfgContext;

    use super::*;

    #[test]
    fn normalizes_identifiers_literals_and_discards_comments() {
        let source = "fn sample(value: i32) { let other = value + 42; /* == false */ }\n";
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let tokens = normalize(&syntax, source, &CfgContext::synthetic(false));
        let values: Vec<_> = tokens.iter().map(|token| token.value.as_str()).collect();
        assert!(values.starts_with(&["fn", "ID", "(", "ID", ":", "ID"]));
        assert!(values.contains(&"NUM"));
        assert!(!values.contains(&"false"));
    }

    #[test]
    fn inactive_cfg_ranges_are_not_tokenized() {
        let source = "fn active() {}\n#[cfg(any())]\nfn hidden() { let special = 99; }\n";
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let tokens = normalize(&syntax, source, &CfgContext::synthetic(false));
        assert_eq!(tokens.iter().filter(|token| token.value == "fn").count(), 1);
        assert!(!tokens.iter().any(|token| token.value == "NUM"));
    }
}

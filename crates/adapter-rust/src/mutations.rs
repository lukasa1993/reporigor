use std::ops::Range;

use proc_macro2::Span;
use reporigor_core::{Language, MutationCandidate};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{BinOp, ExprBinary, ExprLit, Lit};

use crate::scope::CfgContext;
use crate::syntax::inactive_file_ranges;

fn replacement_for_binop(op: &BinOp) -> Option<&'static str> {
    match op {
        BinOp::Add(_) => Some("-"),
        BinOp::Sub(_) => Some("+"),
        BinOp::Mul(_) => Some("/"),
        BinOp::Div(_) | BinOp::Rem(_) => Some("*"),
        BinOp::And(_) => Some("||"),
        BinOp::Or(_) => Some("&&"),
        BinOp::Eq(_) => Some("!="),
        BinOp::Ne(_) => Some("=="),
        BinOp::Lt(_) => Some(">="),
        BinOp::Le(_) => Some(">"),
        BinOp::Gt(_) => Some("<="),
        BinOp::Ge(_) => Some("<"),
        _ => None,
    }
}

struct MutationVisitor<'a> {
    source: &'a str,
    file: &'a str,
    excluded_ranges: &'a [Range<usize>],
    candidates: Vec<MutationCandidate>,
}

impl MutationVisitor<'_> {
    fn add_span(&mut self, span: Span, replacement: &str) {
        let range = span.byte_range();
        if self
            .excluded_ranges
            .iter()
            .any(|excluded| excluded.start <= range.start && range.end <= excluded.end)
        {
            return;
        }
        let Some(original) = self.source.get(range.clone()) else {
            return;
        };
        if original == replacement || original.is_empty() {
            return;
        }
        let Some((line, column)) = crate::scalar_position(self.source, range.start) else {
            return;
        };
        self.candidates.push(MutationCandidate {
            id: 0,
            language: Language::Rust,
            file: self.file.to_string(),
            line,
            column,
            original: original.to_string(),
            replacement: replacement.to_string(),
            start_byte: range.start,
            end_byte: range.end,
        });
    }
}

impl<'ast> Visit<'ast> for MutationVisitor<'_> {
    fn visit_expr_binary(&mut self, node: &'ast ExprBinary) {
        if let Some(replacement) = replacement_for_binop(&node.op) {
            self.add_span(node.op.span(), replacement);
        }
        visit::visit_expr_binary(self, node);
    }

    fn visit_expr_lit(&mut self, node: &'ast ExprLit) {
        if let Lit::Bool(value) = &node.lit {
            self.add_span(value.span(), if value.value { "false" } else { "true" });
        }
        visit::visit_expr_lit(self, node);
    }
}

pub(crate) fn enumerate(
    syntax: &syn::File,
    source: &str,
    file: &str,
    cfg: &CfgContext,
) -> Vec<MutationCandidate> {
    let excluded_ranges = inactive_file_ranges(syntax, cfg);
    let mut visitor = MutationVisitor {
        source,
        file,
        excluded_ranges: &excluded_ranges,
        candidates: Vec::new(),
    };
    visitor.visit_file(syntax);
    visitor
        .candidates
        .sort_by_key(|item| (item.start_byte, item.end_byte, item.replacement.clone()));
    visitor.candidates.dedup_by(|left, right| {
        left.start_byte == right.start_byte
            && left.end_byte == right.end_byte
            && left.replacement == right.replacement
    });
    visitor.candidates
}

#[cfg(test)]
mod tests {
    use crate::scope::CfgContext;

    use super::*;

    #[test]
    fn candidates_come_from_expressions_not_comments_or_strings() {
        let source = r#"
fn choose(a: bool, b: bool) -> bool {
    let text = "a == b and true"; // false && true
    (a == b) && true
}
"#;
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let candidates = enumerate(&syntax, source, "src/lib.rs", &CfgContext::synthetic(false));
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.original == "==")
                .count(),
            1
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.original == "true")
                .count(),
            1
        );
        assert!(candidates
            .iter()
            .any(|candidate| { candidate.original == "&&" && candidate.replacement == "||" }));
    }

    #[test]
    fn inactive_cfg_candidates_are_excluded() {
        let source = "fn active() -> bool { true }\n#[cfg(any())]\nfn hidden() -> bool { false }\n";
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let candidates = enumerate(&syntax, source, "src/lib.rs", &CfgContext::synthetic(false));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].original, "true");
    }

    #[test]
    fn candidate_columns_count_unicode_scalars_instead_of_utf8_bytes() {
        let source = "fn compare(a: i32) -> bool { let emoji = \"😀\"; a == 1 }\n";
        let syntax = syn::parse_file(source).unwrap_or_else(|error| panic!("parse: {error}"));
        let candidates = enumerate(&syntax, source, "src/lib.rs", &CfgContext::synthetic(false));
        let Some(candidate) = candidates.iter().find(|candidate| candidate.original == "==") else {
            panic!("equality mutation must be discovered");
        };
        let Some(start_byte) = source.find("==") else {
            panic!("fixture equality operator must exist");
        };
        let expected_column = u32::try_from(source[..start_byte].chars().count() + 1).unwrap_or(u32::MAX);

        assert_eq!(candidate.start_byte, start_byte);
        assert_eq!(candidate.column, expected_column);
        assert_ne!(
            candidate.column,
            u32::try_from(start_byte + 1).unwrap_or(u32::MAX)
        );
    }
}

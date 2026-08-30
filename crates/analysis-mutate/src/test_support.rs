use reporigor_core::{Language, MutationCandidate};

#[derive(Clone, Copy)]
pub(crate) struct CandidateText {
    original: &'static str,
    replacement: &'static str,
    start_byte: usize,
}

pub(crate) const BOOLEAN_TEXT: CandidateText = CandidateText {
    original: "true",
    replacement: "false",
    start_byte: 0,
};

pub(crate) const COMPARISON_TEXT: CandidateText = CandidateText {
    original: "==",
    replacement: "!=",
    start_byte: 0,
};

pub(crate) fn candidate(
    id: u64,
    file: &str,
    operator: &str,
    fingerprint: &str,
    text: CandidateText,
) -> MutationCandidate {
    let start_byte = text
        .start_byte
        .saturating_add(usize::try_from(id.saturating_sub(1)).unwrap_or(0));
    MutationCandidate {
        id,
        language: Language::Rust,
        file: file.into(),
        stable_symbol: String::new(),
        operator: operator.into(),
        fingerprint: fingerprint.into(),
        line: 1,
        column: 1,
        original: text.original.into(),
        replacement: text.replacement.into(),
        start_byte,
        end_byte: start_byte.saturating_add(text.original.len()),
    }
}

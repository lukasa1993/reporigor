pub(crate) fn indexed_name(names: &'static str, index: usize, fallback: &'static str) -> &'static str {
    names.split('|').nth(index).unwrap_or(fallback)
}

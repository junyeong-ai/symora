/// A query is a path-like pattern when it carries `/`, `*`, or `[` rather
/// than a plain identifier. The callers route these forms specially — a `*`
/// glob resolves against the index, other path-like forms fall through to the
/// LSP workspace-symbol lookup (which the index then supplements).
pub fn looks_like_symbol_path(query: &str) -> bool {
    query.contains('/') || query.contains('*') || query.contains('[')
}

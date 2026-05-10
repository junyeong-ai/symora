//! Heuristic signature extraction — pulls the declaration line out of a
//! symbol body so we can show "fn process(...)" without the body weight.

/// Extract a function/method/type signature from raw body source.
/// Falls back to the first non-empty line when no language keyword
/// matches.
pub fn extract_signature(body: Option<&str>) -> Option<String> {
    let body = body?;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }

        if trimmed.contains("fn ")
            || trimmed.contains("func ")
            || trimmed.contains("def ")
            || trimmed.contains("function ")
            || trimmed.contains("async ")
            || trimmed.contains("pub ")
            || trimmed.contains("class ")
            || trimmed.contains("struct ")
            || trimmed.contains("enum ")
            || trimmed.contains("interface ")
            || trimmed.contains("trait ")
            || trimmed.contains("impl ")
            || trimmed.contains("type ")
            || trimmed.contains("const ")
        {
            let sig = if let Some(brace_pos) = trimmed.find('{') {
                trimmed[..brace_pos].trim()
            } else if let Some(arrow_pos) = trimmed.find("=>") {
                trimmed[..arrow_pos].trim()
            } else if let Some(stripped) = trimmed.strip_suffix(':') {
                stripped
            } else {
                trimmed
            };

            return Some(sig.to_string());
        }
    }

    body.lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
}

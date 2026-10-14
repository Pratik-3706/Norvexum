// ===========================================================================
// Util — Small shared helpers (UTF-8 safe truncation, etc.)
//
// Slicing a `str` with a raw byte index panics when the index falls inside a
// multi-byte character (emoji, CJK, accented text). Every truncation in the
// codebase should go through these helpers instead of `&s[..n]`.
// ===========================================================================

/// Largest index `<= idx` that lies on a char boundary of `s`.
pub fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest index `>= idx` that lies on a char boundary of `s`.
pub fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// First `max_chars` characters of `s`, with `…` appended when something was cut.
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
        None => s.to_string(),
    }
}

/// Shrink `s` to roughly `max_bytes` by keeping its head and tail and replacing
/// the middle with a marker. Command output keeps its errors at the end, so
/// dropping the middle loses far less than dropping the tail.
pub fn truncate_middle(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let head_len = floor_char_boundary(s, max_bytes * 2 / 5);
    let tail_start = ceil_char_boundary(s, s.len() - (max_bytes - head_len));
    let omitted = tail_start - head_len;
    format!(
        "{}\n\n[... {} bytes omitted to save context ...]\n\n{}",
        &s[..head_len],
        omitted,
        &s[tail_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_never_split_characters() {
        let s = "ab✅cd";
        // '✅' occupies bytes 2..5
        assert_eq!(floor_char_boundary(s, 3), 2);
        assert_eq!(ceil_char_boundary(s, 3), 5);
        assert_eq!(floor_char_boundary(s, 99), s.len());
    }

    #[test]
    fn truncate_chars_counts_characters() {
        assert_eq!(truncate_chars("日本語テキスト", 3), "日本語…");
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let s = format!("HEAD{}TAIL", "é".repeat(5000));
        let out = truncate_middle(&s, 200);
        assert!(out.starts_with("HEAD"));
        assert!(out.ends_with("TAIL"));
        assert!(out.contains("bytes omitted"));
        assert!(out.len() < 400);
    }
}

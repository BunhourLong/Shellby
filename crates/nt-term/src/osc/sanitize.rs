//! Strip control and bidi characters from untrusted text and cap its length (pure).

/// Drops C0, DEL, C1, U+200E/F, U+202A-202E, U+2066-2069, U+FEFF; maps `\t` to a space;
/// trims; keeps ≤ `max_chars` chars including a trailing `…` when truncated.
pub(crate) fn sanitize(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let filtered: String = input
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !is_stripped(*c))
        .collect();
    let trimmed = filtered.trim();
    if trimmed.chars().nth(max_chars).is_none() {
        return trimmed.to_owned();
    }
    let cut = trimmed.chars().take(max_chars - 1).collect::<String>();
    let cut = cut.trim();
    let mut out = String::with_capacity(cut.len() + '…'.len_utf8());
    out.push_str(cut);
    out.push('…');
    out
}

/// Whether `c` is a C0 (bar `\t`, already mapped), DEL, C1, bidi or BOM character.
fn is_stripped(c: char) -> bool {
    matches!(
        c,
        '\u{00}'..='\u{1f}'
            | '\u{7f}'
            | '\u{80}'..='\u{9f}'
            | '\u{200e}' | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
    )
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn tab_maps_to_space() {
        assert_eq!(sanitize("a\tb", 10), "a b");
    }

    #[test]
    fn strips_c0_del_c1() {
        assert_eq!(sanitize("a\u{1}b\u{7f}c\u{9b}d", 10), "abcd");
    }

    #[test]
    fn strips_bidi_and_bom() {
        let dirty = "a\u{202e}b\u{200e}c\u{200f}d\u{2066}e\u{2069}f\u{feff}";
        assert_eq!(sanitize(dirty, 10), "abcdef");
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(sanitize("  hi \t ", 10), "hi");
    }

    #[test]
    fn exact_cap_kept() {
        assert_eq!(sanitize("abcde", 5), "abcde");
    }

    #[test]
    fn cap_plus_one_truncates() {
        assert_eq!(sanitize("abcdef", 5), "abcd…");
    }

    #[test]
    fn ellipsis_counts_toward_cap() {
        let out = sanitize("abcdef", 3);
        assert_eq!(out.chars().count(), 3);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn multi_byte_char_at_cap_boundary() {
        assert_eq!(sanitize("ééééé", 5), "ééééé");
        assert_eq!(sanitize("aéééé", 3), "aé…");
    }

    #[test]
    fn re_trims_before_ellipsis() {
        assert_eq!(sanitize("a  b", 2), "a…");
    }

    #[test]
    fn zero_cap_is_empty() {
        assert_eq!(sanitize("hello", 0), "");
        assert_eq!(sanitize("", 0), "");
    }
}

//! Text helpers the client and the renderers share.

/// `s` cut to `max` characters, the cut marked with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!(
        "{}\u{2026}",
        s.chars().take(max.saturating_sub(1)).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use crate::text::truncate;

    #[test]
    fn a_long_string_is_cut_to_the_limit_with_an_ellipsis() {
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate("\u{e9}\u{e9}\u{e9}", 2), "\u{e9}\u{2026}");
    }
}

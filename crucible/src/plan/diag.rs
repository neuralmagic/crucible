//! Did-you-mean helpers for workflow compile errors.

/// Best fuzzy match for an unknown identifier: minimum Levenshtein distance, accepted
/// when <= 2 and < the candidate's length; ties break lexicographically.
pub(crate) fn suggest<'a>(
    unknown: &str,
    known: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    for candidate in known {
        let distance = levenshtein(unknown, candidate);
        if distance > 2 || distance >= candidate.chars().count() {
            continue;
        }
        best = match best {
            Some(held) if held <= (distance, candidate) => Some(held),
            _ => Some((distance, candidate)),
        };
    }
    best.map(|(_, candidate)| candidate)
}

/// The `; did you mean "x"?` suffix an error message appends, empty when there is no
/// suggestion. Errors carry the raw `Option` and render it here, so the suggestion stays
/// a field rather than being baked into a pre-formatted string.
pub(crate) fn hint(suggestion: Option<&str>) -> String {
    match suggestion {
        Some(candidate) => format!("; did you mean {candidate:?}?"),
        None => String::new(),
    }
}

/// Two-row Levenshtein over chars.
fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b_chars.len()).collect();
    let mut current = vec![0usize; b_chars.len() + 1];
    for (i, a_char) in a.chars().enumerate() {
        current[0] = i + 1;
        for (j, b_char) in b_chars.iter().enumerate() {
            let substitution = previous[j] + usize::from(a_char != *b_char);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_near_matches_are_suggested() {
        assert_eq!(
            suggest("depends_on", ["depends_on", "name"]),
            Some("depends_on")
        );
        assert_eq!(
            suggest("depend_on", ["depends_on", "name"]),
            Some("depends_on")
        );
        assert_eq!(suggest("isolatd", ["isolated", "join"]), Some("isolated"));
        assert_eq!(suggest("agnt", ["agent", "apply", "grade"]), Some("agent"));
    }

    #[test]
    fn far_matches_and_empty_candidate_sets_yield_nothing() {
        assert_eq!(suggest("emits", ["name", "session", "depends_on"]), None);
        assert_eq!(suggest("x", []), None);
        // Distance must be under the candidate's length: "x" -> "ab" is a rewrite, not a typo.
        assert_eq!(suggest("x", ["ab"]), None);
    }

    #[test]
    fn ties_break_lexicographically_on_equal_distance() {
        assert_eq!(suggest("dat", ["cat", "bat"]), Some("bat"));
    }

    #[test]
    fn hint_renders_only_when_present() {
        assert_eq!(hint(Some("depends_on")), "; did you mean \"depends_on\"?");
        assert_eq!(hint(None), "");
    }
}

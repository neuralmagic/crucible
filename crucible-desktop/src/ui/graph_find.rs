//! Finding text in the Graph pane's diagram: which labels hold the query, and where they are.

use std::collections::HashMap;
use std::ops::Range;

use rune_kit::search::Fold;

/// A label holding the query.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) struct Found {
    /// The label's element key.
    pub key: String,
    /// Where the query is in the label's text.
    pub ranges: Vec<Range<usize>>,
    /// The label's middle, in the drawing's points from its top left.
    pub middle: [f32; 2],
}

/// The `labels` (element key and text) holding `query`, in drawing order, each at its box in
/// `places` (`[x, y, w, h]` by element key).
pub(in crate::ui) fn found<'a>(
    labels: impl Iterator<Item = (&'a str, &'a str)>,
    places: impl Iterator<Item = (&'a str, [f32; 4])>,
    query: &str,
) -> Vec<Found> {
    let mut boxes: HashMap<&str, [f32; 4]> = HashMap::new();
    for (key, place) in places {
        boxes.entry(key).or_insert(place);
    }
    labels
        .filter_map(|(key, text)| {
            let ranges = Fold::default().find(text, query);
            let [x, y, w, h] = *boxes.get(key)?;
            (!ranges.is_empty()).then(|| Found {
                key: key.to_string(),
                ranges,
                middle: [x + w / 2.0, y + h / 2.0],
            })
        })
        .collect()
}

/// The match Enter moves to after `last`, out of `count`.
pub(in crate::ui) fn next(last: Option<usize>, count: usize) -> Option<usize> {
    (count > 0).then(|| last.map_or(0, |at| (at + 1) % count))
}

#[cfg(test)]
mod tests {
    use crate::ui::graph_find::{Found, found, next};

    const LABELS: [(&str, &str); 4] = [
        ("title", "Release audit"),
        ("n:draft", "Draft notes"),
        ("n:audit", "audit-headings"),
        ("e:0", "then AUDIT"),
    ];
    const PLACES: [(&str, [f32; 4]); 3] = [
        ("n:draft", [10.0, 0.0, 80.0, 20.0]),
        ("n:audit", [10.0, 60.0, 100.0, 20.0]),
        ("e:0", [40.0, 30.0, 60.0, 10.0]),
    ];

    fn find(query: &str) -> Vec<Found> {
        found(LABELS.into_iter(), PLACES.into_iter(), query)
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn labels_holding_the_query_are_found_in_drawing_order_whatever_their_case() {
        assert_eq!(
            find("audit"),
            [
                Found {
                    key: "n:audit".into(),
                    ranges: vec![0..5],
                    middle: [60.0, 70.0],
                },
                Found {
                    key: "e:0".into(),
                    ranges: vec![5..10],
                    middle: [70.0, 35.0],
                },
            ],
            "the title has no place, so it is not a match to move to"
        );
        assert_eq!(find("T N")[0].key, "n:draft");
        assert_eq!(find("t")[0].ranges, vec![4..5, 8..9]);
    }

    #[test]
    fn an_empty_query_or_one_no_label_holds_finds_nothing() {
        assert!(find("").is_empty());
        assert!(find("deploy").is_empty());
    }

    #[test]
    fn enter_walks_the_matches_and_wraps() {
        assert_eq!(next(None, 3), Some(0));
        assert_eq!(next(Some(0), 3), Some(1));
        assert_eq!(next(Some(2), 3), Some(0));
        assert_eq!(next(Some(7), 3), Some(2), "fewer matches than before");
        assert_eq!(next(None, 0), None);
        assert_eq!(next(Some(1), 0), None);
    }
}

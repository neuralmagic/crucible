//! A playbook's item store: labels its runs keep on the items they process, across runs.
//!
//! Every run of a playbook reads its items. A run writes only items it holds, and a hold lasts
//! only while the holding run's pod token is live, so a run that ends, fails, or loses its token
//! frees its items with no release.

use crate::admission::MAX_KEY_LEN;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The most keys one request names.
pub const MAX_KEYS: usize = 500;
/// The most labels one item carries.
pub const MAX_LABELS: usize = 64;
/// The longest store or label name.
pub const MAX_NAME_LEN: usize = 63;
/// The longest label value.
pub const MAX_VALUE_LEN: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ItemsError {
    #[error(
        "store name {0:?} is not 1-{MAX_NAME_LEN} characters of [a-z0-9-] that start and end alphanumeric"
    )]
    Store(String),
    #[error(
        "item key {0:?} is not 1-{MAX_KEY_LEN} bytes free of whitespace and control characters"
    )]
    Key(String),
    #[error(
        "label name {0:?} is not 1-{MAX_NAME_LEN} characters of [A-Za-z0-9._-] that start and end alphanumeric"
    )]
    Label(String),
    #[error(
        "label value {0:?} is longer than {MAX_VALUE_LEN} bytes or holds whitespace, a control character, or ','"
    )]
    Value(String),
    #[error("a request names {0} keys; it takes 1 to {MAX_KEYS}")]
    KeyCount(usize),
    #[error("key {0:?} is named twice")]
    DuplicateKey(String),
    #[error("an edit changes no label")]
    EmptyEdit,
    #[error("{0} labels; an item carries at most {MAX_LABELS}")]
    TooManyLabels(usize),
    #[error("selector term {0:?} is not `name`, `!name`, `name=value` or `name!=value`")]
    Term(String),
}

fn is_name(raw: &str, inner: impl Fn(u8) -> bool) -> bool {
    let bytes = raw.as_bytes();
    (1..=MAX_NAME_LEN).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().copied().all(inner)
}

macro_rules! checked_string {
    ($name:ident, $check:expr) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(raw: impl Into<String>) -> Result<Self, ItemsError> {
                let raw = raw.into();
                let check: fn(String) -> Result<String, ItemsError> = $check;
                check(raw).map(Self)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = ItemsError;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::new(raw)
            }
        }

        impl TryFrom<String> for $name {
            type Error = ItemsError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::new(raw)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }
    };
}

checked_string!(StoreName, |raw| {
    match is_name(&raw, |b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
    }) {
        true => Ok(raw),
        false => Err(ItemsError::Store(raw)),
    }
});

checked_string!(ItemKey, |raw| {
    let ok = !raw.is_empty()
        && raw.len() <= MAX_KEY_LEN
        && !raw.chars().any(|c| c.is_control() || c.is_whitespace());
    match ok {
        true => Ok(raw),
        false => Err(ItemsError::Key(raw)),
    }
});

checked_string!(LabelName, |raw| {
    match is_name(&raw, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
    }) {
        true => Ok(raw),
        false => Err(ItemsError::Label(raw)),
    }
});

checked_string!(LabelValue, |raw| {
    let ok = raw.len() <= MAX_VALUE_LEN
        && !raw
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == ',');
    match ok {
        true => Ok(raw),
        false => Err(ItemsError::Value(raw)),
    }
});

/// The keys one request names: 1 to [`MAX_KEYS`], none twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ItemKey>", into = "Vec<ItemKey>")]
pub struct ItemKeys(Vec<ItemKey>);

impl ItemKeys {
    pub fn new(keys: Vec<ItemKey>) -> Result<Self, ItemsError> {
        if keys.is_empty() || keys.len() > MAX_KEYS {
            return Err(ItemsError::KeyCount(keys.len()));
        }
        let mut seen = std::collections::BTreeSet::new();
        if let Some(twice) = keys.iter().find(|k| !seen.insert(*k)) {
            return Err(ItemsError::DuplicateKey(twice.to_string()));
        }
        Ok(Self(keys))
    }

    pub fn as_slice(&self) -> &[ItemKey] {
        &self.0
    }

    /// The keys as strings in sorted order, the order rows are locked in.
    pub fn sorted(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.0.iter().map(|k| k.0.clone()).collect();
        keys.sort();
        keys
    }
}

impl TryFrom<Vec<ItemKey>> for ItemKeys {
    type Error = ItemsError;

    fn try_from(keys: Vec<ItemKey>) -> Result<Self, Self::Error> {
        Self::new(keys)
    }
}

impl From<ItemKeys> for Vec<ItemKey> {
    fn from(keys: ItemKeys) -> Self {
        keys.0
    }
}

/// An item's labels, at most [`MAX_LABELS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<LabelName, LabelValue>",
    into = "BTreeMap<LabelName, LabelValue>"
)]
pub struct Labels(BTreeMap<LabelName, LabelValue>);

impl Labels {
    pub fn new(labels: BTreeMap<LabelName, LabelValue>) -> Result<Self, ItemsError> {
        match labels.len() > MAX_LABELS {
            true => Err(ItemsError::TooManyLabels(labels.len())),
            false => Ok(Self(labels)),
        }
    }

    pub fn get(&self, name: &LabelName) -> Option<&LabelValue> {
        self.0.get(name)
    }

    /// These labels with `edit` applied: a value sets its label, `None` removes it.
    pub fn apply(&self, edit: &LabelEdit) -> Result<Labels, ItemsError> {
        let mut next = self.0.clone();
        for (name, value) in &edit.0 {
            match value {
                Some(value) => next.insert(name.clone(), value.clone()),
                None => next.remove(name),
            };
        }
        Labels::new(next)
    }
}

impl TryFrom<BTreeMap<LabelName, LabelValue>> for Labels {
    type Error = ItemsError;

    fn try_from(labels: BTreeMap<LabelName, LabelValue>) -> Result<Self, Self::Error> {
        Self::new(labels)
    }
}

impl From<Labels> for BTreeMap<LabelName, LabelValue> {
    fn from(labels: Labels) -> Self {
        labels.0
    }
}

/// Labels to set (a value) or remove (`null`), at least one and at most [`MAX_LABELS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<LabelName, Option<LabelValue>>",
    into = "BTreeMap<LabelName, Option<LabelValue>>"
)]
pub struct LabelEdit(BTreeMap<LabelName, Option<LabelValue>>);

impl LabelEdit {
    pub fn new(edit: BTreeMap<LabelName, Option<LabelValue>>) -> Result<Self, ItemsError> {
        if edit.is_empty() {
            return Err(ItemsError::EmptyEdit);
        }
        if edit.len() > MAX_LABELS {
            return Err(ItemsError::TooManyLabels(edit.len()));
        }
        Ok(Self(edit))
    }
}

impl TryFrom<BTreeMap<LabelName, Option<LabelValue>>> for LabelEdit {
    type Error = ItemsError;

    fn try_from(edit: BTreeMap<LabelName, Option<LabelValue>>) -> Result<Self, Self::Error> {
        Self::new(edit)
    }
}

impl From<LabelEdit> for BTreeMap<LabelName, Option<LabelValue>> {
    fn from(edit: LabelEdit) -> Self {
        edit.0
    }
}

/// One term of a [`Selector`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Has(LabelName),
    Lacks(LabelName),
    Is(LabelName, LabelValue),
    /// The label is absent or holds another value.
    IsNot(LabelName, LabelValue),
}

impl Term {
    fn matches(&self, labels: &Labels) -> bool {
        match self {
            Term::Has(name) => labels.get(name).is_some(),
            Term::Lacks(name) => labels.get(name).is_none(),
            Term::Is(name, value) => labels.get(name) == Some(value),
            Term::IsNot(name, value) => labels.get(name) != Some(value),
        }
    }
}

impl std::str::FromStr for Term {
    type Err = ItemsError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let refused = || ItemsError::Term(raw.to_string());
        let term = if let Some((name, value)) = raw.split_once("!=") {
            Term::IsNot(name.parse().map_err(|_| refused())?, value.parse()?)
        } else if let Some((name, value)) = raw.split_once('=') {
            Term::Is(name.parse().map_err(|_| refused())?, value.parse()?)
        } else if let Some(name) = raw.strip_prefix('!') {
            Term::Lacks(name.parse().map_err(|_| refused())?)
        } else {
            Term::Has(raw.parse().map_err(|_| refused())?)
        };
        Ok(term)
    }
}

impl std::fmt::Display for Term {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Term::Has(name) => write!(f, "{name}"),
            Term::Lacks(name) => write!(f, "!{name}"),
            Term::Is(name, value) => write!(f, "{name}={value}"),
            Term::IsNot(name, value) => write!(f, "{name}!={value}"),
        }
    }
}

/// Comma-separated terms an item's labels must all satisfy, as in `triaged,verdict!=error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Selector(Vec<Term>);

impl Selector {
    pub fn matches(&self, labels: &Labels) -> bool {
        self.0.iter().all(|term| term.matches(labels))
    }
}

impl std::str::FromStr for Selector {
    type Err = ItemsError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        raw.split(',')
            .map(|term| term.trim().parse())
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, term) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{term}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for Selector {
    type Error = ItemsError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        raw.parse()
    }
}

impl From<Selector> for String {
    fn from(selector: Selector) -> Self {
        selector.to_string()
    }
}

/// Read items; a key never written is absent from the reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryRequest {
    pub keys: ItemKeys,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<Selector>,
}

/// Hold every key or none. A key another live run holds, or whose labels match `unless`, refuses
/// the whole claim; `force` skips `unless` and never takes another live run's hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    pub keys: ItemKeys,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless: Option<Selector>,
    #[serde(default)]
    pub force: bool,
}

/// Apply one edit to every key, all of which this run must hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetRequest {
    pub keys: ItemKeys,
    pub labels: LabelEdit,
}

/// Drop this run's holds on the keys; keys it does not hold are left alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequest {
    pub keys: ItemKeys,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub key: ItemKey,
    pub labels: Labels,
    /// The live run holding the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_by: Option<String>,
    /// When a run last set its labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
}

/// The items a request read or changed, by key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemsReply {
    pub items: Vec<Item>,
}

/// Why one key refused a claim or a set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Conflict {
    /// Another live run holds it.
    Held { key: ItemKey, run: String },
    /// Its labels match the claim's `unless`.
    Matched { key: ItemKey, labels: Labels },
    /// A set named a key this run does not hold.
    NotHeld { key: ItemKey },
}

/// The 409 body of a refused claim or set; nothing was changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub conflicts: Vec<Conflict>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Query,
    Claim,
    Set,
    Release,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Query => "query",
            Op::Claim => "claim",
            Op::Set => "set",
            Op::Release => "release",
        }
    }
}

/// The endpoint for `op` on `store` as run `run_id`, relative to the controller's URL. The run's
/// pod token authenticates it.
pub fn path(run_id: &str, store: &StoreName, op: Op) -> String {
    format!(
        "/api/items/runs/{}/{store}/{}",
        crate::artifact::path_segment(run_id),
        op.as_str()
    )
}

#[cfg(test)]
mod tests {
    use crate::items::*;

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        Labels::new(
            pairs
                .iter()
                .map(|(n, v)| (n.parse().unwrap(), v.parse().unwrap()))
                .collect(),
        )
        .unwrap()
    }

    fn keys(raw: &[&str]) -> ItemKeys {
        ItemKeys::new(raw.iter().map(|k| k.parse().unwrap()).collect()).unwrap()
    }

    #[test]
    fn store_names_are_lowercase_dns_labels() {
        for ok in ["tickets", "cve-shadow", "a", "a1", &"a".repeat(63)] {
            assert!(StoreName::new(ok).is_ok(), "{ok:?}");
        }
        for bad in [
            "",
            "-a",
            "a-",
            "Tickets",
            "a_b",
            "a.b",
            "a/b",
            &"a".repeat(64),
        ] {
            assert_eq!(StoreName::new(bad), Err(ItemsError::Store(bad.into())));
        }
    }

    #[test]
    fn item_keys_are_one_token_of_at_most_256_bytes() {
        for ok in [
            "PROJ-1307",
            "CVE-2026-1@sha256:ab",
            "a/b#1",
            &"k".repeat(256),
        ] {
            assert!(ItemKey::new(ok).is_ok(), "{ok:?}");
        }
        for bad in ["", "a b", "a\tb", "a\nb", &"k".repeat(257)] {
            assert_eq!(ItemKey::new(bad), Err(ItemsError::Key(bad.into())));
        }
    }

    #[test]
    fn label_names_and_values_have_no_separators() {
        for ok in ["triaged", "verdict.v2", "a_b-c", "A1"] {
            assert!(LabelName::new(ok).is_ok(), "{ok:?}");
        }
        for bad in ["", "_a", "a=", "a,b", "a b", "a/b", &"a".repeat(64)] {
            assert_eq!(LabelName::new(bad), Err(ItemsError::Label(bad.into())));
        }
        for ok in [
            "",
            "not_affected",
            "sha256:ab",
            "2026-10-28T12:00:00Z",
            &"v".repeat(256),
        ] {
            assert!(LabelValue::new(ok).is_ok(), "{ok:?}");
        }
        for bad in ["a b", "a,b", "a\u{7}", &"v".repeat(257)] {
            assert_eq!(LabelValue::new(bad), Err(ItemsError::Value(bad.into())));
        }
    }

    #[test]
    fn a_request_names_one_to_500_distinct_keys() {
        assert_eq!(ItemKeys::new(vec![]), Err(ItemsError::KeyCount(0)));
        let many: Vec<ItemKey> = (0..=MAX_KEYS)
            .map(|i| ItemKey::new(format!("K-{i}")).unwrap())
            .collect();
        assert_eq!(
            ItemKeys::new(many.clone()),
            Err(ItemsError::KeyCount(MAX_KEYS + 1))
        );
        assert!(ItemKeys::new(many[..MAX_KEYS].to_vec()).is_ok());
        let twice = vec![
            "A".parse().unwrap(),
            "B".parse().unwrap(),
            "A".parse().unwrap(),
        ];
        assert_eq!(
            ItemKeys::new(twice),
            Err(ItemsError::DuplicateKey("A".into()))
        );
        assert_eq!(keys(&["b", "a", "c"]).sorted(), ["a", "b", "c"]);
    }

    #[test]
    fn a_selector_parses_every_term_kind_and_prints_back() {
        let selector: Selector = "triaged, !error,verdict=not_affected,stage!=apply"
            .parse()
            .unwrap();
        assert_eq!(
            selector.to_string(),
            "triaged,!error,verdict=not_affected,stage!=apply"
        );
        assert_eq!(selector.to_string().parse::<Selector>().unwrap(), selector);
        assert!(
            "verdict=".parse::<Selector>().is_ok(),
            "an empty value is a value"
        );
        for bad in ["", "a,", "=v", "!", "a=b c", "!a=b", "a!=b,c d"] {
            assert!(bad.parse::<Selector>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn every_term_of_a_selector_must_match() {
        let item = labels(&[("triaged", ""), ("verdict", "not_affected")]);
        let bare = Labels::default();
        let case = |raw: &str, on: &Labels| raw.parse::<Selector>().unwrap().matches(on);
        assert!(case("triaged", &item));
        assert!(!case("triaged", &bare));
        assert!(case("!error", &item));
        assert!(!case("!triaged", &item));
        assert!(case("verdict=not_affected", &item));
        assert!(!case("verdict=affected", &item));
        assert!(case("verdict!=affected", &item));
        assert!(case("verdict!=affected", &bare), "absent is not equal");
        assert!(!case("verdict!=not_affected", &item));
        assert!(case("triaged,verdict=not_affected", &item));
        assert!(!case("triaged,verdict=affected", &item));
    }

    #[test]
    fn an_edit_sets_and_removes_and_bounds_the_result() {
        let before = labels(&[("pending", ""), ("verdict", "error")]);
        let edit: LabelEdit =
            serde_json::from_str(r#"{"pending": null, "verdict": "not_affected", "triaged": ""}"#)
                .unwrap();
        assert_eq!(
            before.apply(&edit).unwrap(),
            labels(&[("triaged", ""), ("verdict", "not_affected")])
        );
        let full = Labels::new(
            (0..MAX_LABELS)
                .map(|i| {
                    (
                        LabelName::new(format!("l{i}")).unwrap(),
                        LabelValue::new("").unwrap(),
                    )
                })
                .collect(),
        )
        .unwrap();
        let one_more: LabelEdit = serde_json::from_str(r#"{"extra": "x"}"#).unwrap();
        assert_eq!(
            full.apply(&one_more),
            Err(ItemsError::TooManyLabels(MAX_LABELS + 1))
        );
        let removal: LabelEdit = serde_json::from_str(r#"{"l0": null, "extra": "x"}"#).unwrap();
        assert!(full.apply(&removal).is_ok());
        assert!(serde_json::from_str::<LabelEdit>("{}").is_err());
    }

    #[test]
    fn requests_refuse_bad_fields_and_unknown_ones_on_the_wire() {
        let claim: ClaimRequest =
            serde_json::from_str(r#"{"keys": ["A-1", "A-2"], "unless": "triaged"}"#).unwrap();
        assert_eq!(claim.keys, keys(&["A-1", "A-2"]));
        assert!(!claim.force);
        assert_eq!(claim.unless.unwrap().to_string(), "triaged");
        for bad in [
            r#"{"keys": []}"#,
            r#"{"keys": ["A", "A"]}"#,
            r#"{"keys": ["A B"]}"#,
            r#"{"keys": ["A"], "unless": "a b"}"#,
            r#"{"keys": ["A"], "extra": 1}"#,
        ] {
            assert!(serde_json::from_str::<ClaimRequest>(bad).is_err(), "{bad}");
        }
        assert!(
            serde_json::from_str::<SetRequest>(r#"{"keys": ["A"], "labels": {"a b": "x"}}"#)
                .is_err()
        );
    }

    #[test]
    fn conflicts_are_tagged_by_reason() {
        let refused = Refused {
            conflicts: vec![
                Conflict::Held {
                    key: "A".parse().unwrap(),
                    run: "run-1".into(),
                },
                Conflict::Matched {
                    key: "B".parse().unwrap(),
                    labels: labels(&[("triaged", "")]),
                },
                Conflict::NotHeld {
                    key: "C".parse().unwrap(),
                },
            ],
        };
        assert_eq!(
            serde_json::to_value(&refused).unwrap(),
            serde_json::json!({"conflicts": [
                {"reason": "held", "key": "A", "run": "run-1"},
                {"reason": "matched", "key": "B", "labels": {"triaged": ""}},
                {"reason": "not_held", "key": "C"},
            ]})
        );
    }

    #[test]
    fn the_path_encodes_the_run_id_as_one_segment() {
        let store = StoreName::new("tickets").unwrap();
        assert_eq!(
            path("playbook_x_01/a", &store, Op::Claim),
            "/api/items/runs/playbook_x_01%2Fa/tickets/claim"
        );
    }
}

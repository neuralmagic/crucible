//! Typed questions a route task puts to a decision model, and the answers it records.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const UNCERTAIN: &str = "uncertain";
pub const NOUL_YES: &str = "yes";
pub const NOUL_NO: &str = "no";

const MAX_IDENT_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentError {
    pub value: String,
}

impl std::fmt::Display for IdentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} is not an identifier; use 1-{MAX_IDENT_LEN} ASCII letters, digits, or `_`",
            self.value
        )
    }
}

impl std::error::Error for IdentError {}

fn identifier(value: String) -> Result<String, IdentError> {
    let ok = !value.is_empty()
        && value.len() <= MAX_IDENT_LEN
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if ok {
        Ok(value)
    } else {
        Err(IdentError { value })
    }
}

macro_rules! ident_newtype {
    ($name:ident) => {
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(&self.0, f)
            }
        }

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentError> {
                identifier(value.into()).map(Self)
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentError;
            fn try_from(value: String) -> Result<Self, IdentError> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

ident_newtype!(QuestionId);
ident_newtype!(Label);

impl Label {
    pub fn uncertain() -> Self {
        Label(UNCERTAIN.to_owned())
    }

    pub fn is_uncertain(&self) -> bool {
        self.0 == UNCERTAIN
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub label: Label,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum QuestionKind {
    Noul,
    Choice {
        options: Vec<ChoiceOption>,
    },
    /// Ordered levels, lowest first.
    Score {
        levels: Vec<ChoiceOption>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub instructions: String,
    #[serde(flatten)]
    pub kind: QuestionKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop: Vec<Label>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionError {
    EmptyInstructions,
    TooFewOptions { got: usize },
    DuplicateOption { label: Label },
    ReservedOption,
    TooFewLevels { got: usize },
    DuplicateLevel { label: Label },
    ReservedLevel,
    UnknownDrop { label: Label },
}

impl std::fmt::Display for QuestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuestionError::EmptyInstructions => f.write_str("instructions are empty"),
            QuestionError::TooFewOptions { got } => {
                write!(f, "a choice needs at least two options, got {got}")
            }
            QuestionError::DuplicateOption { label } => {
                write!(f, "option {label:?} is declared twice")
            }
            QuestionError::ReservedOption => {
                write!(
                    f,
                    "option {UNCERTAIN:?} is reserved for a low-confidence answer"
                )
            }
            QuestionError::TooFewLevels { got } => {
                write!(f, "a score needs at least two levels, got {got}")
            }
            QuestionError::DuplicateLevel { label } => {
                write!(f, "level {label:?} is declared twice")
            }
            QuestionError::ReservedLevel => {
                write!(
                    f,
                    "level {UNCERTAIN:?} is reserved for a low-confidence answer"
                )
            }
            QuestionError::UnknownDrop { label } => {
                write!(
                    f,
                    "drop names {label:?}, which is not one of the question's labels"
                )
            }
        }
    }
}

impl std::error::Error for QuestionError {}

impl Question {
    /// The labels the model may answer with, in declaration order. Excludes `uncertain`.
    pub fn labels(&self) -> Vec<Label> {
        match &self.kind {
            QuestionKind::Noul => vec![Label(NOUL_YES.to_owned()), Label(NOUL_NO.to_owned())],
            QuestionKind::Choice { options } | QuestionKind::Score { levels: options } => {
                options.iter().map(|o| o.label.clone()).collect()
            }
        }
    }

    pub fn resolves_to(&self, label: &Label) -> bool {
        label.is_uncertain() || self.labels().contains(label)
    }

    pub fn validate(&self) -> Result<(), QuestionError> {
        if self.instructions.trim().is_empty() {
            return Err(QuestionError::EmptyInstructions);
        }
        match &self.kind {
            QuestionKind::Noul => {}
            QuestionKind::Choice { options } => {
                distinct(options).map_err(|fault| match fault {
                    OptionFault::TooFew { got } => QuestionError::TooFewOptions { got },
                    OptionFault::Duplicate { label } => QuestionError::DuplicateOption { label },
                    OptionFault::Reserved => QuestionError::ReservedOption,
                })?;
            }
            QuestionKind::Score { levels } => {
                distinct(levels).map_err(|fault| match fault {
                    OptionFault::TooFew { got } => QuestionError::TooFewLevels { got },
                    OptionFault::Duplicate { label } => QuestionError::DuplicateLevel { label },
                    OptionFault::Reserved => QuestionError::ReservedLevel,
                })?;
            }
        }
        for label in &self.drop {
            if !self.resolves_to(label) {
                return Err(QuestionError::UnknownDrop {
                    label: label.clone(),
                });
            }
        }
        Ok(())
    }

    /// The score recorded for answering `label` with certainty: its level position over n - 1.
    /// `None` for a question that is not a score or a label that is not one of its levels.
    pub fn level_score(&self, label: &Label) -> Option<f64> {
        let QuestionKind::Score { levels } = &self.kind else {
            return None;
        };
        let top = u32::try_from(levels.len().checked_sub(1)?)
            .ok()
            .filter(|top| *top > 0)?;
        let at = u32::try_from(levels.iter().position(|level| &level.label == label)?).ok()?;
        Some(f64::from(at) / f64::from(top))
    }

    /// Pick the most probable declared label, or `uncertain` when it falls below
    /// `min_confidence`. `probabilities` must hold exactly this question's labels. A score's
    /// distribution is normalized first, and its tie goes to the lowest level.
    pub fn resolve(
        &self,
        probabilities: BTreeMap<Label, f64>,
        min_confidence: f64,
    ) -> Result<Answer, ResolveError> {
        let labels = self.labels();
        if let Some(label) = probabilities.keys().find(|l| !labels.contains(l)) {
            return Err(ResolveError::UndeclaredLabel {
                label: label.clone(),
            });
        }
        let mut best: Option<(&Label, f64)> = None;
        for label in &labels {
            let Some(&p) = probabilities.get(label) else {
                return Err(ResolveError::MissingLabel {
                    label: label.clone(),
                });
            };
            if !(0.0..=1.0).contains(&p) {
                return Err(ResolveError::ProbabilityOutOfRange {
                    label: label.clone(),
                    probability: p,
                });
            }
            if best.is_none_or(|(_, top)| p > top) {
                best = Some((label, p));
            }
        }
        let Some((top, confidence)) = best else {
            return Err(ResolveError::NoLabels);
        };
        if let QuestionKind::Score { .. } = self.kind {
            return self.resolve_score(&labels, probabilities, min_confidence);
        }
        let label = if confidence < min_confidence {
            Label::uncertain()
        } else {
            top.clone()
        };
        Ok(Answer {
            label,
            confidence,
            probabilities,
            score: None,
            asked_as: None,
        })
    }

    fn resolve_score(
        &self,
        levels: &[Label],
        probabilities: BTreeMap<Label, f64>,
        min_confidence: f64,
    ) -> Result<Answer, ResolveError> {
        let total: f64 = probabilities.values().sum();
        if total <= 0.0 {
            return Err(ResolveError::AllZero);
        }
        let probabilities: BTreeMap<Label, f64> = probabilities
            .into_iter()
            .map(|(label, p)| (label, p / total))
            .collect();
        let mut best: Option<(&Label, f64)> = None;
        let mut score = 0.0;
        for level in levels {
            let p = probabilities.get(level).copied().unwrap_or(0.0);
            if best.is_none_or(|(_, top)| p > top) {
                best = Some((level, p));
            }
            let position = self
                .level_score(level)
                .ok_or(ResolveError::TooFewLevels { got: levels.len() })?;
            score += p * position;
        }
        let Some((top, confidence)) = best else {
            return Err(ResolveError::NoLabels);
        };
        let label = if confidence < min_confidence {
            Label::uncertain()
        } else {
            top.clone()
        };
        Ok(Answer {
            label,
            confidence,
            probabilities,
            score: Some(score),
            asked_as: None,
        })
    }
}

enum OptionFault {
    TooFew { got: usize },
    Duplicate { label: Label },
    Reserved,
}

fn distinct(options: &[ChoiceOption]) -> Result<(), OptionFault> {
    if options.len() < 2 {
        return Err(OptionFault::TooFew { got: options.len() });
    }
    let mut seen = BTreeSet::new();
    for option in options {
        if option.label.is_uncertain() {
            return Err(OptionFault::Reserved);
        }
        if !seen.insert(&option.label) {
            return Err(OptionFault::Duplicate {
                label: option.label.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    NoLabels,
    AllZero,
    TooFewLevels { got: usize },
    UndeclaredLabel { label: Label },
    MissingLabel { label: Label },
    ProbabilityOutOfRange { label: Label, probability: f64 },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoLabels => f.write_str("the question declares no labels"),
            ResolveError::AllZero => f.write_str("the model gave every level probability 0"),
            ResolveError::TooFewLevels { got } => {
                write!(f, "a score needs at least two levels, got {got}")
            }
            ResolveError::UndeclaredLabel { label } => {
                write!(f, "the model answered with undeclared label {label:?}")
            }
            ResolveError::MissingLabel { label } => {
                write!(f, "the model gave no probability for {label:?}")
            }
            ResolveError::ProbabilityOutOfRange { label, probability } => {
                write!(
                    f,
                    "probability {probability} for {label:?} is outside [0, 1]"
                )
            }
        }
    }
}

impl std::error::Error for ResolveError {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub label: Label,
    pub confidence: f64,
    pub probabilities: BTreeMap<Label, f64>,
    /// A score question's expected level position in [0, 1], recorded whatever the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// The form a model-decided score question was put to the decision API in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_as: Option<AskedAs>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskedAs {
    Score,
    Choice,
}

/// A route task's output: one answer per declared question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Decision(pub BTreeMap<QuestionId, Answer>);

#[cfg(test)]
mod tests {
    use crate::decision::*;

    fn label(s: &str) -> Label {
        Label::new(s).unwrap()
    }

    fn choice(options: &[&str]) -> Question {
        Question {
            instructions: "which?".into(),
            kind: QuestionKind::Choice {
                options: options
                    .iter()
                    .map(|o| ChoiceOption {
                        label: label(o),
                        description: None,
                    })
                    .collect(),
            },
            drop: vec![],
        }
    }

    fn score(levels: &[&str]) -> Question {
        Question {
            instructions: "how risky?".into(),
            kind: QuestionKind::Score {
                levels: levels
                    .iter()
                    .map(|l| ChoiceOption {
                        label: label(l),
                        description: Some(format!("{l} risk")),
                    })
                    .collect(),
            },
            drop: vec![],
        }
    }

    fn noul() -> Question {
        Question {
            instructions: "is it?".into(),
            kind: QuestionKind::Noul,
            drop: vec![],
        }
    }

    fn probs(pairs: &[(&str, f64)]) -> BTreeMap<Label, f64> {
        pairs.iter().map(|(l, p)| (label(l), *p)).collect()
    }

    #[test]
    fn identifiers_reject_empty_long_and_punctuated_values() {
        assert!(Label::new("kv_cache").is_ok());
        assert!(Label::new("").is_err());
        assert!(Label::new("kv-cache").is_err());
        assert!(Label::new("a b").is_err());
        assert!(QuestionId::new("x".repeat(65)).is_err());
        assert!(QuestionId::new("x".repeat(64)).is_ok());
    }

    #[test]
    fn an_invalid_identifier_fails_to_deserialize() {
        assert!(serde_json::from_str::<Label>("\"ok_1\"").is_ok());
        assert!(serde_json::from_str::<Label>("\"not ok\"").is_err());
    }

    #[test]
    fn errors_quote_a_label_as_a_plain_string() {
        let err = QuestionError::UnknownDrop {
            label: label("ghost"),
        };
        assert_eq!(
            err.to_string(),
            "drop names \"ghost\", which is not one of the question's labels"
        );
        let err = ResolveError::UndeclaredLabel { label: label("z") };
        assert_eq!(
            err.to_string(),
            "the model answered with undeclared label \"z\""
        );
    }

    #[test]
    fn a_noul_answers_yes_or_no() {
        assert_eq!(noul().labels(), vec![label("yes"), label("no")]);
    }

    #[test]
    fn a_choice_needs_two_distinct_unreserved_options() {
        assert_eq!(
            choice(&["a"]).validate(),
            Err(QuestionError::TooFewOptions { got: 1 })
        );
        assert_eq!(
            choice(&["a", "a"]).validate(),
            Err(QuestionError::DuplicateOption { label: label("a") })
        );
        assert_eq!(
            choice(&["a", "uncertain"]).validate(),
            Err(QuestionError::ReservedOption)
        );
        assert_eq!(choice(&["a", "b"]).validate(), Ok(()));
    }

    #[test]
    fn empty_instructions_are_rejected() {
        let mut q = noul();
        q.instructions = "  ".into();
        assert_eq!(q.validate(), Err(QuestionError::EmptyInstructions));
    }

    #[test]
    fn drop_may_name_a_declared_label_or_uncertain_only() {
        let mut q = choice(&["a", "b"]);
        q.drop = vec![label("b"), Label::uncertain()];
        assert_eq!(q.validate(), Ok(()));
        q.drop = vec![label("c")];
        assert_eq!(
            q.validate(),
            Err(QuestionError::UnknownDrop { label: label("c") })
        );
    }

    #[test]
    fn resolve_picks_the_most_probable_label() {
        let a = choice(&["a", "b", "c"])
            .resolve(probs(&[("a", 0.1), ("b", 0.85), ("c", 0.05)]), 0.8)
            .unwrap();
        assert_eq!(a.label, label("b"));
        assert_eq!(a.confidence, 0.85);
        assert_eq!(a.probabilities.len(), 3);
    }

    #[test]
    fn resolve_below_the_threshold_is_uncertain_and_keeps_the_distribution() {
        let a = choice(&["a", "b"])
            .resolve(probs(&[("a", 0.55), ("b", 0.45)]), 0.8)
            .unwrap();
        assert!(a.label.is_uncertain());
        assert_eq!(a.confidence, 0.55);
        assert_eq!(a.probabilities[&label("a")], 0.55);
    }

    #[test]
    fn resolve_at_exactly_the_threshold_is_confident() {
        let a = noul()
            .resolve(probs(&[("yes", 0.8), ("no", 0.2)]), 0.8)
            .unwrap();
        assert_eq!(a.label, label("yes"));
    }

    #[test]
    fn resolve_refuses_an_undeclared_label() {
        assert_eq!(
            choice(&["a", "b"]).resolve(probs(&[("a", 0.5), ("b", 0.2), ("z", 0.3)]), 0.5),
            Err(ResolveError::UndeclaredLabel { label: label("z") })
        );
    }

    #[test]
    fn resolve_refuses_a_missing_label() {
        assert_eq!(
            choice(&["a", "b"]).resolve(probs(&[("a", 0.9)]), 0.5),
            Err(ResolveError::MissingLabel { label: label("b") })
        );
    }

    #[test]
    fn resolve_refuses_a_probability_outside_the_unit_interval() {
        for bad in [-0.1, 1.5, f64::NAN] {
            let err = noul()
                .resolve(probs(&[("yes", bad), ("no", 0.1)]), 0.5)
                .unwrap_err();
            assert!(matches!(err, ResolveError::ProbabilityOutOfRange { .. }));
        }
    }

    #[test]
    fn a_question_round_trips_through_json() {
        let mut q = choice(&["a", "b"]);
        q.drop = vec![Label::uncertain()];
        let back: Question = serde_json::from_str(&serde_json::to_string(&q).unwrap()).unwrap();
        assert_eq!(back, q);
        let n: Question =
            serde_json::from_str(r#"{"instructions":"is it?","type":"noul"}"#).unwrap();
        assert_eq!(n, noul());
    }

    #[test]
    fn a_decision_serializes_as_a_map_of_question_to_answer() {
        let answer = noul()
            .resolve(probs(&[("yes", 0.9), ("no", 0.1)]), 0.5)
            .unwrap();
        let decision = Decision(BTreeMap::from([(
            QuestionId::new("urgent").unwrap(),
            answer,
        )]));
        let v = serde_json::to_value(&decision).unwrap();
        assert_eq!(v["urgent"]["label"], "yes");
        assert_eq!(v["urgent"]["probabilities"]["no"], 0.1);
    }

    #[test]
    fn a_score_answers_its_levels_in_declared_order() {
        assert_eq!(
            score(&["low", "medium", "high"]).labels(),
            vec![label("low"), label("medium"), label("high")]
        );
    }

    #[test]
    fn a_score_needs_two_distinct_unreserved_levels() {
        assert_eq!(
            score(&["low"]).validate(),
            Err(QuestionError::TooFewLevels { got: 1 })
        );
        assert_eq!(
            score(&["low", "low"]).validate(),
            Err(QuestionError::DuplicateLevel {
                label: label("low")
            })
        );
        assert_eq!(
            score(&["low", "uncertain"]).validate(),
            Err(QuestionError::ReservedLevel)
        );
        assert_eq!(score(&["low", "high"]).validate(), Ok(()));
        assert_eq!(
            QuestionError::TooFewLevels { got: 1 }.to_string(),
            "a score needs at least two levels, got 1"
        );
    }

    #[test]
    fn a_score_resolves_to_its_most_probable_level_with_the_expected_position() {
        let a = score(&["low", "medium", "high"])
            .resolve(probs(&[("low", 0.2), ("medium", 0.2), ("high", 0.6)]), 0.5)
            .unwrap();
        assert_eq!(a.label, label("high"));
        assert!((a.confidence - 0.6).abs() < 1e-12);
        assert!((a.score.unwrap() - 0.7).abs() < 1e-12);
        assert_eq!(a.asked_as, None);
    }

    #[test]
    fn a_score_normalizes_its_distribution_before_choosing() {
        let a = score(&["low", "medium", "high"])
            .resolve(probs(&[("low", 0.1), ("medium", 0.3), ("high", 0.0)]), 0.7)
            .unwrap();
        assert_eq!(a.label, label("medium"));
        assert!((a.confidence - 0.75).abs() < 1e-12);
        assert!((a.probabilities[&label("low")] - 0.25).abs() < 1e-12);
        assert!((a.probabilities.values().sum::<f64>() - 1.0).abs() < 1e-12);
        assert!((a.score.unwrap() - 0.375).abs() < 1e-12);
    }

    #[test]
    fn a_score_tie_goes_to_the_lowest_level() {
        let a = score(&["low", "medium", "high"])
            .resolve(probs(&[("low", 0.0), ("medium", 0.4), ("high", 0.4)]), 0.5)
            .unwrap();
        assert_eq!(a.label, label("medium"));
        assert!((a.score.unwrap() - 0.75).abs() < 1e-12);
    }

    #[test]
    fn a_score_below_the_threshold_is_uncertain_and_keeps_its_score() {
        let a = score(&["low", "high"])
            .resolve(probs(&[("low", 0.45), ("high", 0.55)]), 0.8)
            .unwrap();
        assert!(a.label.is_uncertain());
        assert!((a.score.unwrap() - 0.55).abs() < 1e-12);
    }

    #[test]
    fn a_score_with_every_probability_zero_fails() {
        assert_eq!(
            score(&["low", "high"]).resolve(probs(&[("low", 0.0), ("high", 0.0)]), 0.5),
            Err(ResolveError::AllZero)
        );
    }

    #[test]
    fn a_score_still_refuses_a_missing_or_undeclared_level() {
        assert_eq!(
            score(&["low", "high"]).resolve(probs(&[("low", 1.0)]), 0.5),
            Err(ResolveError::MissingLabel {
                label: label("high")
            })
        );
        assert_eq!(
            score(&["low", "high"])
                .resolve(probs(&[("low", 0.5), ("high", 0.2), ("extreme", 0.3)]), 0.5),
            Err(ResolveError::UndeclaredLabel {
                label: label("extreme")
            })
        );
    }

    #[test]
    fn a_level_score_is_its_position_over_the_top_position() {
        let q = score(&["low", "medium", "high"]);
        assert_eq!(q.level_score(&label("low")), Some(0.0));
        assert_eq!(q.level_score(&label("medium")), Some(0.5));
        assert_eq!(q.level_score(&label("high")), Some(1.0));
        assert_eq!(q.level_score(&Label::uncertain()), None);
        assert_eq!(choice(&["a", "b"]).level_score(&label("a")), None);
    }

    #[test]
    fn noul_and_choice_answers_keep_their_wire_shape() {
        let noul = noul()
            .resolve(probs(&[("yes", 0.75), ("no", 0.25)]), 0.5)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&noul).unwrap(),
            serde_json::json!({"label": "yes", "confidence": 0.75, "probabilities": {"yes": 0.75, "no": 0.25}})
        );
        let choice = choice(&["a", "b"])
            .resolve(probs(&[("a", 0.25), ("b", 0.75)]), 0.5)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&choice).unwrap(),
            serde_json::json!({"label": "b", "confidence": 0.75, "probabilities": {"a": 0.25, "b": 0.75}})
        );
    }

    #[test]
    fn a_score_answer_serializes_its_score_and_the_form_it_was_asked_in() {
        let mut a = score(&["low", "high"])
            .resolve(probs(&[("low", 0.25), ("high", 0.75)]), 0.5)
            .unwrap();
        a.asked_as = Some(AskedAs::Choice);
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "label": "high",
                "confidence": 0.75,
                "probabilities": {"low": 0.25, "high": 0.75},
                "score": 0.75,
                "asked_as": "choice",
            })
        );
        let back: Answer = serde_json::from_value(v).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn a_score_question_round_trips_with_its_levels_in_order() {
        let q = score(&["low", "medium", "high"]);
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["type"], "score");
        assert_eq!(v["levels"][0]["label"], "low");
        assert_eq!(v["levels"][2]["label"], "high");
        let back: Question = serde_json::from_value(v).unwrap();
        assert_eq!(back, q);
    }
}

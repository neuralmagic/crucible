//! A human-decided route's questions as the controller posts them, and the answers it reads back.
//!
//! The engine opens a question and polls it; the controller owns the Slack message and the click.
//! Both sides decode through these types, so a label the stored question does not declare is
//! refused at each end.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::decision::{Label, Question, QuestionId, QuestionKind};

/// Env var naming the controller's elicitation base URL; the engine appends `/{task}`.
pub const ENV_ELICIT_URL: &str = "CRUCIBLE_ELICIT_URL";

/// Env var naming the file that holds the bearer for [`ENV_ELICIT_URL`], re-read per request.
pub const ENV_ELICIT_TOKEN_PATH: &str = "CRUCIBLE_ELICIT_TOKEN_PATH";

/// The most buttons Slack renders in one actions block, so the most labels a person can pick from.
pub const MAX_BUTTONS: usize = 25;

/// Slack's limit on one text object, which carries a question's instructions or its option
/// descriptions.
pub const MAX_TEXT_LEN: usize = 3000;

/// What the engine PUTs to open a question. Opening an open question changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitRequest {
    pub questions: BTreeMap<QuestionId, Question>,
    pub deadline_secs: u64,
}

/// What the controller answers to a PUT or a GET: the time left before the deadline fixed on
/// first delivery, and the first accepted label per answered question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElicitStatus {
    pub expires_in_secs: u64,
    #[serde(default)]
    pub answers: BTreeMap<QuestionId, Label>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    UndeclaredQuestion { question: QuestionId },
    UndeclaredLabel { question: QuestionId, label: Label },
    Uncertain { question: QuestionId },
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnswerError::UndeclaredQuestion { question } => {
                write!(f, "an answer names undeclared question {question:?}")
            }
            AnswerError::UndeclaredLabel { question, label } => {
                write!(
                    f,
                    "question {question:?} was answered with undeclared label {label:?}"
                )
            }
            AnswerError::Uncertain { question } => write!(
                f,
                "question {question:?} was answered \"uncertain\", which only the engine records"
            ),
        }
    }
}

impl std::error::Error for AnswerError {}

/// Check one answer against the questions it claims to answer.
pub fn check_answer(
    questions: &BTreeMap<QuestionId, Question>,
    question: &QuestionId,
    label: &Label,
) -> Result<(), AnswerError> {
    let Some(asked) = questions.get(question) else {
        return Err(AnswerError::UndeclaredQuestion {
            question: question.clone(),
        });
    };
    if label.is_uncertain() {
        return Err(AnswerError::Uncertain {
            question: question.clone(),
        });
    }
    if !asked.labels().contains(label) {
        return Err(AnswerError::UndeclaredLabel {
            question: question.clone(),
            label: label.clone(),
        });
    }
    Ok(())
}

impl ElicitStatus {
    /// Whether every question has an answer.
    pub fn complete(&self, questions: &BTreeMap<QuestionId, Question>) -> bool {
        questions.keys().all(|id| self.answers.contains_key(id))
    }

    /// Every answer, checked against the questions asked.
    pub fn check(&self, questions: &BTreeMap<QuestionId, Question>) -> Result<(), AnswerError> {
        self.answers
            .iter()
            .try_for_each(|(question, label)| check_answer(questions, question, label))
    }
}

/// A Slack user or channel id (`U024BE7LH`, `C123ABC456`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SlackId(String);

impl std::fmt::Debug for SlackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl std::fmt::Display for SlackId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl SlackId {
    pub fn new(value: impl Into<String>) -> Result<Self, ClickError> {
        let value = value.into();
        let ok = (2..=32).contains(&value.len())
            && value
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if ok {
            Ok(Self(value))
        } else {
            Err(ClickError::BadId { value })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SlackId {
    type Error = ClickError;
    fn try_from(value: String) -> Result<Self, ClickError> {
        Self::new(value)
    }
}

impl From<SlackId> for String {
    fn from(value: SlackId) -> String {
        value.0
    }
}

/// An answer already accepted, as the message shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    pub label: Label,
    pub by: SlackId,
}

/// Everything the message says besides the questions. `run_url` is the controller's own link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageContext<'a> {
    pub run: &'a str,
    pub task: &'a str,
    pub run_url: Option<&'a str>,
    pub deadline_unix: i64,
}

fn slack_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn instructions_text(question: &Question) -> String {
    format!("*{}*", slack_escape(&question.instructions))
}

fn descriptions_text(question: &Question) -> Option<String> {
    let QuestionKind::Choice { options } = &question.kind else {
        return None;
    };
    let described: Vec<String> = options
        .iter()
        .filter_map(|o| {
            o.description
                .as_deref()
                .map(|d| format!("`{}` {}", o.label, slack_escape(d)))
        })
        .collect();
    (!described.is_empty()).then(|| described.join("\n"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackLimit {
    TooManyLabels { got: usize },
    TextTooLong { got: usize },
}

impl std::fmt::Display for SlackLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlackLimit::TooManyLabels { got } => write!(
                f,
                "a person picks from at most {MAX_BUTTONS} labels, and this question has {got}"
            ),
            SlackLimit::TextTooLong { got } => write!(
                f,
                "its text renders to {got} characters, over Slack's {MAX_TEXT_LEN}"
            ),
        }
    }
}

impl std::error::Error for SlackLimit {}

/// Whether Slack can render `question` as [`slack_message`] renders it.
pub fn fits_slack(question: &Question) -> Result<(), SlackLimit> {
    let labels = question.labels().len();
    if labels > MAX_BUTTONS {
        return Err(SlackLimit::TooManyLabels { got: labels });
    }
    for text in std::iter::once(instructions_text(question)).chain(descriptions_text(question)) {
        let got = text.chars().count();
        if got > MAX_TEXT_LEN {
            return Err(SlackLimit::TextTooLong { got });
        }
    }
    Ok(())
}

/// The Block Kit message for a question: one actions block per open question, with one button
/// per declared label and nothing a person can type into. Answered and expired questions lose
/// their buttons.
pub fn slack_message(
    ctx: &MessageContext<'_>,
    questions: &BTreeMap<QuestionId, Question>,
    answered: &BTreeMap<QuestionId, Answered>,
    expired: bool,
) -> Value {
    let deadline = format!(
        "<!date^{}^{{date_short_pretty}} at {{time}}|the deadline>",
        ctx.deadline_unix
    );
    let mut blocks = vec![
        json!({
            "type": "header",
            "text": {"type": "plain_text", "text": "Crucible needs an answer"}
        }),
        json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": format!(
                "*{}* · `{}`\nUnanswered questions are recorded as uncertain at {deadline}.",
                slack_escape(ctx.run),
                slack_escape(ctx.task),
            )}
        }),
    ];
    for (id, question) in questions {
        blocks.push(json!({"type": "divider"}));
        blocks.push(json!({
            "type": "section",
            "text": {"type": "mrkdwn", "text": instructions_text(question)}
        }));
        if let Some(described) = descriptions_text(question) {
            blocks.push(json!({
                "type": "context",
                "elements": [{"type": "mrkdwn", "text": described}]
            }));
        }
        let state = match answered.get(id) {
            Some(Answered { label, by }) => Some(format!("Answered *{label}* by <@{by}>")),
            None if expired => Some("No answer before the deadline: recorded as uncertain".into()),
            None => None,
        };
        match state {
            Some(text) => blocks.push(json!({
                "type": "context",
                "elements": [{"type": "mrkdwn", "text": text}]
            })),
            None => blocks.push(json!({
                "type": "actions",
                "block_id": id.as_str(),
                "elements": question.labels().iter().map(|label| json!({
                    "type": "button",
                    "action_id": label.as_str(),
                    "value": label.as_str(),
                    "text": {"type": "plain_text", "text": label.as_str()},
                })).collect::<Vec<_>>(),
            })),
        }
    }
    if let Some(url) = ctx.run_url {
        blocks.push(json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": format!("<{}|Open run in Crucible>", slack_escape(url))}]
        }));
    }
    json!({
        "text": format!(
            "Crucible needs an answer: {} {}",
            slack_escape(ctx.run),
            slack_escape(ctx.task)
        ),
        "blocks": blocks,
    })
}

/// One button click, decoded against the stored question. The controller still checks the
/// channel, the clicker's membership, and whether the question already has an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Click {
    pub user: SlackId,
    pub channel: SlackId,
    pub message_ts: String,
    pub question: QuestionId,
    pub label: Label,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickError {
    NotBlockActions { got: String },
    Missing { field: &'static str },
    ActionCount { got: usize },
    NotAButton { got: String },
    ValueMismatch { action_id: String, value: String },
    BadId { value: String },
    Ident(crate::decision::IdentError),
    Answer(AnswerError),
}

impl std::fmt::Display for ClickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClickError::NotBlockActions { got } => {
                write!(f, "interaction type is {got:?}, not \"block_actions\"")
            }
            ClickError::Missing { field } => write!(f, "interaction has no {field}"),
            ClickError::ActionCount { got } => {
                write!(f, "interaction carries {got} actions, not exactly one")
            }
            ClickError::NotAButton { got } => write!(f, "action type is {got:?}, not \"button\""),
            ClickError::ValueMismatch { action_id, value } => write!(
                f,
                "action_id {action_id:?} and value {value:?} name different labels"
            ),
            ClickError::BadId { value } => write!(f, "{value:?} is not a Slack id"),
            ClickError::Ident(e) => write!(f, "{e}"),
            ClickError::Answer(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClickError {}

fn text<'v>(value: &'v Value, pointer: &str, field: &'static str) -> Result<&'v str, ClickError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .ok_or(ClickError::Missing { field })
}

/// Decode a Slack `block_actions` payload into the one label it chose.
pub fn decode_click(
    payload: &Value,
    questions: &BTreeMap<QuestionId, Question>,
) -> Result<Click, ClickError> {
    let kind = text(payload, "/type", "type")?;
    if kind != "block_actions" {
        return Err(ClickError::NotBlockActions {
            got: kind.to_owned(),
        });
    }
    let actions = payload
        .get("actions")
        .and_then(Value::as_array)
        .ok_or(ClickError::Missing { field: "actions" })?;
    let [action] = actions.as_slice() else {
        return Err(ClickError::ActionCount { got: actions.len() });
    };
    let kind = text(action, "/type", "action type")?;
    if kind != "button" {
        return Err(ClickError::NotAButton {
            got: kind.to_owned(),
        });
    }
    let action_id = text(action, "/action_id", "action_id")?;
    let value = text(action, "/value", "value")?;
    if action_id != value {
        return Err(ClickError::ValueMismatch {
            action_id: action_id.to_owned(),
            value: value.to_owned(),
        });
    }
    let question =
        QuestionId::new(text(action, "/block_id", "block_id")?).map_err(ClickError::Ident)?;
    let label = Label::new(value).map_err(ClickError::Ident)?;
    check_answer(questions, &question, &label).map_err(ClickError::Answer)?;
    Ok(Click {
        user: SlackId::new(text(payload, "/user/id", "user.id")?)?,
        channel: SlackId::new(text(payload, "/channel/id", "channel.id")?)?,
        message_ts: text(payload, "/container/message_ts", "container.message_ts")?.to_owned(),
        question,
        label,
    })
}

#[cfg(test)]
mod tests {
    use crate::decision::ChoiceOption;
    use crate::elicit::*;

    fn qid(s: &str) -> QuestionId {
        QuestionId::new(s).unwrap()
    }

    fn label(s: &str) -> Label {
        Label::new(s).unwrap()
    }

    fn questions() -> BTreeMap<QuestionId, Question> {
        BTreeMap::from([
            (
                qid("ship"),
                Question {
                    instructions: "Ship <this> & that?".into(),
                    kind: QuestionKind::Noul,
                    drop: vec![],
                },
            ),
            (
                qid("owner"),
                Question {
                    instructions: "Which team owns it?".into(),
                    kind: QuestionKind::Choice {
                        options: vec![
                            ChoiceOption {
                                label: label("serving"),
                                description: Some("the <@here> runtime".into()),
                            },
                            ChoiceOption {
                                label: label("kernels"),
                                description: None,
                            },
                        ],
                    },
                    drop: vec![],
                },
            ),
        ])
    }

    fn ctx() -> MessageContext<'static> {
        MessageContext {
            run: "nightly<1>",
            task: "gate",
            run_url: Some("https://crucible.example/runs/nightly?a=1&b=2"),
            deadline_unix: 1_790_000_000,
        }
    }

    fn click(block: &str, action: &str, value: &str) -> Value {
        json!({
            "type": "block_actions",
            "user": {"id": "U024BE7LH", "username": "someone"},
            "channel": {"id": "C123ABC456"},
            "container": {"type": "message", "message_ts": "1790000000.000100"},
            "actions": [{"type": "button", "block_id": block, "action_id": action, "value": value}],
        })
    }

    #[test]
    fn a_request_and_a_status_round_trip_through_json() {
        let request = ElicitRequest {
            questions: questions(),
            deadline_secs: 3600,
        };
        let back: ElicitRequest =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(back, request);
        let status: ElicitStatus =
            serde_json::from_str(r#"{"expires_in_secs":12,"answers":{"ship":"yes"}}"#).unwrap();
        assert_eq!(status.answers[&qid("ship")], label("yes"));
        let empty: ElicitStatus = serde_json::from_str(r#"{"expires_in_secs":0}"#).unwrap();
        assert!(empty.answers.is_empty());
    }

    #[test]
    fn a_status_whose_answer_is_not_an_identifier_does_not_decode() {
        assert!(
            serde_json::from_str::<ElicitStatus>(
                r#"{"expires_in_secs":1,"answers":{"ship":"yes please"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_status_is_complete_only_when_every_question_is_answered() {
        let mut status = ElicitStatus {
            expires_in_secs: 5,
            answers: BTreeMap::from([(qid("ship"), label("no"))]),
        };
        assert!(!status.complete(&questions()));
        status.answers.insert(qid("owner"), label("kernels"));
        assert!(status.complete(&questions()));
        assert_eq!(status.check(&questions()), Ok(()));
    }

    #[test]
    fn check_refuses_an_undeclared_question_an_undeclared_label_and_uncertain() {
        let cases = [
            (
                ("extra", "yes"),
                AnswerError::UndeclaredQuestion {
                    question: qid("extra"),
                },
            ),
            (
                ("owner", "frontend"),
                AnswerError::UndeclaredLabel {
                    question: qid("owner"),
                    label: label("frontend"),
                },
            ),
            (
                ("ship", "maybe"),
                AnswerError::UndeclaredLabel {
                    question: qid("ship"),
                    label: label("maybe"),
                },
            ),
            (
                ("ship", "uncertain"),
                AnswerError::Uncertain {
                    question: qid("ship"),
                },
            ),
        ];
        for ((q, l), want) in cases {
            let status = ElicitStatus {
                expires_in_secs: 1,
                answers: BTreeMap::from([(qid(q), label(l))]),
            };
            assert_eq!(status.check(&questions()), Err(want));
        }
    }

    #[test]
    fn the_message_offers_one_button_per_declared_label_and_no_input() {
        let message = slack_message(&ctx(), &questions(), &BTreeMap::new(), false);
        let blocks = message["blocks"].as_array().unwrap();
        let actions: Vec<&Value> = blocks.iter().filter(|b| b["type"] == "actions").collect();
        assert_eq!(actions.len(), 2);
        let labels = |block: &str| -> Vec<String> {
            actions.iter().find(|a| a["block_id"] == block).unwrap()["elements"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    assert_eq!(e["type"], "button");
                    assert_eq!(e["action_id"], e["value"]);
                    e["value"].as_str().unwrap().to_owned()
                })
                .collect()
        };
        assert_eq!(labels("ship"), ["yes", "no"]);
        assert_eq!(labels("owner"), ["serving", "kernels"]);
        let encoded = message.to_string();
        assert!(!encoded.contains("\"input\""));
        assert!(!encoded.contains("plain_text_input"));
        assert!(!encoded.contains("\"uncertain\""));
    }

    #[test]
    fn the_message_escapes_authored_text_and_carries_the_deadline_and_run_link() {
        let encoded = slack_message(&ctx(), &questions(), &BTreeMap::new(), false).to_string();
        assert!(
            encoded.contains("Ship &lt;this&gt; &amp; that?"),
            "{encoded}"
        );
        assert!(encoded.contains("the &lt;@here&gt; runtime"), "{encoded}");
        assert!(encoded.contains("nightly&lt;1&gt;"), "{encoded}");
        assert!(encoded.contains("<!date^1790000000^"), "{encoded}");
        assert!(
            encoded.contains(
                "<https://crucible.example/runs/nightly?a=1&amp;b=2|Open run in Crucible>"
            ),
            "{encoded}"
        );
        assert!(!encoded.contains("<@here>"), "{encoded}");
        assert!(
            !encoded.contains("nightly<1>"),
            "the fallback text is escaped too: {encoded}"
        );
    }

    #[test]
    fn an_answered_or_expired_question_loses_its_buttons() {
        let answered = BTreeMap::from([(
            qid("ship"),
            Answered {
                label: label("yes"),
                by: SlackId::new("U024BE7LH").unwrap(),
            },
        )]);
        let open = slack_message(&ctx(), &questions(), &answered, false);
        let encoded = open.to_string();
        assert!(
            encoded.contains("Answered *yes* by <@U024BE7LH>"),
            "{encoded}"
        );
        let actions: Vec<&Value> = open["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "actions")
            .collect();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0]["block_id"], "owner");

        let expired = slack_message(&ctx(), &questions(), &answered, true);
        let encoded = expired.to_string();
        assert!(!encoded.contains("\"actions\""), "{encoded}");
        assert!(encoded.contains("recorded as uncertain"), "{encoded}");
    }

    #[test]
    fn a_click_decodes_to_the_declared_label_it_names() {
        let got = decode_click(&click("owner", "kernels", "kernels"), &questions()).unwrap();
        assert_eq!(
            got,
            Click {
                user: SlackId::new("U024BE7LH").unwrap(),
                channel: SlackId::new("C123ABC456").unwrap(),
                message_ts: "1790000000.000100".into(),
                question: qid("owner"),
                label: label("kernels"),
            }
        );
    }

    #[test]
    fn a_click_that_names_anything_undeclared_or_is_not_one_button_is_refused() {
        let mut two = click("ship", "yes", "yes");
        two["actions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type": "button", "block_id": "ship", "action_id": "no", "value": "no"}));
        let mut typed = click("ship", "yes", "yes");
        typed["actions"][0]["type"] = json!("plain_text_input");
        let mut submission = click("ship", "yes", "yes");
        submission["type"] = json!("view_submission");
        let mut channelless = click("ship", "yes", "yes");
        channelless.as_object_mut().unwrap().remove("channel");
        let mut forged_user = click("ship", "yes", "yes");
        forged_user["user"]["id"] = json!("<@here>");
        let cases = [
            (click("extra", "yes", "yes"), "undeclared question"),
            (click("owner", "frontend", "frontend"), "undeclared label"),
            (click("ship", "uncertain", "uncertain"), "only the engine"),
            (click("ship", "yes", "no"), "different labels"),
            (
                click("ship", "yes please", "yes please"),
                "not an identifier",
            ),
            (two, "2 actions"),
            (typed, "not \"button\""),
            (submission, "not \"block_actions\""),
            (channelless, "no channel.id"),
            (forged_user, "not a Slack id"),
        ];
        for (payload, needle) in cases {
            let err = decode_click(&payload, &questions())
                .unwrap_err()
                .to_string();
            assert!(err.contains(needle), "{err} lacks {needle}");
        }
    }

    #[test]
    fn a_question_fits_slack_up_to_its_button_and_text_limits() {
        let many = |n: usize| Question {
            instructions: "which?".into(),
            kind: QuestionKind::Choice {
                options: (0..n)
                    .map(|i| ChoiceOption {
                        label: label(&format!("l{i}")),
                        description: None,
                    })
                    .collect(),
            },
            drop: vec![],
        };
        assert_eq!(fits_slack(&many(MAX_BUTTONS)), Ok(()));
        assert_eq!(
            fits_slack(&many(MAX_BUTTONS + 1)),
            Err(SlackLimit::TooManyLabels {
                got: MAX_BUTTONS + 1
            })
        );
        let wordy = |text: String| Question {
            instructions: text,
            kind: QuestionKind::Noul,
            drop: vec![],
        };
        assert_eq!(fits_slack(&wordy("x".repeat(MAX_TEXT_LEN - 2))), Ok(()));
        assert_eq!(
            fits_slack(&wordy("&".repeat(600))),
            Err(SlackLimit::TextTooLong { got: 3002 }),
            "escaping counts against the limit"
        );
        let mut described = many(2);
        if let QuestionKind::Choice { options } = &mut described.kind {
            options[0].description = Some("d".repeat(MAX_TEXT_LEN));
        }
        assert!(matches!(
            fits_slack(&described),
            Err(SlackLimit::TextTooLong { .. })
        ));
    }

    #[test]
    fn a_slack_id_is_short_upper_case_alphanumerics() {
        assert!(SlackId::new("U024BE7LH").is_ok());
        for bad in [
            "",
            "U",
            "u024be7lh",
            "U024 BE7",
            "U024BE7LH>",
            &"U".repeat(33),
        ] {
            assert!(SlackId::new(bad).is_err(), "{bad}");
        }
    }
}

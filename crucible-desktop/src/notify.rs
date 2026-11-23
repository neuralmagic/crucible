//! Desktop notifications for launches that finish, each carrying the link to its run so a click
//! opens it. macOS posts through the Notification Center when the app runs from its bundle and
//! through `osascript` otherwise (no clicks there); Linux posts through the freedesktop
//! notification service on the session bus. One notification stands for one launch: a later one
//! replaces it. A decision request that one click can answer carries a button for each answer.

use std::sync::mpsc::Sender;

use rune::Waker;

use crate::live::{Finished, LaunchStatus};

/// One notification: what it says and the run it opens.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    /// The launch it is about; a newer note for the same launch replaces it.
    pub id: String,
    pub title: String,
    pub body: String,
    /// The `crucible-playbooks://` link a click follows.
    pub link: String,
    /// The answers its buttons give, when one click can answer what it is about.
    pub answers: Option<Answers>,
}

/// A decision request asking one question that takes one of `options`.
#[derive(Clone, Debug, PartialEq)]
pub struct Answers {
    /// The profile whose controller asked.
    pub profile: String,
    pub decision: String,
    pub question: String,
    /// The evidence the request was read with.
    pub digest: String,
    pub options: Vec<String>,
}

/// What a click on a notification asks for.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
pub enum Clicked {
    /// Follow this link.
    Open(String),
    Answer(Answer),
}

/// The answer a notification's button gives.
#[derive(Clone, Debug, PartialEq)]
pub struct Answer {
    pub profile: String,
    pub decision: String,
    pub question: String,
    pub digest: String,
    pub label: String,
}

/// What a button's id starts with, before the label it answers with.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
const ANSWER: &str = "answer:";

#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
impl Note {
    /// The strings a click hands back.
    fn info(&self) -> Vec<(&'static str, String)> {
        let mut info = vec![("link", self.link.clone())];
        if let Some(answers) = &self.answers {
            info.extend([
                ("profile", answers.profile.clone()),
                ("decision", answers.decision.clone()),
                ("question", answers.question.clone()),
                ("digest", answers.digest.clone()),
            ]);
        }
        info
    }

    /// Each button's id and label.
    fn buttons(&self) -> Vec<(String, String)> {
        self.answers
            .iter()
            .flat_map(|answers| &answers.options)
            .map(|label| (format!("{ANSWER}{label}"), label.clone()))
            .collect()
    }
}

/// What a click on button `action`, or on the notification itself when `None`, asks for, read
/// from the strings the notification was posted with.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn clicked(action: Option<&str>, info: impl Fn(&str) -> Option<String>) -> Option<Clicked> {
    match action.and_then(|action| action.strip_prefix(ANSWER)) {
        Some(label) => Some(Clicked::Answer(Answer {
            profile: info("profile")?,
            decision: info("decision")?,
            question: info("question")?,
            digest: info("digest")?,
            label: label.to_string(),
        })),
        None => info("link")
            .filter(|link| !link.is_empty())
            .map(Clicked::Open),
    }
}

/// What a finished launch is announced with.
pub fn note(finished: &Finished) -> Note {
    let outcome = match (&finished.status, &finished.why) {
        (LaunchStatus::Done, _) => "Finished".to_string(),
        (LaunchStatus::Cancelled, Some(why)) => capitalized(why),
        (status, Some(why)) => format!("{} \u{2014} {why}", capitalized(status.as_str())),
        (status, None) => capitalized(status.as_str()),
    };
    let body = match finished.cost {
        Some(cost) if cost > 0.0 => format!("{outcome} \u{b7} ${cost:.2}"),
        _ => outcome,
    };
    Note {
        id: finished.key.clone(),
        title: finished.playbook.clone(),
        body,
        link: crate::link::Link::Launch {
            playbook: finished.playbook.clone(),
            key: finished.key.clone(),
            task: None,
        }
        .url(),
        answers: None,
    }
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Whether the app is in front, where a notification about what it shows would be noise.
pub fn app_focused() -> bool {
    rune_mac::notify::app_focused()
}

/// Bring the window forward after a click.
pub fn bring_to_front() {
    rune_mac::notify::bring_to_front();
}

/// Posts notes, and sends what each click on one asks for to `sink`, waking the app.
pub struct Notifier {
    #[cfg(target_os = "linux")]
    bus: Option<tokio::sync::mpsc::UnboundedSender<Note>>,
}

impl Notifier {
    pub fn start(sink: Sender<Clicked>, waker: Waker) -> Notifier {
        #[cfg(target_os = "macos")]
        {
            rune_mac::notify::on_click(move |click| {
                let asked = clicked(click.action(), |key| click.get(key).map(str::to_string));
                if let Some(asked) = asked {
                    let _ = sink.send(asked);
                    waker();
                }
            });
            Notifier {}
        }
        #[cfg(target_os = "linux")]
        {
            Notifier {
                bus: freedesktop::spawn(sink, waker),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (sink, waker);
            Notifier {}
        }
    }

    pub fn post(&self, note: &Note) {
        #[cfg(target_os = "macos")]
        {
            let mut posting =
                rune_mac::notify::Notification::new(&note.id, &note.title).body(&note.body);
            for (key, value) in note.info() {
                posting = posting.info(key, value);
            }
            for (id, label) in note.buttons() {
                posting = posting.action(id, label);
            }
            if !rune_mac::notify::post(&posting) {
                script(note);
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(bus) = &self.bus {
            let _ = bus.send(note.clone());
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let _ = note;
    }
}

/// An unbundled macOS build: `osascript` posts, but nothing comes back from a click.
#[cfg(target_os = "macos")]
fn script(note: &Note) {
    let script = format!(
        "display notification {} with title {}",
        quoted(&note.body),
        quoted(&note.title)
    );
    let _ = std::thread::Builder::new()
        .name("crucible-playbooks-notify".into())
        .spawn(move || {
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .status();
        });
}

/// An AppleScript string literal holding `text`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `org.freedesktop.Notifications` over the session bus, on a thread of its own.
#[cfg(any(target_os = "linux", test))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) mod freedesktop {
    use std::collections::HashMap;
    use std::sync::mpsc::Sender;

    use futures_util::StreamExt;
    use rune::Waker;
    use tokio::sync::mpsc;
    use zbus::Variant;
    use zbus::{Connection, MatchRule, MessageStream};

    use crate::notify::{Clicked, Note, clicked};

    const NAME: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";
    /// The action key a click on the notification itself invokes.
    const DEFAULT: &str = "default";

    /// The sender notes go to, or `None` when no thread could start.
    pub fn spawn(sink: Sender<Clicked>, waker: Waker) -> Option<mpsc::UnboundedSender<Note>> {
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("crucible-playbooks-notify".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let _ = runtime.block_on(serve(rx, sink, waker));
            })
            .ok()?;
        Some(tx)
    }

    /// Post each note, replacing its launch's last one, and pass on what each click asks for.
    pub async fn serve(
        mut notes: mpsc::UnboundedReceiver<Note>,
        sink: Sender<Clicked>,
        waker: Waker,
    ) -> zbus::Result<()> {
        let bus = Connection::session().await?;
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(NAME)
            .member("ActionInvoked")
            .build()?;
        let mut actions = MessageStream::for_match_rule(rule, &bus, None).await?;
        let mut posted: HashMap<u32, Note> = HashMap::new();
        let mut shown: HashMap<String, u32> = HashMap::new();
        loop {
            tokio::select! {
                note = notes.recv() => {
                    let Some(note) = note else { return Ok(()) };
                    let replaces = shown.get(&note.id).copied().unwrap_or(0);
                    if let Ok(id) = notify(&bus, &note, replaces).await {
                        shown.insert(note.id.clone(), id);
                        posted.insert(id, note);
                    }
                }
                Some(Ok(signal)) = actions.next() => {
                    let Ok((id, action)) = signal.body().deserialize::<(u32, String)>() else {
                        continue;
                    };
                    if let Some(note) = posted.get(&id)
                        && let Some(asked) = asked(note, &action)
                    {
                        let _ = sink.send(asked);
                        waker();
                    }
                }
            }
        }
    }

    /// What invoking `action` on `note` asks for; `default` is a click on the notification.
    pub fn asked(note: &Note, action: &str) -> Option<Clicked> {
        let info = note.info();
        clicked((action != DEFAULT).then_some(action), |key| {
            info.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
        })
    }

    /// The action keys and labels `Notify` takes, flattened: the default action, then the buttons.
    pub fn actions(note: &Note) -> Vec<String> {
        let mut actions = vec![DEFAULT.to_string(), "Open".to_string()];
        for (id, label) in note.buttons() {
            actions.extend([id, label]);
        }
        actions
    }

    /// `Notify`, returning the id the service gave the notification.
    pub async fn notify(bus: &Connection, note: &Note, replaces: u32) -> zbus::Result<u32> {
        let hints: HashMap<&str, Variant<'_>> = HashMap::new();
        let reply = bus
            .call_method(
                Some(NAME),
                PATH,
                Some(NAME),
                "Notify",
                &(
                    "Crucible Playbooks",
                    replaces,
                    "",
                    note.title.as_str(),
                    note.body.as_str(),
                    actions(note),
                    hints,
                    -1i32,
                ),
            )
            .await?;
        reply.body().deserialize::<u32>()
    }
}

#[cfg(test)]
mod tests {
    use crate::link::Link;
    use crate::live::{Finished, LaunchStatus};
    use crate::notify::freedesktop::{actions, asked};
    use crate::notify::{Answer, Answers, Clicked, Note, clicked, note, quoted};

    fn finished(status: &str, why: Option<&str>, cost: Option<f64>) -> Finished {
        Finished {
            seq: 1,
            key: "playbook:vllm-agent-fanout:01a1".into(),
            playbook: "vllm-agent-fanout".into(),
            status: LaunchStatus::parse(status, why),
            by: "weaton".into(),
            why: why.map(Into::into),
            cost,
        }
    }

    #[test]
    fn a_cancelled_launch_says_who_cancelled_it() {
        let cancelled = finished("parked", Some("cancelled by weaton"), None);
        assert_eq!(note(&cancelled).body, "Cancelled by weaton");
    }

    #[test]
    fn a_finished_launch_reads_as_its_playbook_outcome_and_spend_and_links_its_run() {
        let done = note(&finished("done", None, Some(1.234)));
        assert_eq!(done.title, "vllm-agent-fanout");
        assert_eq!(done.body, "Finished \u{b7} $1.23");
        assert_eq!(done.id, "playbook:vllm-agent-fanout:01a1");
        assert_eq!(
            Link::parse(&done.link),
            Some(Link::Launch {
                playbook: "vllm-agent-fanout".into(),
                key: "playbook:vllm-agent-fanout:01a1".into(),
                task: None,
            })
        );
        assert_eq!(
            note(&finished("error", Some("budget ran out"), None)).body,
            "Error \u{2014} budget ran out"
        );
        assert_eq!(
            note(&finished("cancelled", None, Some(0.0))).body,
            "Cancelled"
        );
    }

    fn asking() -> Note {
        Note {
            id: "d-7".into(),
            title: "Waiting on you \u{b7} review".into(),
            body: "ship".into(),
            link: Link::Decision { id: "d-7".into() }.url(),
            answers: Some(Answers {
                profile: "mpp".into(),
                decision: "d-7".into(),
                question: "ship".into(),
                digest: "sha256:e".into(),
                options: vec!["yes".into(), "no thanks".into()],
            }),
        }
    }

    #[test]
    fn a_button_click_is_the_answer_its_note_was_posted_with() {
        let note = asking();
        assert_eq!(
            actions(&note),
            [
                "default",
                "Open",
                "answer:yes",
                "yes",
                "answer:no thanks",
                "no thanks"
            ]
        );
        assert_eq!(
            asked(&note, "answer:no thanks"),
            Some(Clicked::Answer(Answer {
                profile: "mpp".into(),
                decision: "d-7".into(),
                question: "ship".into(),
                digest: "sha256:e".into(),
                label: "no thanks".into(),
            }))
        );
    }

    #[test]
    fn a_click_on_the_note_itself_opens_its_link() {
        let note = asking();
        assert_eq!(
            asked(&note, "default"),
            Some(Clicked::Open(note.link.clone()))
        );
        assert_eq!(
            asked(&note, "dismiss"),
            Some(Clicked::Open(note.link.clone())),
            "an action that is no answer"
        );
        let nowhere = Note {
            link: String::new(),
            ..note
        };
        assert_eq!(asked(&nowhere, "default"), None);
    }

    #[test]
    fn a_note_without_answers_has_no_buttons_and_answers_nothing() {
        let plain = Note {
            answers: None,
            ..asking()
        };
        assert_eq!(actions(&plain), ["default", "Open"]);
        assert_eq!(asked(&plain, "answer:yes"), None);
    }

    #[test]
    fn an_answer_missing_any_of_what_it_was_posted_with_is_dropped() {
        let posted = [
            ("link", "crucible-playbooks://decision/d-7"),
            ("profile", "mpp"),
            ("decision", "d-7"),
            ("question", "ship"),
            ("digest", "sha256:e"),
        ];
        let without = |missing: &'static str| {
            move |key: &str| {
                posted
                    .iter()
                    .find(|(name, _)| *name == key && *name != missing)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert!(matches!(
            clicked(Some("answer:yes"), without("")),
            Some(Clicked::Answer(answer)) if answer.label == "yes"
        ));
        for missing in ["profile", "decision", "question", "digest"] {
            assert_eq!(
                clicked(Some("answer:yes"), without(missing)),
                None,
                "{missing}"
            );
        }
        assert_eq!(clicked(None, without("link")), None);
    }

    #[test]
    fn script_strings_escape_quotes_and_backslashes() {
        assert_eq!(quoted(r#"say "hi" \o/"#), r#""say \"hi\" \\o/""#);
    }
}

//! Desktop notifications for launches that finish, each carrying the link to its run so a click
//! opens it. macOS posts through the Notification Center when the app runs from its bundle and
//! through `osascript` otherwise (no clicks there); Linux posts through the freedesktop
//! notification service on the session bus. One notification stands for one launch: a later one
//! replaces it.

use std::sync::mpsc::Sender;

use rune::Waker;

use crate::live::Finished;

/// One notification: what it says and the run it opens.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    /// The launch it is about; a newer note for the same launch replaces it.
    pub id: String,
    pub title: String,
    pub body: String,
    /// The `crucible-playbooks://` link a click follows.
    pub link: String,
}

/// What a finished launch is announced with.
pub fn note(finished: &Finished) -> Note {
    let outcome = match (finished.status.as_str(), &finished.why) {
        ("done", _) => "Finished".to_string(),
        (status, Some(why)) => format!("{} \u{2014} {why}", capitalized(status)),
        (status, None) => capitalized(status),
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

/// Posts notes, and sends the link of each one clicked to `clicked`, waking the app.
pub struct Notifier {
    #[cfg(target_os = "linux")]
    bus: Option<tokio::sync::mpsc::UnboundedSender<Note>>,
}

impl Notifier {
    pub fn start(clicked: Sender<String>, waker: Waker) -> Notifier {
        #[cfg(target_os = "macos")]
        {
            rune_mac::notify::on_click(move |click| {
                if let Some(link) = click.get("link") {
                    let _ = clicked.send(link.to_string());
                    waker();
                }
            });
            Notifier {}
        }
        #[cfg(target_os = "linux")]
        {
            Notifier {
                bus: freedesktop::spawn(clicked, waker),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (clicked, waker);
            Notifier {}
        }
    }

    pub fn post(&self, note: &Note) {
        #[cfg(target_os = "macos")]
        {
            let posted = rune_mac::notify::post(
                &rune_mac::notify::Notification::new(&note.id, &note.title)
                    .body(&note.body)
                    .info("link", &note.link),
            );
            if !posted {
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
mod freedesktop {
    use std::collections::HashMap;
    use std::sync::mpsc::Sender;

    use futures_util::StreamExt;
    use rune::Waker;
    use tokio::sync::mpsc;
    use zbus::Variant;
    use zbus::{Connection, MatchRule, MessageStream};

    use crate::notify::Note;

    const NAME: &str = "org.freedesktop.Notifications";
    const PATH: &str = "/org/freedesktop/Notifications";

    /// The sender notes go to, or `None` when no thread could start.
    pub fn spawn(clicked: Sender<String>, waker: Waker) -> Option<mpsc::UnboundedSender<Note>> {
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
                let _ = runtime.block_on(serve(rx, clicked, waker));
            })
            .ok()?;
        Some(tx)
    }

    /// Post each note, replacing its launch's last one, and pass on default-action clicks.
    pub async fn serve(
        mut notes: mpsc::UnboundedReceiver<Note>,
        clicked: Sender<String>,
        waker: Waker,
    ) -> zbus::Result<()> {
        let bus = Connection::session().await?;
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(NAME)
            .member("ActionInvoked")
            .build()?;
        let mut actions = MessageStream::for_match_rule(rule, &bus, None).await?;
        let mut links: HashMap<u32, String> = HashMap::new();
        let mut shown: HashMap<String, u32> = HashMap::new();
        loop {
            tokio::select! {
                note = notes.recv() => {
                    let Some(note) = note else { return Ok(()) };
                    let replaces = shown.get(&note.id).copied().unwrap_or(0);
                    if let Ok(id) = notify(&bus, &note, replaces).await {
                        shown.insert(note.id.clone(), id);
                        links.insert(id, note.link.clone());
                    }
                }
                Some(Ok(signal)) = actions.next() => {
                    let Ok((id, action)) = signal.body().deserialize::<(u32, String)>() else {
                        continue;
                    };
                    if action == "default"
                        && let Some(link) = links.get(&id)
                    {
                        let _ = clicked.send(link.clone());
                        waker();
                    }
                }
            }
        }
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
                    vec!["default", "Open"],
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
    use crate::live::Finished;
    use crate::notify::{note, quoted};

    fn finished(status: &str, why: Option<&str>, cost: Option<f64>) -> Finished {
        Finished {
            seq: 1,
            key: "playbook:vllm-agent-fanout:01a1".into(),
            playbook: "vllm-agent-fanout".into(),
            status: status.into(),
            by: "weaton".into(),
            why: why.map(Into::into),
            cost,
        }
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

    #[test]
    fn script_strings_escape_quotes_and_backslashes() {
        assert_eq!(quoted(r#"say "hi" \o/"#), r#""say \"hi\" \\o/""#);
    }
}

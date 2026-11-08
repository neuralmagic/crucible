//! Desktop notifications for runs that finish. macOS posts them through `osascript` on a
//! short-lived thread, so the window never waits; other platforms show none.

use crate::live::Finished;

/// The title and body a finished launch is announced with.
pub fn message(finished: &Finished) -> (String, String) {
    let body = match (finished.status.as_str(), &finished.why) {
        ("done", _) => "Finished".to_string(),
        (status, Some(why)) => format!("{} \u{2014} {why}", capitalized(status)),
        (status, None) => capitalized(status),
    };
    (finished.playbook.clone(), body)
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// An AppleScript string literal holding `text`.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn post(title: &str, body: &str) {
    let script = format!(
        "display notification {} with title {}",
        quoted(body),
        quoted(title)
    );
    if cfg!(target_os = "macos") {
        let _ = std::thread::Builder::new()
            .name("crucible-playbooks-notify".into())
            .spawn(move || {
                let _ = std::process::Command::new("osascript")
                    .arg("-e")
                    .arg(script)
                    .status();
            });
    }
}

#[cfg(test)]
mod tests {
    use crate::live::Finished;
    use crate::notify::{message, quoted};

    fn finished(status: &str, why: Option<&str>) -> Finished {
        Finished {
            seq: 1,
            playbook: "vllm-agent-fanout".into(),
            status: status.into(),
            by: "weaton".into(),
            why: why.map(Into::into),
        }
    }

    #[test]
    fn a_finished_launch_reads_as_its_playbook_and_outcome() {
        assert_eq!(
            message(&finished("done", None)),
            ("vllm-agent-fanout".into(), "Finished".into())
        );
        assert_eq!(
            message(&finished("error", Some("budget ran out"))).1,
            "Error \u{2014} budget ran out"
        );
        assert_eq!(message(&finished("cancelled", None)).1, "Cancelled");
    }

    #[test]
    fn script_strings_escape_quotes_and_backslashes() {
        assert_eq!(quoted(r#"say "hi" \o/"#), r#""say \"hi\" \\o/""#);
    }
}

//! Whether a release newer than the running app exists, asked of GitHub once a day.

use std::cmp::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use rune::time::Instant;
use serde::Deserialize;

use crate::App;

pub const LATEST: &str = "https://api.github.com/repos/neuralmagic/crucible/releases/latest";
const NOTES: &str = "https://github.com/neuralmagic/crucible/releases/tag";
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const PATIENCE: Duration = Duration::from_secs(15);

#[derive(Default)]
pub enum Check {
    #[default]
    Unasked,
    Asking(Receiver<Result<String, String>>),
    Known {
        latest: Result<String, String>,
        at: Instant,
    },
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// The newest release's tag, as the releases API at `url` names it.
pub async fn latest(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .user_agent("crucible-playbooks")
        .build()
        .map_err(|e| format!("building the client: {e}"))?;
    let ask = async {
        let response = client
            .get(url)
            .header("accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| format!("asking for the latest release: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("asking for the latest release: {status}"));
        }
        let body = response
            .text()
            .await
            .map_err(|e| format!("reading the latest release: {e}"))?;
        serde_json::from_str::<Release>(&body)
            .map(|release| release.tag_name)
            .map_err(|e| format!("reading the latest release: {e}"))
    };
    crate::net::timeout(PATIENCE, ask)
        .await
        .unwrap_or_else(|_| Err("asking for the latest release timed out".to_string()))
}

impl Check {
    /// What the check has to say beside app version `app`, and the newer release's tag when
    /// there is one.
    pub fn line(&self, app: &str) -> (String, Option<&str>) {
        match self {
            Check::Unasked => ("Not checked".to_string(), None),
            Check::Asking(_) => ("Checking\u{2026}".to_string(), None),
            Check::Known {
                latest: Err(why), ..
            } => (why.clone(), None),
            Check::Known {
                latest: Ok(tag), ..
            } => {
                if crate::version::beside(tag, app) == Some(Ordering::Greater) {
                    (format!("{tag} available"), Some(tag.as_str()))
                } else {
                    ("Up to date".to_string(), None)
                }
            }
        }
    }
}

/// Where a release's notes are.
pub fn notes(tag: &str) -> String {
    format!("{NOTES}/{tag}")
}

impl App {
    /// Ask for the latest release, when the user has not turned the check off.
    pub fn check_release(&mut self) {
        if cfg!(target_arch = "wasm32") || !self.settings.updates {
            return;
        }
        if matches!(self.release, Check::Asking(_)) {
            return;
        }
        let Ok(runtime) = self.net.handle() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let waker = self.waker.clone();
        runtime.spawn(async move {
            let _ = tx.send(latest(LATEST).await);
            if let Some(waker) = waker {
                waker();
            }
        });
        self.release = Check::Asking(rx);
    }

    /// Take the answer when it has come, and ask again once the last one is a day old.
    pub(crate) fn finish_release_check(&mut self) -> bool {
        match &self.release {
            Check::Asking(rx) => {
                let latest = match rx.try_recv() {
                    Ok(latest) => latest,
                    Err(TryRecvError::Empty) => return false,
                    Err(TryRecvError::Disconnected) => Err("the check stopped".to_string()),
                };
                self.release = Check::Known {
                    latest,
                    at: Instant::now(),
                };
                true
            }
            Check::Known { at, .. } if at.elapsed() >= EVERY => {
                self.check_release();
                false
            }
            Check::Known { .. } | Check::Unasked => false,
        }
    }

    /// The tag of a release newer than the running app, when the last check found one.
    pub fn update_available(&self) -> Option<&str> {
        let Check::Known {
            latest: Ok(tag), ..
        } = &self.release
        else {
            return None;
        };
        (crate::version::beside(tag, crate::version::APP) == Some(Ordering::Greater))
            .then_some(tag.as_str())
    }

    pub fn release(&self) -> &Check {
        &self.release
    }
}

#[cfg(test)]
mod tests {
    use crate::testserver::{Reply, serve_replies};
    use rune::time::Instant;

    use crate::update::{Check, latest, notes};

    fn ask(url: &str) -> Result<String, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(latest(url))
    }

    #[test]
    fn the_latest_release_is_its_tag_and_the_request_names_the_app() {
        let (url, _) = serve_replies(|asked| {
            assert_eq!(asked.line, "GET /releases/latest HTTP/1.1");
            assert_eq!(asked.header("user-agent"), Some("crucible-playbooks"));
            Reply::ok(r#"{"tag_name":"v0.6.3","name":"0.6.3","draft":false}"#.into())
        });
        assert_eq!(ask(&format!("{url}/releases/latest")), Ok("v0.6.3".into()));
        assert_eq!(
            notes("v0.6.3"),
            "https://github.com/neuralmagic/crucible/releases/tag/v0.6.3"
        );
    }

    #[test]
    fn the_check_says_what_it_knows_and_names_only_a_newer_release() {
        let known = |latest: Result<&str, &str>| Check::Known {
            latest: latest.map(str::to_string).map_err(str::to_string),
            at: Instant::now(),
        };
        assert_eq!(
            known(Ok("v0.6.3")).line("0.6.2"),
            ("v0.6.3 available".to_string(), Some("v0.6.3"))
        );
        assert_eq!(known(Ok("v0.6.2")).line("0.6.2").0, "Up to date");
        assert_eq!(
            known(Ok("v0.6.1")).line("0.6.2"),
            ("Up to date".to_string(), None)
        );
        assert_eq!(
            known(Err("offline")).line("0.6.2"),
            ("offline".to_string(), None)
        );
        assert_eq!(Check::Unasked.line("0.6.2").0, "Not checked");
        let (_tx, rx) = std::sync::mpsc::channel();
        assert_eq!(Check::Asking(rx).line("0.6.2").0, "Checking\u{2026}");
    }

    #[test]
    fn a_refusal_or_an_unreadable_answer_is_an_error_not_a_release() {
        let (url, _) = serve_replies(|asked| {
            if asked.line.contains("limited") {
                Reply {
                    status: 403,
                    headers: Vec::new(),
                    body: r#"{"message":"rate limit exceeded"}"#.into(),
                }
            } else {
                Reply::ok(r#"{"message":"no tag here"}"#.into())
            }
        });
        let refused = ask(&format!("{url}/limited")).unwrap_err();
        assert!(refused.contains("403"), "{refused}");
        let unreadable = ask(&format!("{url}/odd")).unwrap_err();
        assert!(
            unreadable.contains("reading the latest release"),
            "{unreadable}"
        );
    }
}

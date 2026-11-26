//! Signing in to GitHub and keeping the account linked to the profile in use.

use std::sync::mpsc::{Sender, channel};

use crate::App;
use crate::github::{self, Event, Tokens};
use crate::profile::{Readiness, Target};

const RETRY_SECS: i64 = 300;
const RELINK_SECS: i64 = 6 * 3600;

fn send(tx: &Sender<Event>, waker: &Option<rune::Waker>, event: Event) {
    let _ = tx.send(event);
    if let Some(waker) = waker {
        waker();
    }
}

fn link_now(
    runtime: &crate::net::Handle,
    target: &Target,
    keys: &crate::keys::Store,
    client_id: &str,
    tokens: Tokens,
) -> (Result<String, String>, Option<i64>) {
    let renewed = match github::current(runtime, client_id, tokens.clone()) {
        Ok(renewed) => renewed,
        Err(why) => return (Err(why), None),
    };
    if renewed != tokens
        && let Err(why) = keys.save_github(&renewed)
    {
        return (Err(why), None);
    }
    let renew_at = renewed.renew_at();
    (github::link(runtime, target, &renewed), renew_at)
}

impl App {
    pub fn github_sign_in(&mut self) {
        if self.github.busy {
            return;
        }
        let client_id = self.settings.github_client_id.clone();
        if client_id.is_empty() {
            self.github.error = Some("set the GitHub App client ID first".into());
            return;
        }
        let runtime = match self.net.handle() {
            Ok(runtime) => runtime,
            Err(why) => {
                self.github.error = Some(why);
                return;
            }
        };
        let target = self.remote.target.clone();
        let keys = self.settings.keys();
        let waker = self.waker.clone();
        let (tx, rx) = channel();
        let (cancel, stop) = crate::signin::Cancel::new();
        self.github.busy = true;
        self.github.error = None;
        self.github.code = None;
        self.github.events = Some(rx);
        self.github.cancel = Some(cancel);
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-github-sign-in".into())
            .spawn(move || {
                let code = match github::start(&runtime, &client_id) {
                    Ok(code) => code,
                    Err(why) => return send(&tx, &waker, Event::SignedIn(Err(why))),
                };
                let _ = crate::editor::open_url(&code.verification_uri);
                send(&tx, &waker, Event::Code(code.clone()));
                let tokens = github::finish(&runtime, &client_id, &code, &stop)
                    .and_then(|tokens| keys.save_github(&tokens).map(|()| tokens));
                let signed_in = tokens.clone();
                send(&tx, &waker, Event::SignedIn(signed_in));
                if let Ok(tokens) = tokens {
                    let (answer, renew_at) = link_now(&runtime, &target, &keys, &client_id, tokens);
                    send(
                        &tx,
                        &waker,
                        Event::Linked {
                            profile: target.name.clone(),
                            answer,
                            renew_at,
                        },
                    );
                }
            });
        if let Err(e) = spawned {
            self.github_cancel();
            self.github.error = Some(format!("starting the GitHub sign-in: {e}"));
        }
    }

    pub fn github_cancel(&mut self) {
        self.github.busy = false;
        self.github.code = None;
        self.github.events = None;
        self.github.cancel = None;
    }

    pub fn github_sign_out(&mut self) {
        self.github_cancel();
        if let Err(why) = self.settings.keys().forget_github() {
            self.github.error = Some(why);
            return;
        }
        let linked = self.github.linked.take();
        self.github.login = None;
        self.github.next = None;
        self.github.error = None;
        let Some((profile, _)) = linked else {
            return;
        };
        if profile != self.remote.target.name {
            return;
        }
        let Ok(runtime) = self.net.handle() else {
            return;
        };
        let target = self.remote.target.clone();
        let _ = std::thread::Builder::new()
            .name("crucible-playbooks-github-unlink".into())
            .spawn(move || {
                let _ = github::unlink(&runtime, &target);
            });
    }

    fn github_link(&mut self) {
        let Ok(Some(tokens)) = self.settings.keys().github() else {
            self.github.login = None;
            return;
        };
        let runtime = match self.net.handle() {
            Ok(runtime) => runtime,
            Err(why) => {
                self.github.error = Some(why);
                return;
            }
        };
        let client_id = self.settings.github_client_id.clone();
        let target = self.remote.target.clone();
        let keys = self.settings.keys();
        let waker = self.waker.clone();
        let (tx, rx) = channel();
        self.github.busy = true;
        self.github.events = Some(rx);
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-github-link".into())
            .spawn(move || {
                let (answer, renew_at) = link_now(&runtime, &target, &keys, &client_id, tokens);
                send(
                    &tx,
                    &waker,
                    Event::Linked {
                        profile: target.name.clone(),
                        answer,
                        renew_at,
                    },
                );
            });
        if spawned.is_err() {
            self.github.busy = false;
            self.github.events = None;
        }
    }

    fn github_heard(&mut self, event: Event) {
        let now = github::unix_now();
        match event {
            Event::Code(code) => self.github.code = Some(code),
            Event::SignedIn(Ok(tokens)) => {
                self.github.code = None;
                self.github.cancel = None;
                self.github.login = Some(tokens.login);
            }
            Event::SignedIn(Err(why)) => {
                self.github_cancel();
                self.github.error = Some(why);
            }
            Event::Linked {
                profile,
                answer,
                renew_at,
            } => {
                self.github.busy = false;
                self.github.events = None;
                match answer {
                    Ok(login) => {
                        self.github.error = None;
                        self.github.linked = Some((profile.clone(), login));
                        let at = renew_at.unwrap_or(now + RELINK_SECS);
                        self.github.next = Some((profile, at.max(now + 60)));
                    }
                    Err(why) => {
                        self.github.error = Some(why);
                        self.github.next = Some((profile, now + RETRY_SECS));
                    }
                }
            }
        }
    }

    pub(crate) fn follow_github(&mut self) -> bool {
        let mut changed = false;
        if !self.github.loaded {
            self.github.loaded = true;
            self.github.login = self
                .settings
                .keys()
                .github()
                .ok()
                .flatten()
                .map(|tokens| tokens.login);
            changed = self.github.login.is_some();
        }
        while let Some(event) = self
            .github
            .events
            .as_ref()
            .and_then(|events| events.try_recv().ok())
        {
            self.github_heard(event);
            changed = true;
        }
        let ready = self.remote.target.readiness() == Readiness::Ready;
        if ready
            && self
                .github
                .due(&self.remote.target.name, github::unix_now())
        {
            self.github_link();
            changed = true;
        }
        changed
    }
}

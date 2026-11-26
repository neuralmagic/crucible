//! Signing in to forges (GitHub, GitLab hosts) and keeping each account linked to the profile in
//! use.

use std::sync::mpsc::{Sender, channel};

use crate::App;
use crate::forge::Account;
use crate::forge::device::{self, Tokens};
use crate::forge::link;
use crate::forge::session::{Event, Session};
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
    account: &Account,
    client_id: &str,
    tokens: Tokens,
) -> (Result<String, String>, Option<i64>) {
    let renewed = match device::current(runtime, account, client_id, tokens.clone()) {
        Ok(renewed) => renewed,
        Err(why) => return (Err(why), None),
    };
    if renewed != tokens
        && let Err(why) = keys.save_forge_tokens(account, &renewed)
    {
        return (Err(why), None);
    }
    let renew_at = renewed.renew_at();
    (link::link(runtime, target, account, &renewed), renew_at)
}

impl App {
    fn forge_session(&mut self, account: &Account) -> &mut Session {
        self.forge.entry(account.clone()).or_default()
    }

    pub fn forge_sign_in(&mut self, account: Account) {
        if self.forge_session(&account).busy {
            return;
        }
        let client_id = self.settings.client_id(&account);
        if client_id.is_empty() {
            self.forge_session(&account).error = Some("set the client ID first".into());
            return;
        }
        let runtime = match self.net.handle() {
            Ok(runtime) => runtime,
            Err(why) => {
                self.forge_session(&account).error = Some(why);
                return;
            }
        };
        let target = self.remote.target.clone();
        let keys = self.settings.keys();
        let waker = self.waker.clone();
        let (tx, rx) = channel();
        let (cancel, stop) = crate::signin::Cancel::new();
        let session = self.forge_session(&account);
        session.busy = true;
        session.error = None;
        session.code = None;
        session.events = Some(rx);
        session.cancel = Some(cancel);
        let thread_account = account.clone();
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-forge-sign-in".into())
            .spawn(move || {
                let account = thread_account;
                let code = match device::start(&runtime, &account, &client_id) {
                    Ok(code) => code,
                    Err(why) => return send(&tx, &waker, Event::SignedIn(Err(why))),
                };
                let _ = crate::editor::open_url(code.page());
                send(&tx, &waker, Event::Code(code.clone()));
                let tokens = device::finish(&runtime, &account, &client_id, &code, &stop)
                    .and_then(|tokens| keys.save_forge_tokens(&account, &tokens).map(|()| tokens));
                send(&tx, &waker, Event::SignedIn(tokens.clone()));
                if let Ok(tokens) = tokens {
                    let (answer, renew_at) =
                        link_now(&runtime, &target, &keys, &account, &client_id, tokens);
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
            self.forge_cancel(account.clone());
            self.forge_session(&account).error = Some(format!("starting the sign-in: {e}"));
        }
    }

    pub fn forge_cancel(&mut self, account: Account) {
        let session = self.forge_session(&account);
        session.busy = false;
        session.code = None;
        session.events = None;
        session.cancel = None;
    }

    pub fn forge_sign_out(&mut self, account: Account) {
        self.forge_cancel(account.clone());
        if let Err(why) = self.settings.keys().forget_forge_tokens(&account) {
            self.forge_session(&account).error = Some(why);
            return;
        }
        let session = self.forge_session(&account);
        let linked = session.linked.take();
        session.login = None;
        session.next = None;
        session.error = None;
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
            .name("crucible-playbooks-forge-unlink".into())
            .spawn(move || {
                let _ = link::unlink(&runtime, &target, &account);
            });
    }

    pub fn add_gitlab_host(&mut self) {
        let Some(host) = crate::forge::gitlab_host(&self.gitlab_host.text) else {
            return;
        };
        let client_id = self.gitlab_client_id.text.trim().to_string();
        if client_id.is_empty() {
            return;
        }
        self.settings.set_gitlab_app(&host, &client_id);
        self.gitlab_host = rune::TextEdit::new("");
        self.gitlab_client_id = rune::TextEdit::new("");
    }

    pub fn remove_gitlab_host(&mut self, host: String) {
        let account = Account::Gitlab(host.clone());
        self.forge_sign_out(account.clone());
        self.forge.remove(&account);
        self.settings.remove_gitlab_app(&host);
    }

    fn forge_link(&mut self, account: &Account) {
        let tokens = match self.settings.keys().forge_tokens(account) {
            Ok(Some(tokens)) => tokens,
            Ok(None) => {
                self.forge_session(account).login = None;
                return;
            }
            Err(why) => {
                self.forge_session(account).error = Some(why);
                return;
            }
        };
        let runtime = match self.net.handle() {
            Ok(runtime) => runtime,
            Err(why) => {
                self.forge_session(account).error = Some(why);
                return;
            }
        };
        let client_id = self.settings.client_id(account);
        let target = self.remote.target.clone();
        let keys = self.settings.keys();
        let waker = self.waker.clone();
        let (tx, rx) = channel();
        let session = self.forge_session(account);
        session.busy = true;
        session.events = Some(rx);
        let thread_account = account.clone();
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-forge-link".into())
            .spawn(move || {
                let (answer, renew_at) = link_now(
                    &runtime,
                    &target,
                    &keys,
                    &thread_account,
                    &client_id,
                    tokens,
                );
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
            let session = self.forge_session(account);
            session.busy = false;
            session.events = None;
        }
    }

    fn forge_heard(&mut self, account: &Account, event: Event) {
        let now = device::unix_now();
        match event {
            Event::Code(code) => self.forge_session(account).code = Some(code),
            Event::SignedIn(Ok(tokens)) => {
                let session = self.forge_session(account);
                session.code = None;
                session.cancel = None;
                session.login = Some(tokens.login);
            }
            Event::SignedIn(Err(why)) => {
                self.forge_cancel(account.clone());
                self.forge_session(account).error = Some(why);
            }
            Event::Linked {
                profile,
                answer,
                renew_at,
            } => {
                let session = self.forge_session(account);
                session.busy = false;
                session.events = None;
                match answer {
                    Ok(login) => {
                        session.error = None;
                        session.linked = Some((profile.clone(), login));
                        let at = renew_at.unwrap_or(now + RELINK_SECS);
                        session.next = Some((profile, at.max(now + 60)));
                    }
                    Err(why) => {
                        session.error = Some(why);
                        session.next = Some((profile, now + RETRY_SECS));
                    }
                }
            }
        }
    }

    pub(crate) fn follow_forge_accounts(&mut self) -> bool {
        let mut changed = false;
        let ready = self.remote.target.readiness() == Readiness::Ready;
        let profile = self.remote.target.name.clone();
        for account in self.settings.forge_accounts() {
            if !self.forge_session(&account).loaded {
                let login = self
                    .settings
                    .keys()
                    .forge_tokens(&account)
                    .ok()
                    .flatten()
                    .map(|tokens| tokens.login);
                let session = self.forge_session(&account);
                session.loaded = true;
                changed |= login.is_some();
                session.login = login;
            }
            while let Some(event) = self
                .forge
                .get(&account)
                .and_then(|session| session.events.as_ref())
                .and_then(|events| events.try_recv().ok())
            {
                self.forge_heard(&account, event);
                changed = true;
            }
            if ready
                && self
                    .forge_session(&account)
                    .due(&profile, device::unix_now())
            {
                self.forge_link(&account);
                changed = true;
            }
        }
        changed
    }
}

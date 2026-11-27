//! Signing in to forges (GitHub, GitLab hosts) and keeping each account linked to the profile in
//! use.

use std::sync::mpsc::{Sender, channel};

use crate::App;
use crate::forge::Account;
use crate::forge::apps::{self, Offered};
use crate::forge::device::{self, Tokens};
use crate::forge::link::{self, LinkError};
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
    client_id: Option<&str>,
    tokens: Tokens,
) -> (Result<String, LinkError>, Option<i64>) {
    let renewed = match device::current(runtime, account, client_id, tokens.clone()) {
        Ok(renewed) => renewed,
        Err(why) => return (Err(LinkError::Failed(why)), None),
    };
    if renewed != tokens
        && let Err(why) = keys.save_forge_tokens(account, &renewed)
    {
        return (Err(LinkError::Failed(why)), None);
    }
    let renew_at = renewed.renew_at();
    (link::link(runtime, target, account, &renewed), renew_at)
}

impl App {
    pub fn forge_offered(&self) -> &[Offered] {
        self.forge_offers.of(&self.remote.target.name)
    }

    pub fn forge_accounts(&self) -> Vec<Account> {
        let mut accounts = vec![Account::Github];
        let linked = self.settings.linked_forge_accounts().into_iter();
        let offered = self.forge_offered().iter().filter_map(Offered::account);
        for account in linked.chain(offered) {
            if !accounts.contains(&account) {
                accounts.push(account);
            }
        }
        accounts
    }

    pub fn forge_client_id(&self, account: &Account) -> Option<String> {
        self.forge_offered()
            .iter()
            .filter(|o| o.account().as_ref() == Some(account))
            .find_map(|o| o.client_id.clone())
    }

    fn follow_forge_apps(&mut self, ready: bool) -> bool {
        let now = device::unix_now();
        let mut changed = self.forge_offers.heard(now);
        let profile = self.remote.target.name.clone();
        if !ready || !self.forge_offers.due(&profile, now) {
            return changed;
        }
        let Ok(runtime) = self.net.handle() else {
            return changed;
        };
        let target = self.remote.target.clone();
        let waker = self.waker.clone();
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-forge-apps".into())
            .spawn(move || {
                let _ = tx.send(apps::offered(&runtime, &target));
                if let Some(waker) = waker {
                    waker();
                }
            });
        if spawned.is_ok() {
            self.forge_offers.asking(&profile, rx);
            changed = true;
        }
        changed
    }

    fn forge_session(&mut self, account: &Account) -> &mut Session {
        self.forge.entry(account.clone()).or_default()
    }

    pub fn forge_sign_in(&mut self, account: Account) {
        if self.forge_session(&account).busy {
            return;
        }
        let Some(client_id) = self.forge_client_id(&account) else {
            let why = format!(
                "{} offers no {} sign-in",
                self.remote.target.name,
                account.name()
            );
            self.forge_session(&account).error = Some(why);
            return;
        };
        let runtime = match self.net.handle() {
            Ok(runtime) => runtime,
            Err(why) => {
                self.forge_session(&account).error = Some(why);
                return;
            }
        };
        let target = self.remote.target.clone();
        self.settings.set_forge_linked(&target.name, &account, true);
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
                        link_now(&runtime, &target, &keys, &account, Some(&client_id), tokens);
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

    pub fn forge_link_here(&mut self, account: Account) {
        let profile = self.remote.target.name.clone();
        self.settings.set_forge_linked(&profile, &account, true);
        let session = self.forge_session(&account);
        session.next = None;
        session.error = None;
        if !session.busy {
            self.forge_link(&account);
        }
    }

    pub fn forge_sign_out(&mut self, account: Account) {
        self.forge_cancel(account.clone());
        let keys = self.settings.keys();
        let held = keys.forge_tokens(&account).ok().flatten();
        if let Err(why) = keys.forget_forge_tokens(&account) {
            self.forge_session(&account).error = Some(why);
            return;
        }
        let session = self.forge_session(&account);
        session.linked = None;
        session.login = None;
        session.next = None;
        session.error = None;
        let usable = crate::profile::usable(&self.settings);
        let mut targets = Vec::new();
        for profile in self.settings.forge_profiles(&account) {
            self.settings.set_forge_linked(&profile, &account, false);
            match profile == self.remote.target.name {
                true => targets.push(self.remote.target.clone()),
                false => {
                    if let Some(p) = usable.get(&profile) {
                        targets.push(crate::profile::Target::new(&profile, p.clone(), &keys));
                    }
                }
            }
        }
        let Ok(runtime) = self.net.handle() else {
            return;
        };
        let client_id = self.forge_client_id(&account);
        let _ = std::thread::Builder::new()
            .name("crucible-playbooks-forge-unlink".into())
            .spawn(move || {
                for target in &targets {
                    let _ = link::unlink(&runtime, target, &account);
                }
                if let Some(tokens) = held {
                    let _ = device::revoke(&runtime, &account, client_id.as_deref(), &tokens);
                }
            });
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
        let client_id = self.forge_client_id(account);
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
                    client_id.as_deref(),
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
                    Err(LinkError::Refused(why)) => {
                        session.error = Some(why);
                        session.hold(&profile);
                    }
                    Err(LinkError::Failed(why)) => {
                        session.error = Some(why);
                        session.next = Some((profile, now + RETRY_SECS));
                    }
                }
            }
        }
    }

    pub(crate) fn follow_forge_accounts(&mut self) -> bool {
        let ready = self.remote.target.readiness() == Readiness::Ready;
        let mut changed = self.follow_forge_apps(ready);
        let profile = self.remote.target.name.clone();
        for account in self.forge_accounts() {
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
                && self.settings.forge_linked(&profile, &account)
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

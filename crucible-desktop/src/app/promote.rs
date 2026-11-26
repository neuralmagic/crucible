//! Promoting the draft on screen: reading its promotion, the form for a new one, and sending it
//! with the chosen account's own token.

use std::sync::mpsc::{Receiver, channel};

use rune::TextEdit;

use crate::App;
use crate::forge::Account;
use crate::forge::device;
use crate::forge::promote::{self, Destination, Promotion};

const REREAD_SECS: i64 = 30;

/// What a promotion thread reports back.
pub enum Heard {
    Read(Result<Option<Promotion>, String>),
    Promoted(Result<Promotion, String>),
}

/// The promotion of the draft on screen, and the form for promoting it.
pub struct Promoting {
    pub draft: String,
    pub profile: String,
    pub account: Option<Account>,
    pub shown: Option<Promotion>,
    pub busy: bool,
    pub error: Option<String>,
    read_at: i64,
    heard: Option<Receiver<Heard>>,
}

impl Promoting {
    fn new(draft: &str, profile: &str) -> Self {
        Promoting {
            draft: draft.to_string(),
            profile: profile.to_string(),
            account: None,
            shown: None,
            busy: false,
            error: None,
            read_at: i64::MIN,
            heard: None,
        }
    }

    fn reread_due(&self, now: i64) -> bool {
        let followed = self.shown.as_ref().is_none_or(Promotion::followed);
        !self.busy
            && self.heard.is_none()
            && followed
            && now.saturating_sub(self.read_at) >= REREAD_SECS
    }
}

/// The three fields of the promote form.
pub struct PromoteForm {
    pub project: TextEdit,
    pub path: TextEdit,
    pub base: TextEdit,
}

impl Default for PromoteForm {
    fn default() -> Self {
        PromoteForm {
            project: TextEdit::new(""),
            path: TextEdit::new(""),
            base: TextEdit::new(""),
        }
    }
}

impl PromoteForm {
    pub fn destination(&self) -> Option<Destination> {
        let project = self.project.text.trim();
        if project.is_empty() {
            return None;
        }
        let base = self.base.text.trim();
        Some(Destination {
            project: project.to_string(),
            path: self.path.text.trim().trim_matches('/').to_string(),
            base: (!base.is_empty()).then(|| base.to_string()),
        })
    }
}

impl App {
    pub fn signed_in_accounts(&self) -> Vec<Account> {
        self.forge_accounts()
            .into_iter()
            .filter(|a| self.forge.get(a).is_some_and(|s| s.login.is_some()))
            .collect()
    }

    pub fn promote_with(&mut self, account: Account) {
        if let Some(promoting) = &mut self.promoting {
            promoting.account = Some(account);
        }
    }

    pub fn promote_draft(&mut self) {
        let Some(promoting) = &self.promoting else {
            return;
        };
        let (Some(account), Some(to)) =
            (promoting.account.clone(), self.promote_form.destination())
        else {
            return;
        };
        let draft = promoting.draft.clone();
        let runtime = match self.net.handle() {
            Ok(runtime) => runtime,
            Err(why) => return self.promotion_failed(why),
        };
        let client_id = self.forge_client_id(&account);
        let target = self.remote.target.clone();
        let keys = self.settings.keys();
        let waker = self.waker.clone();
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-promote".into())
            .spawn(move || {
                let promoted = keys
                    .forge_tokens(&account)
                    .and_then(|held| {
                        held.ok_or_else(|| format!("sign in to {} first", account.name()))
                    })
                    .and_then(|held| {
                        let fresh = device::current(&runtime, &account, &client_id, held.clone())?;
                        if fresh != held {
                            keys.save_forge_tokens(&account, &fresh)?;
                        }
                        Ok(fresh)
                    })
                    .and_then(|tokens| {
                        promote::promote(&runtime, &target, &account, &tokens, &draft, &to)
                    });
                let _ = tx.send(Heard::Promoted(promoted));
                if let Some(waker) = waker {
                    waker();
                }
            });
        let Some(promoting) = &mut self.promoting else {
            return;
        };
        match spawned {
            Ok(_) => {
                promoting.busy = true;
                promoting.error = None;
                promoting.heard = Some(rx);
            }
            Err(e) => promoting.error = Some(format!("starting the promotion: {e}")),
        }
    }

    fn promotion_failed(&mut self, why: String) {
        if let Some(promoting) = &mut self.promoting {
            promoting.error = Some(why);
        }
    }

    fn read_promotion(&mut self, now: i64) {
        let Some(promoting) = &mut self.promoting else {
            return;
        };
        let Ok(runtime) = self.net.handle() else {
            return;
        };
        let draft = promoting.draft.clone();
        let target = self.remote.target.clone();
        let waker = self.waker.clone();
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new()
            .name("crucible-playbooks-promotion".into())
            .spawn(move || {
                let _ = tx.send(Heard::Read(promote::promotion(&runtime, &target, &draft)));
                if let Some(waker) = waker {
                    waker();
                }
            });
        if spawned.is_ok() {
            promoting.read_at = now;
            promoting.heard = Some(rx);
        }
    }

    pub(crate) fn follow_promotion(&mut self) -> bool {
        let selected = self.selected_draft().map(|d| d.id.clone());
        let profile = self.remote.target.name.clone();
        let Some(draft) = selected else {
            let changed = self.promoting.is_some();
            self.promoting = None;
            return changed;
        };
        let stale = self
            .promoting
            .as_ref()
            .is_none_or(|p| p.draft != draft || p.profile != profile);
        if stale {
            let mut fresh = Promoting::new(&draft, &profile);
            fresh.account = self.signed_in_accounts().into_iter().next();
            self.promoting = Some(fresh);
            self.promote_form = PromoteForm::default();
        }
        let mut changed = stale;
        let heard = self
            .promoting
            .as_ref()
            .and_then(|p| p.heard.as_ref())
            .and_then(|rx| rx.try_recv().ok());
        let form = &mut self.promote_form;
        if let (Some(heard), Some(promoting)) = (heard, &mut self.promoting) {
            promoting.heard = None;
            changed = true;
            match heard {
                Heard::Read(Ok(shown)) => {
                    if let Some(shown) = &shown
                        && form.project.text.is_empty()
                    {
                        form.project = TextEdit::new(&shown.project);
                        form.path = TextEdit::new(&shown.path);
                        form.base = TextEdit::new(&shown.base);
                    }
                    promoting.shown = shown;
                }
                Heard::Read(Err(why)) => promoting.error = Some(why),
                Heard::Promoted(Ok(shown)) => {
                    promoting.busy = false;
                    promoting.shown = Some(shown);
                }
                Heard::Promoted(Err(why)) => {
                    promoting.busy = false;
                    promoting.error = Some(why);
                }
            }
        }
        let now = device::unix_now();
        let due = self.promoting.as_ref().is_some_and(|p| p.reread_due(now));
        if due && self.remote.target.readiness() == crate::profile::Readiness::Ready {
            self.read_promotion(now);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use crate::app::promote::*;

    #[test]
    fn a_destination_needs_a_project_and_trims_the_rest() {
        let mut p = PromoteForm::default();
        assert_eq!(p.destination(), None);
        p.project = TextEdit::new(" acme/packs ");
        p.path = TextEdit::new("/packs/triage/");
        assert_eq!(
            p.destination(),
            Some(Destination {
                project: "acme/packs".into(),
                path: "packs/triage".into(),
                base: None,
            })
        );
        p.base = TextEdit::new("release");
        assert_eq!(
            p.destination().and_then(|d| d.base).as_deref(),
            Some("release")
        );
    }

    #[test]
    fn a_promotion_is_reread_only_while_it_can_still_move() {
        let mut p = Promoting::new("triage", "local");
        assert!(p.reread_due(0));
        p.read_at = 100;
        assert!(!p.reread_due(100 + REREAD_SECS - 1));
        assert!(p.reread_due(100 + REREAD_SECS));
        p.shown = Some(Promotion {
            forge: "github".into(),
            host: "github.com".into(),
            project: "acme/packs".into(),
            path: String::new(),
            base: "main".into(),
            number: 1,
            url: "u".into(),
            version: 1,
            state: "imported".into(),
            note: String::new(),
            author: "wren".into(),
        });
        assert!(!p.reread_due(i64::MAX), "an imported promotion is settled");
        p.busy = true;
        p.shown = None;
        assert!(
            !p.reread_due(i64::MAX),
            "not while a promotion is in flight"
        );
    }
}

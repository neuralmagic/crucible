//! The management pages' picks and half-filled forms.

use crate::admin::InStatus;
use crate::admin::forms::{SecretForm, TestFireForm};
use crux::dto;
use rune::TextEdit;
use std::collections::{BTreeMap, HashSet};

/// The share roles, least first.
pub const ROLES: [&str; 3] = ["viewer", "launcher", "editor"];

/// The management pages' picks and half-filled forms.
pub struct Desk {
    /// The secret the Secrets page shows.
    pub secret: Option<String>,
    /// The next value of that secret, while its rotate form is open.
    pub rotate: Option<TextEdit>,
    /// The decision request the Decisions page shows, and the labels picked for each question.
    pub decision: Option<String>,
    pub picks: BTreeMap<String, Vec<String>>,
    pub note: TextEdit,
    /// Whether the share form is open.
    pub sharing: bool,
    pub grantee: TextEdit,
    pub role: usize,
    /// `YYYY-MM-DD`, or blank for no end.
    pub until: TextEdit,
    /// Set while an action is on its way: replies up to this one predate it.
    pub sent: Option<u64>,
    /// What the newest action came to.
    pub outcome: Option<Result<String, String>>,
    /// The removal armed by a first click, which a second click on it carries out.
    pub armed: Option<String>,
    /// The new-secret form, while open.
    pub new_secret: Option<SecretForm>,
    /// The owner picked to move the shown secret to.
    pub move_to: Option<String>,
    /// The import being registered: its id, and the registry id and description typed.
    pub register: Option<(String, TextEdit, TextEdit)>,
    /// The webhook the Triggers page shows in full.
    pub webhook: Option<String>,
    /// Whether the Admin page shows its policy tab.
    pub policy_tab: bool,
    /// The policy set shown, by digest; the active one when none is picked.
    pub policy: Option<String>,
    /// A new policy set's text, while its editor is open.
    pub policy_draft: Option<TextEdit>,
    /// The status the Issues page lists; none lists every one.
    pub issue_status: InStatus,
    /// The issue picked on the Issues page.
    pub issue: Option<String>,
    /// The scratch copy of a policy set open in the user's own editor.
    pub policy_file: Option<std::path::PathBuf>,
    /// The login, action and resource to explain a decision for.
    pub explain: (TextEdit, TextEdit, TextEdit),
    /// The setting being overridden on the Admin page, and the value typed for it.
    pub knob: Option<(String, TextEdit)>,
    /// Why an admin change is made, which the controller records.
    pub why: TextEdit,
    /// The repo typed to start watching.
    pub new_repo: TextEdit,
    /// The blueprint being used, with the new draft's id and description, while its form is open.
    pub use_blueprint: Option<(String, TextEdit, TextEdit)>,
    /// The provider the Providers page shows, and its form while open.
    pub provider: Option<String>,
    pub provider_form: Option<crate::provider::ProviderForm>,
    /// The webhook form, while open.
    pub webhook_form: Option<crate::webhook::WebhookForm>,
    /// The team the Teams page shows.
    pub team: Option<String>,
    /// The new-team form, while open.
    pub new_team: Option<crate::teams::TeamForm>,
    pub add_member: crate::teams::MemberForm,
    /// The team's next display name, while its rename form is open.
    pub rename_team: Option<TextEdit>,
    /// The test-fire form, while open on a schedule.
    pub test_fire: Option<TestFireForm>,
    /// The mint form on the API keys page.
    pub new_key: crate::apikeys::KeyForm,
    /// The decision request a notification's button answered, until its outcome lands.
    pub quick: Option<String>,
    /// The decision requests already announced; `None` until the first queue lands.
    seen: Option<HashSet<String>>,
}

impl Default for Desk {
    fn default() -> Self {
        Desk {
            secret: None,
            rotate: None,
            decision: None,
            picks: BTreeMap::new(),
            note: TextEdit::new(""),
            sharing: false,
            grantee: TextEdit::new(""),
            role: 0,
            until: TextEdit::new(""),
            sent: None,
            outcome: None,
            armed: None,
            new_secret: None,
            move_to: None,
            register: None,
            new_key: crate::apikeys::KeyForm::default(),
            policy_tab: false,
            policy: None,
            policy_draft: None,
            policy_file: None,
            issue_status: Some(crate::issues::FIRST.to_string()),
            issue: None,
            explain: (TextEdit::new(""), TextEdit::new(""), TextEdit::new("")),
            knob: None,
            why: TextEdit::new(""),
            new_repo: TextEdit::new(""),
            use_blueprint: None,
            provider: None,
            provider_form: None,
            webhook: None,
            webhook_form: None,
            team: None,
            new_team: None,
            add_member: crate::teams::MemberForm::default(),
            rename_team: None,
            test_fire: None,
            quick: None,
            seen: None,
        }
    }
}

impl Desk {
    pub fn pick_decision(&mut self, id: &str) {
        if self.decision.as_deref() != Some(id) {
            self.picks.clear();
            self.note = TextEdit::new("");
        }
        self.decision = Some(id.to_string());
    }

    pub fn pick_secret(&mut self, id: &str) {
        if self.secret.as_deref() != Some(id) {
            self.rotate = None;
        }
        self.secret = Some(id.to_string());
    }

    pub fn pick_team(&mut self, slug: &str) {
        if self.team.as_deref() != Some(slug) {
            self.add_member = crate::teams::MemberForm::default();
            self.rename_team = None;
        }
        self.new_team = None;
        self.team = Some(slug.to_string());
    }

    pub fn busy(&self) -> bool {
        self.sent.is_some()
    }

    /// Pick `label` for `question`; a question taking one answer drops the one it had.
    pub fn toggle_pick(&mut self, question: &str, label: &str, multiple: bool) {
        let picked = self.picks.entry(question.to_string()).or_default();
        if let Some(at) = picked.iter().position(|l| l == label) {
            picked.remove(at);
        } else if multiple {
            picked.push(label.to_string());
        } else {
            *picked = vec![label.to_string()];
        }
    }

    /// Takes in an answer's outcome. One that was sent drops the picks it was made from, which
    /// stay when a notification answered a request other than the one shown. A notification's
    /// answer that was not sent comes back as its request and why.
    pub(in crate::admin) fn answered(
        &mut self,
        result: &Result<String, String>,
    ) -> Option<(String, String)> {
        let quick = self.quick.take();
        match result {
            Ok(_) => {
                if quick.is_none() || quick == self.decision {
                    self.picks.clear();
                    self.note = TextEdit::new("");
                }
                None
            }
            Err(why) => quick.map(|decision| (decision, why.clone())),
        }
    }

    /// The questions of `decision` still without an answer.
    pub fn unanswered<'a>(&self, decision: &'a dto::Decision) -> Vec<&'a str> {
        decision
            .questions
            .iter()
            .filter(|q| self.picks.get(&q.id).is_none_or(Vec::is_empty))
            .map(|q| q.id.as_str())
            .collect()
    }

    /// The decision requests in `open` not announced before. The first queue only marks what is
    /// already waiting.
    pub(in crate::admin) fn arrived<'a>(
        &mut self,
        open: &'a [dto::DecisionSummary],
    ) -> Vec<&'a dto::DecisionSummary> {
        let ids: HashSet<String> = open.iter().map(|d| d.id.clone()).collect();
        let new = match &self.seen {
            Some(seen) => open.iter().filter(|d| !seen.contains(&d.id)).collect(),
            None => Vec::new(),
        };
        self.seen = Some(ids);
        new
    }
}

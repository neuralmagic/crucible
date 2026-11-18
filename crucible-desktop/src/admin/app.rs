//! The management pages' side of the app: what the worker is asked to watch, and what its
//! answers change.

use crate::admin::{Action, AdminView, Did, Watch};
use crux::client::Shared;
use crux::dto;
use rune::TextEdit;

/// What a decision request that just opened is announced with.
fn note(decision: &dto::DecisionSummary) -> crate::notify::Note {
    let mut body = decision.questions.join(", ");
    if let Some(key) = &decision.launch_key {
        body = format!("{body} \u{b7} {key}");
    }
    crate::notify::Note {
        id: decision.id.clone(),
        title: format!("Waiting on you \u{b7} {}", decision.task),
        body,
        link: crate::link::Link::Decision {
            id: decision.id.clone(),
        }
        .url(),
    }
}

impl crate::App {
    /// Tell the worker what the window shows.
    pub fn sync_admin(&mut self) {
        let controller = self.backend == crate::Backend::Controller;
        let on = |page: crate::Page| controller && self.page == page;
        let watch = Watch {
            controller,
            schedules: on(crate::Page::Triggers),
            secrets: on(crate::Page::Secrets),
            secret: self
                .remote
                .desk
                .secret
                .clone()
                .filter(|_| on(crate::Page::Secrets)),
            shares: self.shared_here().filter(|_| on(crate::Page::Runs)),
            decision: self
                .remote
                .desk
                .decision
                .clone()
                .filter(|_| on(crate::Page::Decisions)),
            keys: on(crate::Page::Keys),
            webhooks: on(crate::Page::Triggers),
            webhook: self
                .remote
                .desk
                .webhook
                .clone()
                .filter(|_| on(crate::Page::Triggers)),
            webhook_form: self
                .remote
                .desk
                .webhook_form
                .as_ref()
                .filter(|_| on(crate::Page::Triggers))
                .map(|form| form.playbook.clone()),
            providers: on(crate::Page::Providers),
            blueprints: on(crate::Page::Blueprints),
            activity: on(crate::Page::Activity),
            admin: on(crate::Page::Admin) && !self.remote.desk.policy_tab,
            policy: on(crate::Page::Admin) && self.remote.desk.policy_tab,
            issues: on(crate::Page::Issues).then(|| self.remote.desk.issue_status.clone()),
            issue: self
                .remote
                .desk
                .issue
                .clone()
                .filter(|_| on(crate::Page::Issues)),
            about: on(crate::Page::Settings),
            teams: controller,
            team_audit: self.owned_team().filter(|_| on(crate::Page::Teams)),
        };
        if self.remote.admin_sent.as_ref() != Some(&watch) {
            self.remote.admin_client.watch(watch.clone());
            self.remote.admin_sent = Some(watch);
        }
    }

    /// Start a new policy set from `text`: in the user's own editor on a scratch copy, or in the
    /// page where there is no editor to hand it to.
    pub fn edit_policy(&mut self, digest: &str, text: &str) {
        if cfg!(target_arch = "wasm32") {
            self.remote.desk.policy_draft = Some(TextEdit::new(text));
            return;
        }
        let name = format!("policy-{}.cedar", digest.get(..12).unwrap_or(digest));
        match crate::editor::scratch(&name, text) {
            Ok(path) => {
                self.open_path_in_editor(&path, 1);
                self.remote.desk.policy_file = Some(path);
                self.remote.desk.outcome = self.editor_error.clone().map(Err);
            }
            Err(why) => self.remote.desk.outcome = Some(Err(why)),
        }
    }

    /// Store the scratch copy as it is on disk now.
    pub fn store_policy_file(&mut self) {
        let Some(path) = &self.remote.desk.policy_file else {
            return;
        };
        match std::fs::read_to_string(path) {
            Ok(text) => self.act(Action::CreatePolicy { text }),
            Err(why) => {
                self.remote.desk.outcome = Some(Err(format!("reading {}: {why}", path.display())));
            }
        }
    }

    /// The picked team's slug, when the caller owns it and so may read its audit trail.
    fn owned_team(&self) -> Option<String> {
        let slug = self.remote.desk.team.as_ref()?;
        let teams = self.remote.admin.teams.as_ref()?.as_ref().ok()?;
        teams
            .iter()
            .find(|team| &team.slug == slug && team.my_role == Some(dto::TeamRole::Owner))
            .map(|team| team.slug.clone())
    }

    /// What the runs page on screen can share: the selected playbook, or the draft it launched
    /// from.
    pub fn shared_here(&self) -> Option<Shared> {
        let Some(crate::Selection::Live {
            playbook,
            launch: None,
        }) = &self.selection
        else {
            return None;
        };
        Some(if self.remote.registered.contains(playbook) {
            Shared::Playbook(playbook.clone())
        } else {
            Shared::Draft(playbook.clone())
        })
    }

    pub fn act(&mut self, action: Action) {
        self.remote.desk.armed = None;
        self.remote.desk.sent = Some(
            self.remote
                .admin
                .acted
                .as_ref()
                .map_or(0, |acted| acted.seq),
        );
        self.remote.desk.outcome = None;
        self.remote.admin_client.act(action);
    }

    pub fn apply_admin(&mut self, view: AdminView) {
        if let (Some(sent), Some(acted)) = (self.remote.desk.sent, &view.acted)
            && acted.seq > sent
        {
            self.remote.desk.sent = None;
            if acted.result.is_ok() {
                match acted.did {
                    Did::Rotate => self.remote.desk.rotate = None,
                    Did::Answer => {
                        self.remote.desk.picks.clear();
                        self.remote.desk.note = TextEdit::new("");
                    }
                    Did::Grant => {
                        self.remote.desk.sharing = false;
                        self.remote.desk.grantee = TextEdit::new("");
                        self.remote.desk.until = TextEdit::new("");
                    }
                    Did::CreateSecret => self.remote.desk.new_secret = None,
                    Did::MoveSecret => self.remote.desk.move_to = None,
                    Did::MintKey => {
                        self.remote.desk.new_key = crate::apikeys::KeyForm::default();
                    }
                    Did::CreateTeam => {
                        if let Some(Ok((slug, _))) =
                            self.remote.desk.new_team.as_ref().map(|form| form.ask())
                        {
                            self.remote.desk.pick_team(&slug);
                        }
                        self.remote.desk.new_team = None;
                    }
                    Did::RenameTeam => self.remote.desk.rename_team = None,
                    Did::DeleteTeam => self.remote.desk.team = None,
                    Did::DeleteWebhook => self.remote.desk.webhook = None,
                    Did::SaveProvider => {
                        if let Some(form) = self.remote.desk.provider_form.take() {
                            self.remote.desk.provider = Some(form.id.text.trim().to_string());
                        }
                    }
                    Did::UseBlueprint => self.remote.desk.use_blueprint = None,
                    Did::ParkIssue => self.remote.desk.why = TextEdit::new(""),
                    Did::CreatePolicy => {
                        self.remote.desk.policy_draft = None;
                        self.remote.desk.policy_file = None;
                        self.remote.desk.policy = view.stored_policy.clone();
                    }
                    Did::SetOverrides => {
                        self.remote.desk.knob = None;
                        self.remote.desk.why = TextEdit::new("");
                    }
                    Did::SetAutopilot => self.remote.desk.why = TextEdit::new(""),
                    Did::AddRepo => {
                        self.remote.desk.new_repo = TextEdit::new("");
                        self.remote.desk.why = TextEdit::new("");
                    }
                    Did::DeleteProvider => {
                        self.remote.desk.provider = None;
                        self.remote.desk.provider_form = None;
                    }
                    Did::SaveWebhook => {
                        let saved = self
                            .remote
                            .desk
                            .webhook_form
                            .take()
                            .and_then(|form| form.editing);
                        let created = view.rotated.as_ref().map(|r| r.webhook.id.clone());
                        self.remote.desk.webhook = saved.or(created);
                    }
                    Did::PutMembers => {
                        self.remote.desk.add_member = crate::teams::MemberForm::default();
                    }
                    Did::SaveSchedule => {
                        self.remote.launcher = None;
                        self.page = crate::Page::Triggers;
                    }
                    Did::TestFireSchedule => {
                        self.remote.desk.test_fire = None;
                    }
                    Did::Enable
                    | Did::FireSchedule
                    | Did::Revoke
                    | Did::Unbind
                    | Did::RegisterImport
                    | Did::DiscardImport
                    | Did::CancelLaunch
                    | Did::RevokeKey
                    | Did::SignOutMcp
                    | Did::EnableWebhook
                    | Did::RotateWebhook
                    | Did::CheckWebhook
                    | Did::PreviewWebhook
                    | Did::Reconcile
                    | Did::UnparkIssue
                    | Did::PauseRepo
                    | Did::UnwatchRepo
                    | Did::ActivatePolicy
                    | Did::Explain => {}
                }
            }
            if let (Did::SaveSchedule, Err(why), Some(launcher)) =
                (acted.did, &acted.result, self.remote.launcher.as_mut())
            {
                launcher.stage = crate::Stage::Ready;
                launcher.refusal = Some(why.clone());
            }
            self.remote.desk.outcome = Some(acted.result.clone());
        }
        if let Some(Ok(open)) = &view.decisions {
            let looking = crate::notify::app_focused() && self.page == crate::Page::Decisions;
            for decision in self.remote.desk.arrived(open) {
                if self.settings.ask
                    && !looking
                    && let Some(notifier) = &self.notifier
                {
                    notifier.post(&note(decision));
                }
            }
        }
        if let (Some(form), Some((playbook, Some(Ok(schema))))) =
            (&mut self.remote.desk.webhook_form, &view.webhook_schema)
        {
            form.lay_out(playbook, &crate::form::fields(schema));
        }
        self.remote.admin = view;
    }

    /// How many decision requests wait on the signed-in user.
    pub fn waiting(&self) -> usize {
        match &self.remote.admin.decisions {
            Some(Ok(open)) => open.len(),
            _ => 0,
        }
    }

    /// Scope packs to approve and imports to settle.
    pub fn approvals_waiting(&self) -> usize {
        match &self.remote.admin.approvals {
            Some(Ok(queue)) => queue.waiting(),
            _ => 0,
        }
    }
}

/// An open decision request to lay the Decisions page out against.
pub(in crate::admin) fn fixture() -> Result<dto::Decision, String> {
    serde_json::from_str(include_str!("../../fixtures/decision.json"))
        .map_err(|error| format!("the decision fixture: {error}"))
}

/// Shows the fixture request on the Decisions page, queued and picked.
pub fn show_fixture(app: &mut crate::App) {
    let decision = fixture();
    if let Ok(d) = &decision {
        app.remote.admin.decisions = Some(Ok(vec![dto::DecisionSummary {
            id: d.id.clone(),
            run_id: d.run_id.clone(),
            task: d.task.clone(),
            launch_key: d.launch_key.clone(),
            questions: d.questions.iter().map(|q| q.id.clone()).collect(),
            opened_at: d.opened_at.clone(),
            expires_at: d.expires_at.clone(),
        }]));
        app.remote.desk.pick_decision(&d.id);
        app.remote.admin.decision = Some((d.id.clone(), Some(Ok(d.clone()))));
    } else {
        app.remote.admin.decisions = Some(decision.map(|_| Vec::new()));
    }
}

//! Schedules, secrets, shares and decision requests, read and changed through crux on a task of
//! their own so a slow call never holds up the launch list.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crux::client::Shared;
use crux::dto;
use rune::time::Instant;
use rune::{TextEdit, Waker};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::net::{Connection, Connector, Task, within};

/// How often what is on screen is read again.
const SHOWN: Duration = Duration::from_secs(15);
/// How often the decision queue is read while nothing shows it, for the badge and notifications.
const QUEUE: Duration = Duration::from_secs(30);
/// How many of a webhook's newest deliveries are read.
const DELIVERIES: i64 = 20;

/// A read that has not come back yet, failed, or landed.
pub type Loaded<T> = Option<Result<T, String>>;

/// What the window shows, and so what the worker keeps current.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Watch {
    /// The controller backend is on; nothing is read while it is not.
    pub controller: bool,
    pub schedules: bool,
    pub secrets: bool,
    pub secret: Option<String>,
    pub shares: Option<Shared>,
    pub decision: Option<String>,
    /// The API keys page is shown.
    pub keys: bool,
    pub teams: bool,
    /// The team whose audit trail the Teams page shows.
    pub team_audit: Option<String>,
    pub webhooks: bool,
    /// The webhook whose deliveries the Triggers page shows.
    pub webhook: Option<String>,
    /// The playbook the open webhook form launches.
    pub webhook_form: Option<String>,
}

/// A change to make on the controller.
#[derive(Clone, Debug)]
pub enum Action {
    Enable {
        schedule: Box<dto::Schedule>,
        on: bool,
    },
    Grant {
        of: Shared,
        grantee: String,
        role: String,
        not_after: Option<String>,
    },
    Revoke {
        of: Shared,
        grantee: String,
    },
    Rotate {
        secret: String,
        value: String,
    },
    Unbind {
        secret: String,
        binding: String,
    },
    Answer {
        id: String,
        labels: BTreeMap<String, Vec<String>>,
        digest: String,
        note: Option<String>,
    },
    CreateSecret {
        body: serde_json::Value,
    },
    MoveSecret {
        secret: String,
        owner: String,
    },
    /// Create a schedule, or replace schedule `id` whole.
    SaveSchedule {
        id: Option<String>,
        body: serde_json::Value,
    },
    /// Register pending import `import` as registry id `id`.
    RegisterImport {
        import: String,
        id: String,
        description: String,
    },
    DiscardImport {
        import: String,
    },
    /// Stop playbook launch `key`: park it and delete its running pods.
    CancelLaunch {
        key: String,
    },
    /// Mint an API key that expires in `days`, or never.
    MintKey {
        name: String,
        days: Option<i64>,
    },
    RevokeKey {
        id: String,
    },
    /// Sign out one MCP client, or all of them.
    SignOutMcp {
        client: Option<String>,
    },
    CreateTeam {
        slug: String,
        name: String,
    },
    RenameTeam {
        slug: String,
        name: String,
    },
    DeleteTeam {
        slug: String,
    },
    /// Replace team `slug`'s whole member list.
    PutMembers {
        slug: String,
        members: Vec<dto::TeamMemberBody>,
    },
    /// Resume (`on`) or pause a webhook.
    EnableWebhook {
        id: String,
        on: bool,
    },
    RotateWebhook {
        id: String,
    },
    DeleteWebhook {
        id: String,
    },
    /// Create a webhook, or replace webhook `id`.
    SaveWebhook {
        id: Option<String>,
        body: Box<dto::WebhookBody>,
    },
    CheckWebhook {
        body: Box<dto::WebhookBody>,
    },
    /// Run the form's transform on a sample delivery.
    PreviewWebhook {
        body: Box<dto::WebhookBody>,
        sample: serde_json::Value,
        headers: BTreeMap<String, String>,
    },
}

/// Which kind of action an outcome answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Did {
    Enable,
    Grant,
    Revoke,
    Rotate,
    Unbind,
    Answer,
    CreateSecret,
    MoveSecret,
    SaveSchedule,
    RegisterImport,
    DiscardImport,
    CancelLaunch,
    MintKey,
    RevokeKey,
    SignOutMcp,
    CreateTeam,
    RenameTeam,
    DeleteTeam,
    PutMembers,
    EnableWebhook,
    RotateWebhook,
    DeleteWebhook,
    SaveWebhook,
    CheckWebhook,
    PreviewWebhook,
}

impl Action {
    fn did(&self) -> Did {
        match self {
            Action::Enable { .. } => Did::Enable,
            Action::Grant { .. } => Did::Grant,
            Action::Revoke { .. } => Did::Revoke,
            Action::Rotate { .. } => Did::Rotate,
            Action::Unbind { .. } => Did::Unbind,
            Action::Answer { .. } => Did::Answer,
            Action::CreateSecret { .. } => Did::CreateSecret,
            Action::MoveSecret { .. } => Did::MoveSecret,
            Action::SaveSchedule { .. } => Did::SaveSchedule,
            Action::RegisterImport { .. } => Did::RegisterImport,
            Action::DiscardImport { .. } => Did::DiscardImport,
            Action::CancelLaunch { .. } => Did::CancelLaunch,
            Action::MintKey { .. } => Did::MintKey,
            Action::RevokeKey { .. } => Did::RevokeKey,
            Action::SignOutMcp { .. } => Did::SignOutMcp,
            Action::CreateTeam { .. } => Did::CreateTeam,
            Action::RenameTeam { .. } => Did::RenameTeam,
            Action::DeleteTeam { .. } => Did::DeleteTeam,
            Action::PutMembers { .. } => Did::PutMembers,
            Action::EnableWebhook { .. } => Did::EnableWebhook,
            Action::RotateWebhook { .. } => Did::RotateWebhook,
            Action::DeleteWebhook { .. } => Did::DeleteWebhook,
            Action::SaveWebhook { .. } => Did::SaveWebhook,
            Action::CheckWebhook { .. } => Did::CheckWebhook,
            Action::PreviewWebhook { .. } => Did::PreviewWebhook,
        }
    }

    fn what(&self) -> String {
        match self {
            Action::Enable { schedule, on: true } => format!("Enabled {}", schedule.playbook),
            Action::Enable { schedule, .. } => format!("Disabled {}", schedule.playbook),
            Action::Grant { grantee, role, .. } => format!("Shared with {grantee} as {role}"),
            Action::Revoke { grantee, .. } => format!("Stopped sharing with {grantee}"),
            Action::Rotate { .. } => "Rotated".to_string(),
            Action::Unbind { .. } => "Unbound".to_string(),
            Action::Answer { .. } => "Answered".to_string(),
            Action::CreateSecret { body } => format!(
                "Created {}",
                body.get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("the secret")
            ),
            Action::MoveSecret { owner, .. } => format!("Moved to {owner}"),
            Action::SaveSchedule { id: None, .. } => "Scheduled".to_string(),
            Action::SaveSchedule { id: Some(_), .. } => "Saved the schedule".to_string(),
            Action::RegisterImport { id, .. } => format!("Registered {id}"),
            Action::DiscardImport { .. } => "Discarded the import".to_string(),
            Action::CancelLaunch { .. } => "Cancelling the launch".to_string(),
            Action::MintKey { name, .. } => format!("Minted {name}"),
            Action::RevokeKey { .. } => "Revoked".to_string(),
            Action::SignOutMcp { .. } => "Signed out".to_string(),
            Action::CreateTeam { slug, .. } => format!("Created {slug}"),
            Action::RenameTeam { name, .. } => format!("Renamed to {name}"),
            Action::DeleteTeam { slug } => format!("Deleted {slug}"),
            Action::PutMembers { .. } => "Saved the members".to_string(),
            Action::EnableWebhook { on: true, .. } => "Resumed the webhook".to_string(),
            Action::EnableWebhook { .. } => "Paused the webhook".to_string(),
            Action::RotateWebhook { .. } => "Rotated the secret".to_string(),
            Action::DeleteWebhook { .. } => "Deleted the webhook".to_string(),
            Action::SaveWebhook { id: None, .. } => "Created the webhook".to_string(),
            Action::SaveWebhook { id: Some(_), .. } => "Saved the webhook".to_string(),
            Action::CheckWebhook { .. } => "Checked".to_string(),
            Action::PreviewWebhook { .. } => "Previewed".to_string(),
        }
    }
}

/// What the last action came to. `seq` only grows.
#[derive(Clone, Debug, PartialEq)]
pub struct Acted {
    pub seq: u64,
    pub did: Did,
    pub result: Result<String, String>,
}

/// A secret with its bindings and audit trail.
#[derive(Clone, Debug)]
pub struct SecretView {
    pub detail: dto::SecretDetail,
    pub audit: Vec<dto::SecretAudit>,
}

/// What the worker last read, for the window.
#[derive(Clone, Default)]
pub struct AdminView {
    pub schedules: Loaded<Vec<dto::Schedule>>,
    pub secrets: Loaded<Vec<dto::Secret>>,
    pub secret: Option<(String, Loaded<SecretView>)>,
    pub shares: Option<(Shared, Loaded<Vec<dto::Share>>)>,
    pub decisions: Loaded<Vec<dto::DecisionSummary>>,
    pub decision: Option<(String, Loaded<dto::Decision>)>,
    /// Scope packs to approve, imports to settle and kept PRs to review.
    pub approvals: Loaded<dto::Approvals>,
    pub keys: Loaded<Vec<dto::ApiKey>>,
    pub mcp: Loaded<dto::McpSignIns>,
    pub teams: Loaded<Vec<dto::Team>>,
    pub team_audit: Option<(String, Loaded<Vec<dto::TeamAudit>>)>,
    pub webhooks: Loaded<Vec<dto::Webhook>>,
    pub deliveries: Option<(String, Loaded<Vec<dto::WebhookDelivery>>)>,
    /// The secret of the webhook just created or rotated, until another webhook is opened or
    /// the Triggers page is left.
    pub rotated: Option<dto::WebhookSecret>,
    /// The open webhook form's playbook and its params schema.
    pub webhook_schema: Option<(String, Loaded<serde_json::Value>)>,
    pub presets: Loaded<Vec<dto::WebhookPreset>>,
    /// What the last check and preview of the open webhook form came to.
    pub webhook_check: Option<dto::WebhookCheck>,
    pub webhook_preview: Option<dto::WebhookPreview>,
    /// The key just minted, with its secret, for as long as the API keys page stays open.
    pub minted: Option<dto::MintedKey>,
    pub acted: Option<Acted>,
}

enum Msg {
    Watch(Watch),
    Act(Action),
    Reconnect,
}

/// The UI-side handle to the worker. Dropping it ends its task.
pub struct Client {
    tx: UnboundedSender<Msg>,
    shared: Arc<Mutex<Published>>,
    _task: Option<Task>,
}

#[derive(Default)]
struct Published {
    view: AdminView,
    version: u64,
    waker: Option<Waker>,
}

impl Client {
    pub fn spawn(connector: Connector) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Mutex::new(Published::default()));
        let task = match connector.runtime() {
            Ok(runtime) => Some(Task::spawn(
                &runtime,
                run(connector, rx, Arc::clone(&shared)),
            )),
            Err(why) => {
                let view = AdminView {
                    decisions: Some(Err(why)),
                    ..AdminView::default()
                };
                publish(&shared, view);
                None
            }
        };
        Client {
            tx,
            shared,
            _task: task,
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.waker = Some(waker);
        }
    }

    pub fn watch(&self, watch: Watch) {
        let _ = self.tx.send(Msg::Watch(watch));
    }

    pub fn act(&self, action: Action) {
        let _ = self.tx.send(Msg::Act(action));
    }

    pub fn reconnect(&self) {
        let _ = self.tx.send(Msg::Reconnect);
    }

    pub fn take(&self, seen: u64) -> Option<(u64, AdminView)> {
        let shared = self.shared.lock().ok()?;
        (shared.version > seen).then(|| (shared.version, shared.view.clone()))
    }
}

fn publish(shared: &Mutex<Published>, view: AdminView) {
    let waker = match shared.lock() {
        Ok(mut shared) => {
            shared.view = view;
            shared.version += 1;
            shared.waker.clone()
        }
        Err(_) => return,
    };
    if let Some(waker) = waker {
        waker();
    }
}

struct Worker {
    connector: Connector,
    view: AdminView,
    acted: u64,
}

impl Worker {
    async fn refresh(&mut self, watch: &Watch) {
        let read = match self.connector.connect().await {
            Ok(c) => read(&c.client, watch).await,
            Err(why) => Read::failed(&why, watch),
        };
        self.view.decisions = Some(read.decisions);
        self.view.approvals = Some(read.approvals);
        if let Some(schedules) = read.schedules {
            self.view.schedules = Some(schedules);
        }
        if let Some(secrets) = read.secrets {
            self.view.secrets = Some(secrets);
        }
        self.view.secret = read.secret.map(|(id, view)| (id, Some(view)));
        self.view.shares = read.shares.map(|(of, shares)| (of, Some(shares)));
        self.view.decision = read.decision.map(|(id, decision)| (id, Some(decision)));
        if let Some(keys) = read.keys {
            self.view.keys = Some(keys);
        }
        if let Some(mcp) = read.mcp {
            self.view.mcp = Some(mcp);
        }
        if !watch.keys {
            self.view.minted = None;
        }
        if let Some(teams) = read.teams {
            self.view.teams = Some(teams);
        }
        self.view.team_audit = read.team_audit.map(|(slug, audit)| (slug, Some(audit)));
        if let Some(webhooks) = read.webhooks {
            self.view.webhooks = Some(webhooks);
        }
        self.view.deliveries = read.deliveries.map(|(id, list)| (id, Some(list)));
        let rotated = self.view.rotated.as_ref().map(|r| r.webhook.id.as_str());
        let elsewhere = watch
            .webhook
            .as_deref()
            .is_some_and(|shown| Some(shown) != rotated);
        if !watch.webhooks || elsewhere {
            self.view.rotated = None;
        }
        self.view.webhook_schema = read.webhook_schema.map(|(id, schema)| (id, Some(schema)));
        if let Some(presets) = read.presets {
            self.view.presets = Some(presets);
        }
        if watch.webhook_form.is_none() {
            self.view.webhook_check = None;
            self.view.webhook_preview = None;
        }
    }

    async fn act(&mut self, action: Action) {
        let what = action.what();
        let result = match self.connector.connect().await {
            Ok(c) => act(&c, &action, &mut self.view).await,
            Err(why) => Err(why),
        };
        self.acted += 1;
        self.view.acted = Some(Acted {
            seq: self.acted,
            did: action.did(),
            result: result.map(|()| what),
        });
    }
}

/// One pass over what `Watch` asks for, read all at once.
struct Read {
    decisions: Result<Vec<dto::DecisionSummary>, String>,
    approvals: Result<dto::Approvals, String>,
    schedules: Option<Result<Vec<dto::Schedule>, String>>,
    secrets: Option<Result<Vec<dto::Secret>, String>>,
    secret: Option<(String, Result<SecretView, String>)>,
    shares: Option<(Shared, Result<Vec<dto::Share>, String>)>,
    decision: Option<(String, Result<dto::Decision, String>)>,
    keys: Option<Result<Vec<dto::ApiKey>, String>>,
    mcp: Option<Result<dto::McpSignIns, String>>,
    teams: Option<Result<Vec<dto::Team>, String>>,
    team_audit: Option<(String, Result<Vec<dto::TeamAudit>, String>)>,
    webhooks: Option<Result<Vec<dto::Webhook>, String>>,
    deliveries: Option<(String, Result<Vec<dto::WebhookDelivery>, String>)>,
    webhook_schema: Option<(String, Result<serde_json::Value, String>)>,
    presets: Option<Result<Vec<dto::WebhookPreset>, String>>,
}

impl Read {
    fn failed(why: &str, watch: &Watch) -> Read {
        let why = why.to_string();
        Read {
            decisions: Err(why.clone()),
            approvals: Err(why.clone()),
            schedules: watch.schedules.then(|| Err(why.clone())),
            secrets: watch.secrets.then(|| Err(why.clone())),
            secret: watch.secret.clone().map(|id| (id, Err(why.clone()))),
            shares: watch.shares.clone().map(|of| (of, Err(why.clone()))),
            decision: watch.decision.clone().map(|id| (id, Err(why.clone()))),
            keys: watch.keys.then(|| Err(why.clone())),
            mcp: watch.keys.then(|| Err(why.clone())),
            teams: watch.teams.then(|| Err(why.clone())),
            team_audit: watch
                .team_audit
                .clone()
                .map(|slug| (slug, Err(why.clone()))),
            webhooks: watch.webhooks.then(|| Err(why.clone())),
            deliveries: watch.webhook.clone().map(|id| (id, Err(why.clone()))),
            webhook_schema: watch.webhook_form.clone().map(|id| (id, Err(why.clone()))),
            presets: watch.webhook_form.is_some().then(|| Err(why.clone())),
        }
    }
}

/// `read`'s answer when `on`, and nothing asked otherwise.
async fn when<T>(on: bool, read: impl Future<Output = T>) -> Option<T> {
    if on { Some(read.await) } else { None }
}

async fn read(client: &crux::Client, watch: &Watch) -> Read {
    let secret = async {
        let id = watch.secret.as_ref()?;
        let (detail, audit) = tokio::join!(
            within("the secret", client.secret(id)),
            within("its audit trail", client.secret_audit(id)),
        );
        let view = detail.and_then(|detail| {
            Ok(SecretView {
                detail,
                audit: audit?,
            })
        });
        Some((id.clone(), view))
    };
    let shares = async {
        let of = watch.shares.as_ref()?;
        Some((of.clone(), within("the shares", client.shares(of)).await))
    };
    let decision = async {
        let id = watch.decision.as_ref()?;
        let decision = within("the decision request", client.decision(id)).await;
        Some((id.clone(), decision))
    };
    let team_audit = async {
        let slug = watch.team_audit.as_ref()?;
        let audit = within("the team's audit trail", client.team_audit(slug)).await;
        Some((slug.clone(), audit))
    };
    let deliveries = async {
        let id = watch.webhook.as_ref()?;
        let list = within(
            "the deliveries",
            client.webhook_deliveries(id, None, Some(DELIVERIES)),
        )
        .await;
        Some((id.clone(), list))
    };
    let webhook_schema = async {
        let playbook = watch.webhook_form.as_ref()?;
        let schema = within("the params schema", client.playbook_schema(playbook)).await;
        Some((playbook.clone(), schema))
    };
    let (
        decisions,
        approvals,
        schedules,
        secrets,
        secret,
        shares,
        decision,
        keys,
        mcp,
        teams,
        team_audit,
        webhooks,
        deliveries,
        webhook_schema,
        presets,
    ) = tokio::join!(
        within("the decision queue", client.decisions()),
        within("the approval queue", client.approvals()),
        when(watch.schedules, within("the schedules", client.schedules())),
        when(watch.secrets, within("the secrets", client.secrets())),
        secret,
        shares,
        decision,
        when(watch.keys, within("the API keys", client.api_keys())),
        when(watch.keys, within("the MCP clients", client.mcp_sign_ins())),
        when(watch.teams, within("the teams", client.teams())),
        team_audit,
        when(watch.webhooks, within("the webhooks", client.webhooks())),
        deliveries,
        webhook_schema,
        when(
            watch.webhook_form.is_some(),
            within("the presets", client.webhook_presets()),
        ),
    );
    Read {
        decisions: decisions.map(|queue| queue.open),
        approvals,
        schedules,
        secrets,
        secret,
        shares,
        decision,
        keys,
        mcp,
        teams,
        team_audit,
        webhooks,
        deliveries,
        webhook_schema,
        presets,
    }
}

async fn act(c: &Connection, action: &Action, view: &mut AdminView) -> Result<(), String> {
    let client = &c.client;
    match action {
        Action::Enable { schedule, on } => within(
            "saving the schedule",
            client.update_schedule::<serde_json::Value>(&schedule.id, &schedule.body(*on)),
        )
        .await
        .map(drop),
        Action::Grant {
            of,
            grantee,
            role,
            not_after,
        } => within(
            "the share",
            client.grant_share(of, grantee, role, not_after.as_deref()),
        )
        .await
        .map(drop),
        Action::Revoke { of, grantee } => {
            within("the share", client.revoke_share(of, grantee)).await
        }
        Action::Rotate { secret, value } => {
            within("the rotation", client.rotate_secret(secret, value))
                .await
                .map(drop)
        }
        Action::Unbind { secret, binding } => {
            within("the binding", client.unbind_secret(secret, binding)).await
        }
        Action::Answer {
            id,
            labels,
            digest,
            note,
        } => within(
            "the answer",
            client.answer_decision(id, labels, digest, note.as_deref()),
        )
        .await
        .map(drop),
        Action::CreateSecret { body } => within(
            "the new secret",
            client.create_secret::<serde_json::Value>(body),
        )
        .await
        .map(drop),
        Action::MoveSecret { secret, owner } => {
            within("the owner change", client.transfer_secret(secret, owner))
                .await
                .map(drop)
        }
        Action::SaveSchedule { id: None, body } => within(
            "the new schedule",
            client.create_schedule::<serde_json::Value>(body),
        )
        .await
        .map(drop),
        Action::SaveSchedule { id: Some(id), body } => within(
            "the schedule",
            client.update_schedule::<serde_json::Value>(id, body),
        )
        .await
        .map(drop),
        Action::RegisterImport {
            import,
            id,
            description,
        } => within(
            "the registration",
            client.register_import(import, &dto::RegisterImport { id, description }),
        )
        .await
        .map(drop),
        Action::DiscardImport { import } => within("the discard", client.discard_import(import))
            .await
            .map(drop),
        Action::CancelLaunch { key } => within("the cancel", client.cancel_launch(key))
            .await
            .map(drop),
        Action::MintKey { name, days } => within("the new key", client.mint_api_key(name, *days))
            .await
            .map(|key| view.minted = Some(key)),
        Action::RevokeKey { id } => within("the revoke", client.revoke_api_key(id)).await,
        Action::SignOutMcp { client: one } => {
            within("the sign-out", client.sign_out_mcp(one.as_deref()))
                .await
                .map(drop)
        }
        Action::CreateTeam { slug, name } => within("the new team", client.create_team(slug, name))
            .await
            .map(drop),
        Action::RenameTeam { slug, name } => within("the rename", client.rename_team(slug, name))
            .await
            .map(drop),
        Action::DeleteTeam { slug } => within("the delete", client.delete_team(slug)).await,
        Action::PutMembers { slug, members } => {
            within("the members", client.put_team_members(slug, members))
                .await
                .map(drop)
        }
        Action::EnableWebhook { id, on } => {
            within("the webhook", client.set_webhook_enabled(id, *on))
                .await
                .map(drop)
        }
        Action::RotateWebhook { id } => within("the rotation", client.rotate_webhook_secret(id))
            .await
            .map(|rotated| view.rotated = Some(rotated)),
        Action::DeleteWebhook { id } => within("the delete", client.delete_webhook(id)).await,
        Action::SaveWebhook { id: None, body } => {
            within("the new webhook", client.create_webhook(body))
                .await
                .map(|created| view.rotated = Some(created))
        }
        Action::SaveWebhook { id: Some(id), body } => {
            within("the webhook", client.update_webhook(id, body))
                .await
                .map(drop)
        }
        Action::CheckWebhook { body } => within("the check", client.check_webhook(body))
            .await
            .map(|check| view.webhook_check = Some(check)),
        Action::PreviewWebhook {
            body,
            sample,
            headers,
        } => within("the preview", client.preview_webhook(body, sample, headers))
            .await
            .map(|preview| view.webhook_preview = Some(preview)),
    }
}

async fn run(connector: Connector, mut rx: UnboundedReceiver<Msg>, shared: Arc<Mutex<Published>>) {
    let mut worker = Worker {
        connector,
        view: AdminView::default(),
        acted: 0,
    };
    let mut watch = Watch::default();
    let mut next = Instant::now();
    loop {
        let wait = next
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(500));
        match crate::net::timeout(wait, rx.recv()).await {
            Ok(Some(Msg::Watch(now))) => {
                if now != watch {
                    watch = now;
                    next = Instant::now();
                }
            }
            Ok(Some(Msg::Act(action))) => {
                worker.act(action).await;
                next = Instant::now();
            }
            Ok(Some(Msg::Reconnect)) => {
                worker.connector.reset();
                worker.view = AdminView {
                    acted: worker.view.acted.take(),
                    ..AdminView::default()
                };
                next = Instant::now();
            }
            Ok(None) => return,
            Err(_) => {}
        }
        if Instant::now() < next {
            continue;
        }
        if !watch.controller {
            next = Instant::now() + QUEUE;
            continue;
        }
        worker.refresh(&watch).await;
        publish(&shared, worker.view.clone());
        let shown = watch.schedules
            || watch.secrets
            || watch.secret.is_some()
            || watch.shares.is_some()
            || watch.decision.is_some()
            || watch.keys
            || watch.teams
            || watch.webhooks
            || watch.webhook_form.is_some();
        next = Instant::now() + if shown { SHOWN } else { QUEUE };
    }
}

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
    /// The webhook form, while open.
    pub webhook_form: Option<crate::webhook::WebhookForm>,
    /// The team the Teams page shows.
    pub team: Option<String>,
    /// The new-team form, while open.
    pub new_team: Option<crate::teams::TeamForm>,
    pub add_member: crate::teams::MemberForm,
    /// The team's next display name, while its rename form is open.
    pub rename_team: Option<TextEdit>,
    /// The mint form on the API keys page.
    pub new_key: crate::apikeys::KeyForm,
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
            webhook: None,
            webhook_form: None,
            team: None,
            new_team: None,
            add_member: crate::teams::MemberForm::default(),
            rename_team: None,
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
    fn arrived<'a>(&mut self, open: &'a [dto::DecisionSummary]) -> Vec<&'a dto::DecisionSummary> {
        let ids: HashSet<String> = open.iter().map(|d| d.id.clone()).collect();
        let new = match &self.seen {
            Some(seen) => open.iter().filter(|d| !seen.contains(&d.id)).collect(),
            None => Vec::new(),
        };
        self.seen = Some(ids);
        new
    }
}

/// The launch sheet's schedule half: when it fires, in which zone, owned by whom, and the
/// schedule it edits.
pub struct ScheduleForm {
    pub stored: Option<Box<dto::Schedule>>,
    pub cron: TextEdit,
    pub tz: TextEdit,
    pub owner: String,
}

impl ScheduleForm {
    pub fn new(owner: &str) -> ScheduleForm {
        let tz = jiff::tz::TimeZone::system()
            .iana_name()
            .unwrap_or("UTC")
            .to_string();
        ScheduleForm {
            stored: None,
            cron: TextEdit::new("0 9 * * MON-FRI"),
            tz: TextEdit::new(tz),
            owner: owner.to_string(),
        }
    }

    pub fn editing(schedule: &dto::Schedule) -> ScheduleForm {
        ScheduleForm {
            cron: TextEdit::new(&schedule.cron_expr),
            tz: TextEdit::new(if schedule.tz.is_empty() {
                "UTC"
            } else {
                &schedule.tz
            }),
            owner: schedule.owner_principal.clone().unwrap_or_default(),
            stored: Some(Box::new(schedule.clone())),
        }
    }

    /// The body that creates the schedule, or replaces the stored one keeping what the form
    /// does not show.
    pub fn body(
        &self,
        target: &crate::live::Target,
        params: &serde_json::Value,
        max_cost: f64,
        max_time: &str,
        digest: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let cron = self.cron.text.split_whitespace().collect::<Vec<_>>();
        if cron.len() != 5 {
            return Err("A cron expression has five fields, like 0 9 * * MON-FRI".to_string());
        }
        let tz = self.tz.text.trim();
        if tz.is_empty() {
            return Err("A time zone, like UTC or America/New_York".to_string());
        }
        let params = params
            .as_object()
            .map(dto::string_params)
            .unwrap_or_default();
        let mut body = match &self.stored {
            Some(stored) => stored.body(stored.enabled),
            None => serde_json::json!({"advance_dedupe": true, "enabled": true}),
        };
        let target_kind = match target {
            crate::live::Target::Playbook(_) => "adopted",
            crate::live::Target::Draft(_) => "draft_head",
        };
        let fields = [
            ("owner", serde_json::json!(self.owner)),
            ("playbook", serde_json::json!(target.id())),
            ("target_kind", serde_json::json!(target_kind)),
            ("params", serde_json::Value::Object(params)),
            ("max_cost", serde_json::json!(max_cost)),
            ("max_time", serde_json::json!(max_time)),
            ("cron_expr", serde_json::json!(cron.join(" "))),
            ("tz", serde_json::json!(tz)),
        ];
        for (name, value) in fields {
            body[name] = value;
        }
        if let Some(digest) = digest {
            body["schema_digest"] = serde_json::json!(digest);
        }
        Ok(body)
    }
}

/// The secret kinds a value can be typed in for, with what the form calls them.
pub const SECRET_KINDS: [(&str, &str); 6] = [
    ("opaque", "Token"),
    ("github_token", "GitHub token"),
    ("inference_api_key", "Model API key"),
    ("file", "File"),
    ("registry_authfile", "Registry auth"),
    ("kubeconfig", "Kubeconfig"),
];

/// A secret being registered from a typed value.
pub struct SecretForm {
    pub name: TextEdit,
    pub owner: String,
    pub kind: usize,
    pub agent_visible: bool,
    pub value: TextEdit,
    /// The environment variable a model API key lands in.
    pub variable: TextEdit,
}

impl SecretForm {
    pub fn new(owner: &str) -> SecretForm {
        SecretForm {
            name: TextEdit::new(""),
            owner: owner.to_string(),
            kind: 0,
            agent_visible: false,
            value: TextEdit::new(""),
            variable: TextEdit::new("OPENAI_API_KEY"),
        }
    }

    pub fn kind(&self) -> &'static str {
        SECRET_KINDS[self.kind.min(SECRET_KINDS.len() - 1)].0
    }

    /// Whether the value is a document rather than one line.
    pub fn multiline(&self) -> bool {
        matches!(self.kind(), "file" | "registry_authfile" | "kubeconfig")
    }

    /// The registration body, or what is wrong with the form.
    pub fn body(&self) -> Result<serde_json::Value, String> {
        let name = self.name.text.trim();
        if name.is_empty() || name.len() > 96 {
            return Err("A name of 1 to 96 characters".to_string());
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err("Names are lowercase letters, digits, '.', '_' and '-'".to_string());
        }
        if self.value.text.is_empty() {
            return Err("No value".to_string());
        }
        let value = match self.kind() {
            "inference_api_key" => {
                let variable = self.variable.text.trim();
                let valid = variable
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
                    && variable
                        .chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                if !valid {
                    return Err(format!("{variable:?} is not an environment variable name"));
                }
                serde_json::json!({ variable: self.value.text.trim() }).to_string()
            }
            "opaque" | "github_token" => self.value.text.trim().to_string(),
            _ => self.value.text.clone(),
        };
        let visible = self.agent_visible && self.kind() == "opaque";
        Ok(serde_json::json!({
            "name": name,
            "owner": self.owner,
            "kind": self.kind(),
            "visibility": if visible { "agent_visible" } else { "broker_only" },
            "value": value,
        }))
    }
}

/// What a share form's `until` sends: the end of that day in UTC, blank for no end, or why it is
/// not a date.
pub fn not_after(until: &str) -> Result<Option<String>, String> {
    let day = until.trim();
    if day.is_empty() {
        return Ok(None);
    }
    day.parse::<jiff::civil::Date>()
        .map(|date| Some(format!("{date}T23:59:59Z")))
        .map_err(|_| format!("{day} is not a date like 2026-12-31"))
}

/// Whether `grantee` names a principal a share can go to.
pub fn valid_grantee(grantee: &str) -> bool {
    match grantee.trim().split_once(':') {
        Some(("user" | "team", name)) => {
            !name.is_empty() && !name.contains(':') && !name.chars().any(char::is_whitespace)
        }
        _ => false,
    }
}

/// `in 3h`, `in 12m` or `5d ago` from Unix seconds `now` to the RFC 3339 `stamp`; empty when it
/// does not parse.
pub fn relative(stamp: &str, now: i64) -> String {
    let Ok(at) = stamp.parse::<jiff::Timestamp>() else {
        return String::new();
    };
    let delta = at.as_second() - now;
    let span = delta.unsigned_abs();
    let amount = match span {
        0..60 => return "now".to_string(),
        60..3_600 => format!("{}m", span / 60),
        3_600..86_400 => format!("{}h", span / 3_600),
        _ => format!("{}d", span / 86_400),
    };
    if delta > 0 {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

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
            teams: on(crate::Page::Teams),
            team_audit: self.owned_team().filter(|_| on(crate::Page::Teams)),
        };
        if self.remote.admin_sent.as_ref() != Some(&watch) {
            self.remote.admin_client.watch(watch.clone());
            self.remote.admin_sent = Some(watch);
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
                    Did::Enable
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
                    | Did::PreviewWebhook => {}
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
fn fixture() -> Result<dto::Decision, String> {
    serde_json::from_str(include_str!("../fixtures/decision.json"))
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

#[cfg(test)]
mod tests {
    use crate::admin::*;

    #[test]
    fn the_decision_fixture_reads_as_an_open_answerable_request() {
        let decision = fixture().expect("fixture");
        assert_eq!(decision.state, "open");
        assert!(decision.can_answer);
        assert_eq!(decision.questions.len(), 2);
        let mut desk = Desk::default();
        assert_eq!(desk.unanswered(&decision), ["ship", "repos"]);
        desk.toggle_pick("ship", "yes", false);
        desk.toggle_pick("repos", "llm-d/llm-d", true);
        assert!(desk.unanswered(&decision).is_empty());
    }

    #[test]
    fn everything_watched_is_read_at_once() {
        let slow = Duration::from_secs(1);
        let (url, _) = crate::testserver::serve(move |request| {
            std::thread::sleep(slow);
            match request.split_whitespace().nth(1) {
                Some("/api/decisions") => r#"{"open":[]}"#.into(),
                Some("/api/approvals") => "{}".into(),
                _ => "[]".into(),
            }
        });
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client =
            Client::spawn(net.connector(crate::testserver::target("t", &url, keys.path())));
        let asked = Instant::now();
        client.watch(Watch {
            controller: true,
            schedules: true,
            secrets: true,
            ..Watch::default()
        });
        let view = loop {
            if let Some((_, view)) = client.take(0) {
                break view;
            }
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "nothing was read"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            asked.elapsed() < slow * 3,
            "four reads of {slow:?} each took {:?}",
            asked.elapsed()
        );
        assert_eq!(view.decisions, Some(Ok(Vec::new())));
        assert_eq!(view.approvals.map(|a| a.map(|a| a.waiting())), Some(Ok(0)));
        assert_eq!(view.schedules.map(|s| s.map(|s| s.len())), Some(Ok(0)));
        assert_eq!(view.secrets.map(|s| s.map(|s| s.len())), Some(Ok(0)));
        assert!(view.secret.is_none() && view.shares.is_none() && view.decision.is_none());
    }

    /// The newest view once `done` holds for it.
    fn until(client: &Client, done: impl Fn(&AdminView) -> bool) -> AdminView {
        let asked = Instant::now();
        loop {
            if let Some((_, view)) = client.take(0)
                && done(&view)
            {
                return view;
            }
            assert!(
                asked.elapsed() < Duration::from_secs(10),
                "the view never got there"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn keys_are_read_only_while_their_page_shows_and_a_minted_secret_lasts_as_long() {
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&requests);
        let (url, _) = crate::testserver::serve(move |request| {
            let line: Vec<&str> = request.split_whitespace().take(2).collect();
            seen.lock().unwrap().push(line.join(" "));
            match line[..] {
                ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
                ["GET", "/api/approvals"] => "{}".into(),
                ["GET", "/api/keys"] => r#"[{"id":"k1","name":"laptop",
                    "created_at":"2026-11-01T00:00:00Z","expires_at":null,
                    "last_used_at":null,"revoked_at":null}]"#
                    .into(),
                ["POST", "/api/keys"] => r#"{"id":"k2","name":"ci",
                    "created_at":"2026-11-10T00:00:00Z","expires_at":null,"last_used_at":null,
                    "revoked_at":null,"secret":"crk_once","mcp_url":null}"#
                    .into(),
                ["GET", "/api/me/mcp-sign-ins"] => r#"{"enabled":true,"sign_ins":[]}"#.into(),
                ["DELETE", "/api/me/mcp-sign-ins?client_id=dyn-1"] => r#"{"revoked":1}"#.into(),
                _ => "[]".into(),
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn(net.connector(crate::testserver::target("t", &url, dir.path())));
        let elsewhere = Watch {
            controller: true,
            ..Watch::default()
        };
        let on_page = Watch {
            keys: true,
            ..elsewhere.clone()
        };

        client.watch(elsewhere.clone());
        let view = until(&client, |view| view.decisions.is_some());
        assert!(view.keys.is_none() && view.mcp.is_none());
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("/api/keys")),
            "keys were read off their page"
        );

        client.watch(on_page);
        let view = until(&client, |view| view.keys.is_some() && view.mcp.is_some());
        assert_eq!(view.keys.map(|k| k.map(|k| k.len())), Some(Ok(1)));
        assert_eq!(view.mcp.map(|m| m.map(|m| m.enabled)), Some(Ok(true)));

        client.act(Action::MintKey {
            name: "ci".into(),
            days: None,
        });
        let view = until(&client, |view| view.acted.is_some());
        assert_eq!(
            view.acted.map(|a| (a.did, a.result)),
            Some((Did::MintKey, Ok("Minted ci".to_string())))
        );
        assert_eq!(view.minted.map(|m| m.secret), Some("crk_once".to_string()));

        client.act(Action::RevokeKey { id: "k1".into() });
        until(&client, |view| {
            view.acted.as_ref().is_some_and(|a| a.did == Did::RevokeKey)
        });
        client.act(Action::SignOutMcp {
            client: Some("dyn-1".into()),
        });
        let view = until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::SignOutMcp)
        });
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Signed out".to_string()))
        );
        assert!(
            view.minted.is_some(),
            "the secret went while its page showed"
        );
        let sent = requests.lock().unwrap().clone();
        for change in [
            "POST /api/keys",
            "DELETE /api/keys/k1",
            "DELETE /api/me/mcp-sign-ins?client_id=dyn-1",
        ] {
            assert!(sent.iter().any(|r| r == change), "{change} was not sent");
        }

        client.watch(elsewhere);
        until(&client, |view| view.minted.is_none());
    }

    #[test]
    fn teams_are_read_on_their_page_with_the_audit_of_the_one_asked_for() {
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&requests);
        let (url, _) = crate::testserver::serve(move |request| {
            let line: Vec<&str> = request.split_whitespace().take(2).collect();
            seen.lock().unwrap().push(line.join(" "));
            match line[..] {
                ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
                ["GET", "/api/approvals"] => "{}".into(),
                ["GET", "/api/teams"] => r#"[{"slug":"infra","display_name":"Infra",
                    "members":[{"kind":"user","member":"wynn","role":"owner"}],
                    "my_role":"owner","reachable":true}]"#
                    .into(),
                ["GET", "/api/teams/infra/audit"] => r#"[{"id":7,"at":"2026-11-01T00:00:00Z",
                    "actor":"user:wynn","action":"team:create","decision":"allow","rule":"owner"}]"#
                    .into(),
                ["POST", "/api/teams"] | ["PUT", _] => {
                    r#"{"slug":"infra","display_name":"Infra"}"#.into()
                }
                _ => "[]".into(),
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn(net.connector(crate::testserver::target("t", &url, dir.path())));
        let listed = Watch {
            controller: true,
            teams: true,
            ..Watch::default()
        };

        client.watch(listed.clone());
        let view = until(&client, |view| view.teams.is_some());
        assert_eq!(
            view.teams
                .map(|teams| teams.map(|teams| (teams.len(), teams[0].my_role))),
            Some(Ok((1, Some(dto::TeamRole::Owner))))
        );
        assert!(view.team_audit.is_none());
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.ends_with("/audit")),
            "an audit trail nobody asked for was read"
        );

        client.watch(Watch {
            team_audit: Some("infra".into()),
            ..listed.clone()
        });
        let view = until(&client, |view| view.team_audit.is_some());
        let (slug, audit) = view.team_audit.unwrap();
        assert_eq!(slug, "infra");
        assert_eq!(
            audit.map(|a| a.map(|a| a[0].action.clone())),
            Some(Ok("team:create".to_string()))
        );

        let steps = [
            (
                Action::CreateTeam {
                    slug: "infra".into(),
                    name: "Infra".into(),
                },
                Did::CreateTeam,
                "Created infra",
                "POST /api/teams",
            ),
            (
                Action::RenameTeam {
                    slug: "infra".into(),
                    name: "Platform".into(),
                },
                Did::RenameTeam,
                "Renamed to Platform",
                "PUT /api/teams/infra",
            ),
            (
                Action::PutMembers {
                    slug: "infra".into(),
                    members: Vec::new(),
                },
                Did::PutMembers,
                "Saved the members",
                "PUT /api/teams/infra/members",
            ),
            (
                Action::DeleteTeam {
                    slug: "infra".into(),
                },
                Did::DeleteTeam,
                "Deleted infra",
                "DELETE /api/teams/infra",
            ),
        ];
        for (action, did, said, sent) in steps {
            client.act(action);
            let view = until(&client, |view| {
                view.acted.as_ref().is_some_and(|a| a.did == did)
            });
            assert_eq!(view.acted.map(|a| a.result), Some(Ok(said.to_string())));
            assert!(
                requests.lock().unwrap().iter().any(|r| r == sent),
                "{sent} was not sent"
            );
        }

        client.watch(listed);
        until(&client, |view| view.team_audit.is_none());
    }

    #[test]
    fn webhooks_are_read_on_triggers_and_a_rotated_secret_lasts_while_its_webhook_shows() {
        const HOOK: &str = r#"{"id":"w1","playbook":"triage","verifier":"path_token",
            "enabled":true,"max_launches_per_hour":3}"#;
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&requests);
        let (url, _) = crate::testserver::serve(move |request| {
            let line: Vec<&str> = request.split_whitespace().take(2).collect();
            seen.lock().unwrap().push(line.join(" "));
            match line[..] {
                ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
                ["GET", "/api/approvals"] => "{}".into(),
                ["GET", "/api/webhooks"] => format!("[{HOOK}]"),
                ["GET", "/api/webhooks/w1/deliveries?limit=20"] => r#"[{"id":"d1",
                    "received_at":"2026-11-01T00:00:00Z","outcome":"filtered",
                    "reason":"the filter was false","launch_key":null}]"#
                    .into(),
                ["POST", "/api/webhooks/w1/secret"] => {
                    format!(r#"{{"webhook":{HOOK},"secret":"whsec_once","delivery_url":null}}"#)
                }
                ["POST", "/api/webhooks/w1/enabled"] => HOOK.into(),
                _ => "[]".into(),
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn(net.connector(crate::testserver::target("t", &url, dir.path())));
        let listed = Watch {
            controller: true,
            schedules: true,
            webhooks: true,
            ..Watch::default()
        };
        let opened = Watch {
            webhook: Some("w1".into()),
            ..listed.clone()
        };

        client.watch(listed.clone());
        let view = until(&client, |view| view.webhooks.is_some());
        assert_eq!(
            view.webhooks
                .map(|hooks| hooks.map(|hooks| hooks[0].playbook.clone())),
            Some(Ok("triage".to_string()))
        );
        assert!(view.deliveries.is_none());
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.contains("/deliveries")),
            "deliveries nobody opened were read"
        );

        client.watch(opened);
        let view = until(&client, |view| view.deliveries.is_some());
        let (id, list) = view.deliveries.unwrap();
        assert_eq!(id, "w1");
        assert_eq!(
            list.map(|list| list.map(|list| list[0].outcome.clone())),
            Some(Ok("filtered".to_string()))
        );

        client.act(Action::RotateWebhook { id: "w1".into() });
        let view = until(&client, |view| view.rotated.is_some());
        assert_eq!(
            view.rotated.map(|r| r.secret),
            Some("whsec_once".to_string())
        );
        client.act(Action::EnableWebhook {
            id: "w1".into(),
            on: false,
        });
        let view = until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::EnableWebhook)
        });
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Paused the webhook".to_string()))
        );
        assert!(
            view.rotated.is_some(),
            "the secret went while its webhook showed"
        );
        client.act(Action::DeleteWebhook { id: "w1".into() });
        until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::DeleteWebhook)
        });
        let sent = requests.lock().unwrap().clone();
        for change in [
            "POST /api/webhooks/w1/secret",
            "POST /api/webhooks/w1/enabled",
            "DELETE /api/webhooks/w1",
        ] {
            assert!(sent.iter().any(|r| r == change), "{change} was not sent");
        }

        client.watch(listed);
        let view = until(&client, |view| view.deliveries.is_none());
        assert!(
            view.rotated.is_some(),
            "closing the details dropped the secret"
        );
        client.watch(Watch {
            controller: true,
            ..Watch::default()
        });
        until(&client, |view| view.rotated.is_none());
    }

    #[test]
    fn a_webhook_form_reads_its_schema_and_presets_and_a_created_secret_is_kept() {
        const HOOK: &str = r#"{"id":"w2","playbook":"triage","verifier":"path_token"}"#;
        let requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&requests);
        let (url, _) = crate::testserver::serve(move |request| {
            let line: Vec<&str> = request.split_whitespace().take(2).collect();
            seen.lock().unwrap().push(line.join(" "));
            match line[..] {
                ["GET", "/api/decisions"] => r#"{"open":[]}"#.into(),
                ["GET", "/api/approvals"] => "{}".into(),
                ["GET", "/api/playbooks/triage/schema"] => {
                    r#"{"properties":{"repo":{"type":"string"}},"required":["repo"]}"#.into()
                }
                ["GET", "/api/webhooks/presets"] => r#"[{"id":"quay-push","title":"quay.io push",
                    "verifier":"path_token","header":null,"filter":"true","dedupe":"delivery",
                    "derive":{}}]"#
                    .into(),
                ["POST", "/api/webhooks"] => {
                    format!(r#"{{"webhook":{HOOK},"secret":"whsec_new","delivery_url":null}}"#)
                }
                ["PUT", "/api/webhooks/w2"] => HOOK.into(),
                ["POST", "/api/webhooks/check"] => r#"{"diagnostics":[]}"#.into(),
                ["POST", "/api/webhooks/preview"] => r#"{"filter":{"value":true},
                    "dedupe":{"value":"d1"},"derive":{"error":"no such key"}}"#
                    .into(),
                _ => "[]".into(),
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn(net.connector(crate::testserver::target("t", &url, dir.path())));
        let listed = Watch {
            controller: true,
            webhooks: true,
            ..Watch::default()
        };
        let filling = Watch {
            webhook_form: Some("triage".into()),
            ..listed.clone()
        };
        let mut form = crate::webhook::WebhookForm::new("triage");

        client.watch(listed.clone());
        let view = until(&client, |view| view.webhooks.is_some());
        assert!(view.webhook_schema.is_none() && view.presets.is_none());

        client.watch(filling);
        let view = until(&client, |view| {
            view.webhook_schema.is_some() && view.presets.is_some()
        });
        let (playbook, schema) = view.webhook_schema.unwrap();
        form.lay_out(&playbook, &crate::form::fields(&schema.unwrap().unwrap()));
        assert_eq!(form.choices[0].name, "repo");
        assert_eq!(view.presets.unwrap().unwrap()[0].id, "quay-push");
        let body = Box::new(form.body().unwrap());

        client.act(Action::CheckWebhook { body: body.clone() });
        let view = until(&client, |view| view.webhook_check.is_some());
        assert!(view.webhook_check.unwrap().diagnostics.is_empty());
        client.act(Action::PreviewWebhook {
            body: body.clone(),
            sample: serde_json::json!({}),
            headers: BTreeMap::new(),
        });
        let view = until(&client, |view| view.webhook_preview.is_some());
        let preview = view.webhook_preview.unwrap();
        assert_eq!(preview.filter.value, Some(serde_json::json!(true)));
        assert_eq!(preview.derive.error.as_deref(), Some("no such key"));

        client.act(Action::SaveWebhook {
            id: None,
            body: body.clone(),
        });
        let view = until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.did == Did::SaveWebhook)
        });
        assert_eq!(
            view.acted.map(|a| a.result),
            Some(Ok("Created the webhook".to_string()))
        );
        assert_eq!(
            view.rotated.map(|r| r.secret),
            Some("whsec_new".to_string())
        );
        client.act(Action::SaveWebhook {
            id: Some("w2".into()),
            body,
        });
        until(&client, |view| {
            view.acted
                .as_ref()
                .is_some_and(|a| a.result == Ok("Saved the webhook".to_string()))
        });
        let sent = requests.lock().unwrap().clone();
        for change in ["POST /api/webhooks", "PUT /api/webhooks/w2"] {
            assert!(sent.iter().any(|r| r == change), "{change} was not sent");
        }

        client.watch(listed);
        let view = until(&client, |view| {
            view.webhook_check.is_none() && view.webhook_preview.is_none()
        });
        assert!(view.webhook_schema.is_none());
        assert_eq!(
            view.rotated.map(|r| r.secret),
            Some("whsec_new".to_string()),
            "closing the form dropped the new webhook's secret"
        );
    }

    fn summary(id: &str) -> dto::DecisionSummary {
        dto::DecisionSummary {
            id: id.into(),
            run_id: "run-1".into(),
            task: "review".into(),
            launch_key: Some("playbook:demo:1".into()),
            questions: vec!["ship".into()],
            opened_at: "2026-11-08T10:00:00Z".into(),
            expires_at: "2026-11-09T10:00:00Z".into(),
        }
    }

    #[test]
    fn the_first_queue_announces_nothing_and_later_ones_only_what_is_new() {
        let mut desk = Desk::default();
        assert!(desk.arrived(&[summary("a")]).is_empty());
        let next = [summary("a"), summary("b")];
        let new: Vec<&str> = desk.arrived(&next).iter().map(|d| d.id.as_str()).collect();
        assert_eq!(new, ["b"]);
        assert!(desk.arrived(&next).is_empty(), "announced once");
        assert!(desk.arrived(&[]).is_empty());
        let returned = [summary("a")];
        let back: Vec<&str> = desk
            .arrived(&returned)
            .iter()
            .map(|d| d.id.as_str())
            .collect();
        assert_eq!(
            back,
            ["a"],
            "a request that left the queue and came back is new again"
        );
    }

    #[test]
    fn a_single_answer_question_keeps_one_pick_and_a_multiple_one_keeps_many() {
        let mut desk = Desk::default();
        desk.toggle_pick("ship", "yes", false);
        desk.toggle_pick("ship", "no", false);
        assert_eq!(desk.picks["ship"], ["no"]);
        desk.toggle_pick("ship", "no", false);
        assert!(desk.picks["ship"].is_empty(), "picking it again clears it");
        desk.toggle_pick("tags", "a", true);
        desk.toggle_pick("tags", "b", true);
        assert_eq!(desk.picks["tags"], ["a", "b"]);
        desk.toggle_pick("tags", "a", true);
        assert_eq!(desk.picks["tags"], ["b"]);
    }

    #[test]
    fn picking_another_decision_drops_the_picks() {
        let mut desk = Desk::default();
        desk.pick_decision("a");
        desk.toggle_pick("ship", "yes", false);
        desk.pick_decision("a");
        assert_eq!(desk.picks.len(), 1, "the same request keeps them");
        desk.pick_decision("b");
        assert!(desk.picks.is_empty());
    }

    fn form(kind: &str, name: &str, value: &str) -> SecretForm {
        let mut form = SecretForm::new("team:infra");
        form.kind = SECRET_KINDS
            .iter()
            .position(|(k, _)| *k == kind)
            .expect("kind");
        form.name = TextEdit::new(name);
        form.value = TextEdit::new(value);
        form
    }

    #[test]
    fn a_secret_form_sends_the_registration_the_controller_takes() {
        let body = form("opaque", "gh-token", " ghp_x \n")
            .body()
            .expect("valid");
        assert_eq!(
            body,
            serde_json::json!({"name": "gh-token", "owner": "team:infra", "kind": "opaque",
                "visibility": "broker_only", "value": "ghp_x"})
        );
        let mut visible = form("opaque", "t", "v");
        visible.agent_visible = true;
        assert_eq!(
            visible.body().expect("valid")["visibility"],
            "agent_visible"
        );
        let mut file = form("file", "conf", "a\nb\n");
        file.agent_visible = true;
        let body = file.body().expect("valid");
        assert_eq!(body["value"], "a\nb\n", "a document keeps its bytes");
        assert_eq!(
            body["visibility"], "broker_only",
            "only a token can be agent visible"
        );
        let key = form("inference_api_key", "openai", "sk-1")
            .body()
            .expect("valid");
        assert_eq!(key["value"], r#"{"OPENAI_API_KEY":"sk-1"}"#);
    }

    #[test]
    fn a_secret_form_refuses_what_the_controller_would() {
        assert!(form("opaque", "", "v").body().is_err());
        assert!(form("opaque", "Upper", "v").body().is_err());
        assert!(form("opaque", "has space", "v").body().is_err());
        assert!(form("opaque", &"a".repeat(97), "v").body().is_err());
        assert!(form("opaque", "ok", "").body().is_err());
        let mut key = form("inference_api_key", "k", "sk");
        key.variable = TextEdit::new("openai key");
        assert!(key.body().is_err());
    }

    #[test]
    fn a_new_schedule_body_carries_the_form_and_defaults() {
        let mut form = ScheduleForm::new("team:infra");
        form.cron = TextEdit::new(" 0  6 * * 1 ");
        form.tz = TextEdit::new("America/New_York");
        let body = form
            .body(
                &crate::live::Target::Draft("cve".into()),
                &serde_json::json!({"limit": 5, "repo": "a/b"}),
                3.0,
                "45m",
                Some("sha256:d"),
            )
            .expect("valid");
        assert_eq!(
            body,
            serde_json::json!({"advance_dedupe": true, "enabled": true, "owner": "team:infra",
                "playbook": "cve", "target_kind": "draft_head",
                "params": {"limit": "5", "repo": "a/b"}, "max_cost": 3.0, "max_time": "45m",
                "cron_expr": "0 6 * * 1", "tz": "America/New_York",
                "schema_digest": "sha256:d"})
        );
    }

    #[test]
    fn an_edited_schedule_keeps_what_the_form_does_not_show() {
        let stored: dto::Schedule = serde_json::from_str(
            r#"{"id":"s1","playbook":"report","cron_expr":"0 9 * * *","enabled":false,
                "next_due_at":null,"last_fired_at":null,"consecutive_failures":0,
                "owner_principal":"user:wren","target_kind":"adopted","params":{"a":"1"},
                "max_cost":1.0,"max_time":"10m","advance_dedupe":false,"tz":"UTC",
                "dispatch_target":"wharf","agent_provider":"vertex","agent_model":"m",
                "cursor":{"from":"result","param":"since","path":null}}"#,
        )
        .expect("wire shape");
        let mut form = ScheduleForm::editing(&stored);
        form.owner = "team:infra".into();
        let body = form
            .body(
                &crate::live::Target::Playbook("report".into()),
                &serde_json::json!({"a": "2"}),
                2.0,
                "20m",
                None,
            )
            .expect("valid");
        assert_eq!(body["owner"], "team:infra");
        assert_eq!(body["params"], serde_json::json!({"a": "2"}));
        assert_eq!(body["enabled"], false, "editing does not resume it");
        assert_eq!(body["advance_dedupe"], false);
        assert_eq!(body["dispatch_target"], "wharf");
        assert_eq!(body["provider"], "vertex");
        assert_eq!(body["cursor"]["param"], "since");
        assert_eq!(body["target_kind"], "adopted");
    }

    #[test]
    fn a_schedule_form_refuses_a_bad_cron_or_no_zone() {
        let target = crate::live::Target::Playbook("p".into());
        let params = serde_json::json!({});
        let mut form = ScheduleForm::new("user:me");
        form.cron = TextEdit::new("0 9 * *");
        assert!(form.body(&target, &params, 1.0, "5m", None).is_err());
        form.cron = TextEdit::new("0 9 * * *");
        form.tz = TextEdit::new(" ");
        assert!(form.body(&target, &params, 1.0, "5m", None).is_err());
    }

    #[test]
    fn a_share_ends_at_the_end_of_its_day_or_never() {
        assert_eq!(not_after(" "), Ok(None));
        assert_eq!(
            not_after("2026-12-31"),
            Ok(Some("2026-12-31T23:59:59Z".to_string()))
        );
        assert!(not_after("next week").is_err());
        assert!(not_after("2026-02-30").is_err());
    }

    #[test]
    fn a_grantee_is_a_user_or_a_team() {
        assert!(valid_grantee("user:wren"));
        assert!(valid_grantee(" team:infra "));
        for bad in ["wren", "user:", "group:x", "user:a b", "user:a:b", ""] {
            assert!(!valid_grantee(bad), "{bad}");
        }
    }

    #[test]
    fn times_read_relative_to_now() {
        let now = "2026-11-08T12:00:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_second();
        assert_eq!(relative("2026-11-08T15:00:00Z", now), "in 3h");
        assert_eq!(relative("2026-11-08T11:48:00Z", now), "12m ago");
        assert_eq!(relative("2026-11-03T12:00:00Z", now), "5d ago");
        assert_eq!(relative("2026-11-08T12:00:30Z", now), "now");
        assert_eq!(relative("soon", now), "");
    }
}

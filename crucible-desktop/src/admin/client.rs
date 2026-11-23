//! The management worker: it reads everything a watch names at once, carries out actions, and
//! publishes what it read.

use crate::admin::act::act;
use crate::admin::{Acted, Action, AdminView, InStatus, SecretView, Watch};
use crate::net::{Connector, Task, within};
use crux::client::Shared;
use crux::dto;
use futures_util::future::join_all;
use rune::Waker;
use rune::time::Instant;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// How often what is on screen is read again.
const SHOWN: Duration = Duration::from_secs(15);
/// How often the decision queue is read while nothing shows it, for the badge and notifications.
const QUEUE: Duration = Duration::from_secs(30);
/// How many of a webhook's newest deliveries are read.
const DELIVERIES: i64 = 20;
/// How many of the newest transitions the Activity page reads.
const EVENTS: usize = 200;
/// How many open decision requests one pass reads in full.
pub(in crate::admin) const DETAILS: usize = 4;

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
            Ok(c) => {
                let read = read(&c.client, watch).await;
                if let Ok(open) = &read.decisions {
                    let wanted = unread(open, &self.view.details);
                    let details = join_all(
                        wanted
                            .iter()
                            .map(|id| within("the decision request", c.client.decision(id))),
                    )
                    .await;
                    self.view
                        .details
                        .retain(|id, _| open.iter().any(|decision| &decision.id == id));
                    for decision in details.into_iter().flatten() {
                        self.view.details.insert(decision.id.clone(), decision);
                    }
                }
                read
            }
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
        if let Some(providers) = read.providers {
            self.view.providers = Some(providers);
        }
        if let Some(blueprints) = read.blueprints {
            self.view.blueprints = Some(blueprints);
        }
        if let Some(version) = read.version {
            self.view.version = Some(version);
        }
        self.view.issues = read.issues.map(|(status, issues)| (status, Some(issues)));
        self.view.issue = read.issue.map(|(key, issue)| (key, Some(issue)));
        if !watch.blueprints {
            self.view.used = None;
        }
        if let Some(events) = read.events {
            self.view.events = Some(events);
        }
        if let Some(sets) = read.policy_sets {
            self.view.policy_sets = Some(sets);
        }
        if !watch.policy {
            self.view.stored_policy = None;
            self.view.explained = None;
        }
        if let Some((config, access, autopilot, repos)) = read.admin {
            self.view.config = Some(config);
            self.view.access = Some(access);
            self.view.autopilot = Some(autopilot);
            self.view.repos = Some(repos);
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

/// The first few of `open` not yet in `details`.
pub(in crate::admin) fn unread(
    open: &[dto::DecisionSummary],
    details: &BTreeMap<String, dto::Decision>,
) -> Vec<String> {
    open.iter()
        .filter(|decision| !details.contains_key(&decision.id))
        .take(DETAILS)
        .map(|decision| decision.id.clone())
        .collect()
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
    providers: Option<Result<Vec<dto::Provider>, String>>,
    blueprints: Option<Result<Vec<dto::Blueprint>, String>>,
    events: Option<Result<Vec<dto::Event>, String>>,
    admin: Option<Controller>,
    policy_sets: Option<Result<Vec<dto::PolicySet>, String>>,
    issues: Option<(InStatus, Result<Vec<dto::Issue>, String>)>,
    issue: Option<(String, Result<dto::IssueDetail, String>)>,
    version: Option<Result<dto::Version, String>>,
}

/// What the Admin page reads: settings, access lists, the autopilot flag and the watched repos.
type Controller = (
    Result<dto::ControllerConfig, String>,
    Result<dto::Access, String>,
    Result<dto::Autopilot, String>,
    Result<Vec<dto::WatchedRepo>, String>,
);

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
            providers: watch.providers.then(|| Err(why.clone())),
            blueprints: watch.blueprints.then(|| Err(why.clone())),
            events: watch.activity.then(|| Err(why.clone())),
            policy_sets: watch.policy.then(|| Err(why.clone())),
            issues: watch
                .issues
                .clone()
                .map(|status| (status, Err(why.clone()))),
            issue: watch.issue.clone().map(|key| (key, Err(why.clone()))),
            version: watch.about.then(|| Err(why.clone())),
            admin: watch.admin.then(|| {
                (
                    Err(why.clone()),
                    Err(why.clone()),
                    Err(why.clone()),
                    Err(why.clone()),
                )
            }),
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
    let issues = async {
        let status = watch.issues.as_ref()?;
        let list = within("the issues", client.lane_issues(status.as_deref())).await;
        Some((status.clone(), list))
    };
    let issue = async {
        let key = watch.issue.as_ref()?;
        let detail = within("the issue", client.issue::<dto::IssueDetail>(key)).await;
        Some((key.clone(), detail))
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
        providers,
        blueprints,
        events,
        admin,
        policy_sets,
        issues,
        issue,
        version,
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
        when(watch.providers, within("the providers", client.providers())),
        when(
            watch.blueprints,
            within("the blueprints", client.blueprints()),
        ),
        when(
            watch.activity,
            within("the activity", client.events(EVENTS))
        ),
        when(watch.admin, async {
            tokio::join!(
                within("the settings", client.config()),
                within("the access lists", client.access()),
                within("the autopilot flag", client.autopilot()),
                within("the repos", client.repos()),
            )
        }),
        when(
            watch.policy,
            within("the policy sets", client.policy_sets()),
        ),
        issues,
        issue,
        when(
            watch.about,
            within("the version", client.version::<dto::Version>()),
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
        providers,
        blueprints,
        events,
        admin,
        policy_sets,
        issues,
        issue,
        version,
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
            || watch.webhook_form.is_some()
            || watch.providers
            || watch.blueprints
            || watch.activity
            || watch.admin
            || watch.policy
            || watch.issues.is_some();
        next = Instant::now() + if shown { SHOWN } else { QUEUE };
    }
}

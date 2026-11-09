//! Schedules, secrets, shares and decision requests, read and changed through crux on a worker
//! thread of their own so a slow call never holds up the launch list.

use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crux::client::Shared;
use crux::dto;
use rune::{TextEdit, Waker};

use crate::live::{Cache, Connection};

/// How often what is on screen is read again.
const SHOWN: Duration = Duration::from_secs(15);
/// How often the decision queue is read while nothing shows it, for the badge and notifications.
const QUEUE: Duration = Duration::from_secs(30);

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
    pub acted: Option<Acted>,
}

enum Msg {
    Watch(Watch),
    Act(Action),
    Reconnect,
}

/// The UI-side handle to the worker. Dropping it stops the thread.
pub struct Client {
    tx: Sender<Msg>,
    shared: Arc<Mutex<Published>>,
}

#[derive(Default)]
struct Published {
    view: AdminView,
    version: u64,
    waker: Option<Waker>,
}

impl Client {
    pub fn spawn(target: crate::profile::Target) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Published::default()));
        let worker = Arc::clone(&shared);
        if let Err(error) = std::thread::Builder::new()
            .name("crucible-playbooks-admin".into())
            .spawn(move || run(target, rx, worker))
        {
            let view = AdminView {
                decisions: Some(Err(format!("starting the admin worker: {error}"))),
                ..AdminView::default()
            };
            publish(&shared, view);
        }
        Client { tx, shared }
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
    target: crate::profile::Target,
    cache: Option<Cache>,
    connection: Option<Connection>,
    view: AdminView,
    acted: u64,
}

impl Worker {
    fn connection(&mut self) -> Result<&Connection, String> {
        if self.connection.is_none() {
            self.connection = Some(Connection::open(&self.target, self.cache.as_ref())?);
        }
        self.connection
            .as_ref()
            .ok_or_else(|| "no controller connection".to_string())
    }

    fn refresh(&mut self, watch: &Watch) {
        let read = match self.connection() {
            Ok(c) => read(c, watch),
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
    }

    fn act(&mut self, action: Action) {
        let what = action.what();
        let result = self.connection().and_then(|c| act(c, &action));
        self.acted += 1;
        self.view.acted = Some(Acted {
            seq: self.acted,
            did: action.did(),
            result: result.map(|()| what),
        });
    }
}

/// One pass over what `Watch` asks for.
struct Read {
    decisions: Result<Vec<dto::DecisionSummary>, String>,
    approvals: Result<dto::Approvals, String>,
    schedules: Option<Result<Vec<dto::Schedule>, String>>,
    secrets: Option<Result<Vec<dto::Secret>, String>>,
    secret: Option<(String, Result<SecretView, String>)>,
    shares: Option<(Shared, Result<Vec<dto::Share>, String>)>,
    decision: Option<(String, Result<dto::Decision, String>)>,
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
        }
    }
}

fn read(c: &Connection, watch: &Watch) -> Read {
    let client = &c.client;
    Read {
        decisions: c
            .call("the decision queue", client.decisions())
            .map(|queue| queue.open),
        approvals: c.call("the approval queue", client.approvals()),
        schedules: watch
            .schedules
            .then(|| c.call("the schedules", client.schedules())),
        secrets: watch
            .secrets
            .then(|| c.call("the secrets", client.secrets())),
        secret: watch.secret.clone().map(|id| {
            let view = c.call("the secret", client.secret(&id)).and_then(|detail| {
                let audit = c.call("its audit trail", client.secret_audit(&id))?;
                Ok(SecretView { detail, audit })
            });
            (id, view)
        }),
        shares: watch.shares.clone().map(|of| {
            let shares = c.call("the shares", client.shares(&of));
            (of, shares)
        }),
        decision: watch.decision.clone().map(|id| {
            let decision = c.call("the decision request", client.decision(&id));
            (id, decision)
        }),
    }
}

fn act(c: &Connection, action: &Action) -> Result<(), String> {
    let client = &c.client;
    match action {
        Action::Enable { schedule, on } => c
            .call(
                "saving the schedule",
                client.update_schedule::<serde_json::Value>(&schedule.id, &schedule.body(*on)),
            )
            .map(drop),
        Action::Grant {
            of,
            grantee,
            role,
            not_after,
        } => c
            .call(
                "the share",
                client.grant_share(of, grantee, role, not_after.as_deref()),
            )
            .map(drop),
        Action::Revoke { of, grantee } => c.call("the share", client.revoke_share(of, grantee)),
        Action::Rotate { secret, value } => c
            .call("the rotation", client.rotate_secret(secret, value))
            .map(drop),
        Action::Unbind { secret, binding } => {
            c.call("the binding", client.unbind_secret(secret, binding))
        }
        Action::Answer {
            id,
            labels,
            digest,
            note,
        } => c
            .call(
                "the answer",
                client.answer_decision(id, labels, digest, note.as_deref()),
            )
            .map(drop),
        Action::CreateSecret { body } => c
            .call(
                "the new secret",
                client.create_secret::<serde_json::Value>(body),
            )
            .map(drop),
        Action::MoveSecret { secret, owner } => c
            .call("the owner change", client.transfer_secret(secret, owner))
            .map(drop),
        Action::SaveSchedule { id: None, body } => c
            .call(
                "the new schedule",
                client.create_schedule::<serde_json::Value>(body),
            )
            .map(drop),
        Action::SaveSchedule { id: Some(id), body } => c
            .call(
                "the schedule",
                client.update_schedule::<serde_json::Value>(id, body),
            )
            .map(drop),
        Action::RegisterImport {
            import,
            id,
            description,
        } => c
            .call(
                "the registration",
                client.register_import(import, &dto::RegisterImport { id, description }),
            )
            .map(drop),
        Action::DiscardImport { import } => c
            .call("the discard", client.discard_import(import))
            .map(drop),
        Action::CancelLaunch { key } => c.call("the cancel", client.cancel_launch(key)).map(drop),
    }
}

fn run(target: crate::profile::Target, rx: Receiver<Msg>, shared: Arc<Mutex<Published>>) {
    let mut worker = Worker {
        target,
        cache: Cache::open(),
        connection: None,
        view: AdminView::default(),
        acted: 0,
    };
    let mut watch = Watch::default();
    let mut next = Instant::now();
    loop {
        let wait = next
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(500));
        match rx.recv_timeout(wait) {
            Ok(Msg::Watch(now)) => {
                if now != watch {
                    watch = now;
                    next = Instant::now();
                }
            }
            Ok(Msg::Act(action)) => {
                worker.act(action);
                next = Instant::now();
            }
            Ok(Msg::Reconnect) => {
                worker.connection = None;
                worker.view = AdminView {
                    acted: worker.view.acted.take(),
                    ..AdminView::default()
                };
                next = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() < next {
            continue;
        }
        if !watch.controller {
            next = Instant::now() + QUEUE;
            continue;
        }
        worker.refresh(&watch);
        publish(&shared, worker.view.clone());
        let shown = watch.schedules
            || watch.secrets
            || watch.secret.is_some()
            || watch.shares.is_some()
            || watch.decision.is_some();
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
    /// The share form.
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
            grantee: TextEdit::new(""),
            role: 0,
            until: TextEdit::new(""),
            sent: None,
            outcome: None,
            armed: None,
            new_secret: None,
            move_to: None,
            register: None,
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
            schedules: on(crate::Page::Schedules),
            secrets: on(crate::Page::Secrets),
            secret: self
                .desk
                .secret
                .clone()
                .filter(|_| on(crate::Page::Secrets)),
            shares: self.shared_here().filter(|_| on(crate::Page::Runs)),
            decision: self
                .desk
                .decision
                .clone()
                .filter(|_| on(crate::Page::Decisions)),
        };
        if self.admin_sent.as_ref() != Some(&watch) {
            self.admin_client.watch(watch.clone());
            self.admin_sent = Some(watch);
        }
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
        Some(if self.registered.contains(playbook) {
            Shared::Playbook(playbook.clone())
        } else {
            Shared::Draft(playbook.clone())
        })
    }

    pub fn act(&mut self, action: Action) {
        self.desk.armed = None;
        self.desk.sent = Some(self.admin.acted.as_ref().map_or(0, |acted| acted.seq));
        self.desk.outcome = None;
        self.admin_client.act(action);
    }

    pub fn apply_admin(&mut self, view: AdminView) {
        if let (Some(sent), Some(acted)) = (self.desk.sent, &view.acted)
            && acted.seq > sent
        {
            self.desk.sent = None;
            if acted.result.is_ok() {
                match acted.did {
                    Did::Rotate => self.desk.rotate = None,
                    Did::Answer => {
                        self.desk.picks.clear();
                        self.desk.note = TextEdit::new("");
                    }
                    Did::Grant => {
                        self.desk.grantee = TextEdit::new("");
                        self.desk.until = TextEdit::new("");
                    }
                    Did::CreateSecret => self.desk.new_secret = None,
                    Did::MoveSecret => self.desk.move_to = None,
                    Did::SaveSchedule => {
                        self.launcher = None;
                        self.page = crate::Page::Schedules;
                    }
                    Did::Enable
                    | Did::Revoke
                    | Did::Unbind
                    | Did::RegisterImport
                    | Did::DiscardImport
                    | Did::CancelLaunch => {}
                }
            }
            if let (Did::SaveSchedule, Err(why), Some(launcher)) =
                (acted.did, &acted.result, self.launcher.as_mut())
            {
                launcher.stage = crate::Stage::Ready;
                launcher.refusal = Some(why.clone());
            }
            self.desk.outcome = Some(acted.result.clone());
        }
        if let Some(Ok(open)) = &view.decisions {
            let looking = crate::notify::app_focused() && self.page == crate::Page::Decisions;
            for decision in self.desk.arrived(open) {
                if self.settings.ask
                    && !looking
                    && let Some(notifier) = &self.notifier
                {
                    notifier.post(&note(decision));
                }
            }
        }
        self.admin = view;
    }

    /// How many decision requests wait on the signed-in user.
    pub fn waiting(&self) -> usize {
        match &self.admin.decisions {
            Some(Ok(open)) => open.len(),
            _ => 0,
        }
    }

    /// Scope packs to approve and imports to settle.
    pub fn approvals_waiting(&self) -> usize {
        match &self.admin.approvals {
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
        app.admin.decisions = Some(Ok(vec![dto::DecisionSummary {
            id: d.id.clone(),
            run_id: d.run_id.clone(),
            task: d.task.clone(),
            launch_key: d.launch_key.clone(),
            questions: d.questions.iter().map(|q| q.id.clone()).collect(),
            opened_at: d.opened_at.clone(),
            expires_at: d.expires_at.clone(),
        }]));
        app.desk.pick_decision(&d.id);
        app.admin.decision = Some((d.id.clone(), Some(Ok(d.clone()))));
    } else {
        app.admin.decisions = Some(decision.map(|_| Vec::new()));
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

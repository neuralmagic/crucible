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
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Published::default()));
        let worker = Arc::clone(&shared);
        if let Err(error) = std::thread::Builder::new()
            .name("crucible-playbooks-admin".into())
            .spawn(move || run(rx, worker))
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
    cache: Option<Cache>,
    connection: Option<Connection>,
    view: AdminView,
    acted: u64,
}

impl Worker {
    fn connection(&mut self) -> Result<&Connection, String> {
        if self.connection.is_none() {
            self.connection = Some(Connection::open(self.cache.as_ref())?);
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
    }
}

fn run(rx: Receiver<Msg>, shared: Arc<Mutex<Published>>) {
    let mut worker = Worker {
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
                    Did::Enable | Did::Revoke | Did::Unbind => {}
                }
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
}

#[cfg(test)]
mod tests {
    use crate::admin::*;

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

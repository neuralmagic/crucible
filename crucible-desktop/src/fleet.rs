//! The profiles not in use, read in the background for the menu bar and the Dock badge: what each
//! is running, and how many decisions and approvals wait on it. Their tunnels are not started; a
//! profile that does not answer is reported so.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::net::Handle;
use futures_util::future::join_all;
use rune::Waker;
use rune::time::Instant;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::net::{Connector, Task, within};
use crate::profile::Target;
use crate::tray::Running;

/// How often the other profiles are read.
const EVERY: Duration = Duration::from_secs(60);

/// One profile not in use, as last read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub name: String,
    pub running: Vec<Running>,
    pub decisions: usize,
    pub approvals: usize,
    pub error: Option<String>,
}

impl Status {
    pub fn waiting(&self) -> usize {
        self.decisions + self.approvals
    }
}

type Shared = Arc<Mutex<(u64, Vec<Status>, Option<Waker>)>>;

/// The UI-side handle to the worker. Dropping it ends its task.
pub struct Client {
    tx: UnboundedSender<Vec<Connector>>,
    shared: Shared,
    _task: Option<Task>,
}

impl Client {
    /// With no runtime nothing is read, and the profiles in use say why themselves.
    pub fn spawn(runtime: Result<Handle, String>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared: Shared = Arc::new(Mutex::new((0, Vec::new(), None)));
        let task = runtime
            .ok()
            .map(|runtime| Task::spawn(&runtime, run(rx, Arc::clone(&shared))));
        Client {
            tx,
            shared,
            _task: task,
        }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.2 = Some(waker);
        }
    }

    /// Read these profiles from now on.
    pub fn watch(&self, targets: Vec<Connector>) {
        let _ = self.tx.send(targets);
    }

    pub fn take(&self, seen: u64) -> Option<(u64, Vec<Status>)> {
        let shared = self.shared.lock().ok()?;
        (shared.0 > seen).then(|| (shared.0, shared.1.clone()))
    }
}

async fn run(mut rx: UnboundedReceiver<Vec<Connector>>, shared: Shared) {
    let mut targets: Vec<Connector> = Vec::new();
    let mut next = Instant::now() + EVERY;
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        match crate::net::timeout(wait, rx.recv()).await {
            Ok(Some(now)) => {
                targets = now;
                while let Ok(newer) = rx.try_recv() {
                    targets = newer;
                }
            }
            Ok(None) => return,
            Err(_) => {}
        }
        let statuses: Vec<Status> = join_all(targets.iter().map(status)).await;
        next = Instant::now() + EVERY;
        let waker = match shared.lock() {
            Ok(mut shared) => {
                shared.0 += 1;
                shared.1 = statuses;
                shared.2.clone()
            }
            Err(_) => return,
        };
        if let Some(waker) = waker {
            waker();
        }
    }
}

/// One profile's launches, decisions and approvals, read at once.
async fn status(connector: &Connector) -> Status {
    let mut status = Status {
        name: connector.target().name.clone(),
        ..Status::default()
    };
    let c = match connector.connect().await {
        Ok(c) => c,
        Err(why) => {
            status.error = Some(why);
            return status;
        }
    };
    let (launches, decisions, approvals) = tokio::join!(
        within(
            "the launch list",
            c.client
                .playbook_runs::<Vec<crux::dto::PlaybookRun>>(Some("running"), None),
        ),
        within("the decision queue", c.client.decisions()),
        within(
            "the approval queue",
            c.client.approvals::<crux::dto::Approvals>(),
        ),
    );
    match launches {
        Ok(launches) => {
            let now = jiff::Timestamp::now().as_second();
            status.running = launches
                .iter()
                .filter(|l| l.status == "running")
                .map(|l| running(l, now))
                .collect();
            status.decisions = decisions.map(|q| q.open.len()).unwrap_or(0);
            status.approvals = approvals.map(|q| q.waiting()).unwrap_or(0);
        }
        Err(why) => status.error = Some(why),
    }
    status
}

/// A launch from the list, as the menu shows it at `now` (Unix seconds).
pub fn running(launch: &crux::dto::PlaybookRun, now: i64) -> Running {
    let landed = launch
        .created_at
        .parse::<jiff::Timestamp>()
        .map_or(now, |at| at.as_second());
    Running {
        key: launch.key.clone(),
        playbook: launch.playbook.clone(),
        cost: launch.cost_usd.unwrap_or_default(),
        cap: launch.max_cost,
        elapsed: (now - landed).max(0),
        max: crate::form::minutes(&launch.max_time).map(|m| m as i64 * 60),
    }
}

impl crate::App {
    /// Point the worker at the usable profiles other than the one in use, when they changed.
    pub fn sync_fleet(&mut self) {
        if rune::is_headless() {
            return;
        }
        let others: Vec<(String, crate::profile::Profile)> = crate::profile::usable(&self.settings)
            .into_iter()
            .filter(|(name, _)| *name != self.remote.target.name)
            .collect();
        if others == self.fleet_sent {
            return;
        }
        let targets = others
            .iter()
            .map(|(name, profile)| match self.targets.get(name) {
                Some((target, _)) if target.profile == *profile => target.clone(),
                _ => Target::new(name, profile.clone(), &self.settings.keys()),
            })
            .map(|target| self.net.uncached(target))
            .collect();
        self.fleet
            .retain(|s| others.iter().any(|(name, _)| *name == s.name));
        self.fleet_client.watch(targets);
        self.fleet_sent = others;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::fleet::{Client, running};

    #[test]
    fn the_other_profiles_are_read_at_once() {
        let slow = Duration::from_secs(1);
        let (url, _) = crate::testserver::serve(move |request| {
            std::thread::sleep(slow);
            match request.split_whitespace().nth(1) {
                Some("/api/decisions") => r#"{"open":[]}"#.into(),
                Some("/api/approvals") => {
                    r#"{"pending_imports":[{"id":"i1","repo":"o/r","rev":"abc","created_at":"2026-11-09T01:00:00Z"}]}"#.into()
                }
                _ => r#"[{"key":"playbook:triage:1","playbook":"triage","status":"running",
                    "runs":1,"max_cost":2.0,"max_time":"30m","created_at":"2026-11-09T01:00:00Z"}]"#
                    .into(),
            }
        });
        let keys = tempfile::tempdir().unwrap();
        let net = crate::net::Net::uncached_for_tests();
        let client = Client::spawn(net.handle());
        let asked = Instant::now();
        client.watch(
            ["a", "b", "c"]
                .into_iter()
                .map(|name| net.uncached(crate::testserver::target(name, &url, keys.path())))
                .collect(),
        );
        let statuses = loop {
            if let Some((_, statuses)) = client.take(0) {
                break statuses;
            }
            assert!(
                asked.elapsed() < Duration::from_secs(15),
                "nothing was read"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(
            asked.elapsed() < slow * 3,
            "nine reads of {slow:?} each took {:?}",
            asked.elapsed()
        );
        assert_eq!(
            statuses.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        for status in &statuses {
            assert_eq!(status.error, None);
            assert_eq!(status.running.len(), 1);
            assert_eq!(status.running[0].playbook, "triage");
            assert_eq!((status.decisions, status.approvals), (0, 1));
        }
    }

    #[test]
    fn a_listed_launch_reads_as_running_with_its_caps() {
        let launch: crux::dto::PlaybookRun = serde_json::from_str(
            r#"{"key":"playbook:triage:1","playbook":"triage","status":"running","runs":1,
                "cost_usd":0.5,"max_cost":2.0,"max_time":"30m","created_at":"2026-11-09T01:00:00Z"}"#,
        )
        .unwrap();
        let now = "2026-11-09T01:12:00Z"
            .parse::<jiff::Timestamp>()
            .unwrap()
            .as_second();
        let r = running(&launch, now);
        assert_eq!((r.elapsed, r.max), (720, Some(1800)));
        assert_eq!(r.budget(), Some(0.4));
        assert_eq!(running(&launch, 0).elapsed, 0, "never negative");
    }
}

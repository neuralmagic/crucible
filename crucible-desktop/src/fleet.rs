//! The profiles not in use, read in the background for the menu bar and the Dock badge: what each
//! is running, and how many decisions and approvals wait on it. Their tunnels are not started; a
//! profile that does not answer is reported so.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rune::Waker;

use crate::live::Connection;
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

/// The UI-side handle to the worker. Dropping it stops the thread.
pub struct Client {
    tx: Sender<Vec<Target>>,
    shared: Shared,
}

impl Client {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared: Shared = Arc::new(Mutex::new((0, Vec::new(), None)));
        let worker = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("crucible-playbooks-fleet".into())
            .spawn(move || run(rx, worker));
        Client { tx, shared }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.2 = Some(waker);
        }
    }

    /// Read these profiles from now on.
    pub fn watch(&self, targets: Vec<Target>) {
        let _ = self.tx.send(targets);
    }

    pub fn take(&self, seen: u64) -> Option<(u64, Vec<Status>)> {
        let shared = self.shared.lock().ok()?;
        (shared.0 > seen).then(|| (shared.0, shared.1.clone()))
    }
}

fn run(rx: Receiver<Vec<Target>>, shared: Shared) {
    let mut targets: Vec<Target> = Vec::new();
    let mut next = Instant::now() + EVERY;
    loop {
        let wait = next.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(now) => {
                targets = now;
                while let Ok(newer) = rx.try_recv() {
                    targets = newer;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        let statuses: Vec<Status> = targets.iter().map(status).collect();
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

fn status(target: &Target) -> Status {
    let mut status = Status {
        name: target.name.clone(),
        ..Status::default()
    };
    let read = Connection::open(target, None).and_then(|c| {
        let now = jiff::Timestamp::now().as_second();
        let launches: Vec<crux::dto::PlaybookRun> = c.call(
            "the launch list",
            c.client.playbook_runs(Some("running"), None),
        )?;
        let running = launches
            .iter()
            .filter(|l| l.status == "running")
            .map(|l| running(l, now))
            .collect();
        let decisions = c
            .call("the decision queue", c.client.decisions())
            .map(|q| q.open.len())
            .unwrap_or(0);
        let approvals = c
            .call(
                "the approval queue",
                c.client.approvals::<crux::dto::Approvals>(),
            )
            .map(|q| q.waiting())
            .unwrap_or(0);
        Ok((running, decisions, approvals))
    });
    match read {
        Ok((running, decisions, approvals)) => {
            status.running = running;
            status.decisions = decisions;
            status.approvals = approvals;
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
            .filter(|(name, _)| *name != self.target.name)
            .collect();
        if others == self.fleet_sent {
            return;
        }
        let targets = others
            .iter()
            .map(|(name, profile)| match self.targets.get(name) {
                Some((target, _)) if target.profile == *profile => target.clone(),
                _ => Target::new(name, profile.clone(), &self.settings.keys_dir()),
            })
            .collect();
        self.fleet
            .retain(|s| others.iter().any(|(name, _)| *name == s.name));
        self.fleet_client.watch(targets);
        self.fleet_sent = others;
    }
}

#[cfg(test)]
mod tests {
    use crate::fleet::running;

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

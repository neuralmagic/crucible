//! The launch loop (ADR-0056): runs a pass over the due-time triggers as soon as one is due,
//! instead of on the discovery cadence.
//!
//! ```text
//!        ┌──────────── probe: earliest due, held rows left out ◀──────────────┐
//!        │                                                                     │
//!   due <= now ──▶ pass ──▶ claimed any? ── yes ───────────────────────────────┤
//!        │                      │ no                                           │
//!   due > now                   ▼                                              │
//!        └────────▶ sleep min(until due, probe interval), or until woken ──────┘
//! ```

#![allow(clippy::disallowed_macros)]

use crate::daemon::queue::{DueSource, Enqueue};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

/// How often the loop re-reads the due times when nothing is due sooner.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// The launch loop's knobs.
#[derive(Clone)]
pub struct LaunchLoopCfg {
    pub probe: Duration,
    /// How long a row a pass deferred stays out of the probe. Re-read after every pass.
    pub hold: Arc<dyn Fn() -> Duration + Send + Sync>,
}

/// Run the launch loop until the task is cancelled. `wake` forces an early probe.
pub async fn run(
    source: Arc<dyn DueSource>,
    enqueue: Arc<dyn Enqueue>,
    cfg: LaunchLoopCfg,
    wake: Arc<Notify>,
) {
    let mut held: HashMap<String, Instant> = HashMap::new();
    loop {
        let now = Instant::now();
        held.retain(|_, until| *until > now);
        let ids: Vec<String> = held.keys().cloned().collect();
        let sleep = match source.next_due(ids.clone()).await {
            Ok(Some(due)) if due <= jiff::Timestamp::now() => {
                match source.pass(enqueue.clone(), ids).await {
                    Ok(pass) => {
                        let until = Instant::now() + (cfg.hold)();
                        held.extend(pass.held.into_iter().map(|id| (id, until)));
                        if pass.claimed > 0 {
                            continue;
                        }
                        cfg.probe
                    }
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "launch loop: pass failed");
                        cfg.probe
                    }
                }
            }
            Ok(Some(due)) => {
                let until_due = jiff::Timestamp::now().duration_until(due);
                Duration::try_from(until_due)
                    .unwrap_or(Duration::ZERO)
                    .min(cfg.probe)
            }
            Ok(None) => cfg.probe,
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "launch loop: probe failed");
                cfg.probe
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(sleep) => {}
            _ = wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::daemon::launch_loop::{LaunchLoopCfg, run};
    use crate::daemon::queue::{BoxFuture, DueSource, Enqueue, IssueKey, Pass};
    use jiff::{SignedDuration, Timestamp};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::Notify;

    /// One scripted source: `next_due` answers from `due` (the last answer repeats), and `pass`
    /// answers from `passes` (an empty pass once they run out). Both calls are recorded.
    #[derive(Default)]
    struct Script {
        due: Mutex<VecDeque<Option<SignedDuration>>>,
        passes: Mutex<VecDeque<Pass>>,
        probes: Mutex<Vec<Vec<String>>>,
        passed: Mutex<Vec<Vec<String>>>,
        passed_notify: Notify,
    }

    impl Script {
        fn new(due: Vec<Option<SignedDuration>>, passes: Vec<Pass>) -> Arc<Self> {
            Arc::new(Script {
                due: Mutex::new(due.into()),
                passes: Mutex::new(passes.into()),
                ..Script::default()
            })
        }

        async fn wait_for_passes(&self, n: usize) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let notified = self.passed_notify.notified();
                    if self.passed.lock().unwrap().len() >= n {
                        return;
                    }
                    notified.await;
                }
            })
            .await
            .expect("the passes ran");
        }

        fn passes(&self) -> Vec<Vec<String>> {
            self.passed.lock().unwrap().clone()
        }

        fn probes(&self) -> Vec<Vec<String>> {
            self.probes.lock().unwrap().clone()
        }
    }

    impl DueSource for Script {
        fn next_due(&self, held: Vec<String>) -> BoxFuture<anyhow::Result<Option<Timestamp>>> {
            self.probes.lock().unwrap().push(held);
            let mut due = self.due.lock().unwrap();
            let answer = if due.len() > 1 {
                due.pop_front().flatten()
            } else {
                due.front().copied().flatten()
            };
            Box::pin(async move { Ok(answer.map(|offset| Timestamp::now() + offset)) })
        }

        fn pass(&self, _: Arc<dyn Enqueue>, held: Vec<String>) -> BoxFuture<anyhow::Result<Pass>> {
            self.passed.lock().unwrap().push(held);
            self.passed_notify.notify_waiters();
            let pass = self.passes.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(async move { Ok(pass) })
        }
    }

    struct NoQueue;

    impl Enqueue for NoQueue {
        fn enqueue(&self, _: IssueKey) {}
    }

    fn spawn(
        script: &Arc<Script>,
        probe: Duration,
        hold: Duration,
    ) -> (tokio::task::JoinHandle<()>, Arc<Notify>) {
        let wake = Arc::new(Notify::new());
        let handle = tokio::spawn(run(
            script.clone(),
            Arc::new(NoQueue),
            LaunchLoopCfg {
                probe,
                hold: Arc::new(move || hold),
            },
            wake.clone(),
        ));
        (handle, wake)
    }

    const PAST: Option<SignedDuration> = Some(SignedDuration::from_secs(-1));
    const LONG: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn a_due_row_passes_at_once_and_a_claiming_pass_repeats_until_idle() {
        let claimed = |n| Pass {
            claimed: n,
            held: Vec::new(),
        };
        let script = Script::new(vec![PAST, PAST, PAST, None], vec![claimed(32), claimed(3)]);
        let (handle, _) = spawn(&script, LONG, LONG);
        script.wait_for_passes(3).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        assert_eq!(
            script.passes().len(),
            3,
            "two claiming passes, then one that claimed nothing, then the long sleep"
        );
    }

    #[tokio::test]
    async fn nothing_due_sleeps_the_probe_interval_between_probes() {
        let script = Script::new(vec![None], Vec::new());
        let (handle, _) = spawn(&script, Duration::from_millis(20), LONG);
        tokio::time::sleep(Duration::from_millis(150)).await;
        handle.abort();
        let probes = script.probes().len();
        assert!((3..=10).contains(&probes), "{probes} probes in 150ms");
        assert!(script.passes().is_empty());
    }

    #[tokio::test]
    async fn a_future_due_time_is_met_before_the_probe_interval() {
        let script = Script::new(
            vec![Some(SignedDuration::from_millis(200)), PAST, None],
            Vec::new(),
        );
        let started = std::time::Instant::now();
        let (handle, _) = spawn(&script, LONG, LONG);
        script.wait_for_passes(1).await;
        handle.abort();
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(150) && waited < Duration::from_secs(2),
            "passed after {waited:?}"
        );
    }

    #[tokio::test]
    async fn a_deferred_row_is_held_out_of_probes_and_passes_until_its_hold_ends() {
        let script = Script::new(
            vec![PAST],
            vec![Pass {
                claimed: 1,
                held: vec!["standing-a".into()],
            }],
        );
        let (handle, _) = spawn(
            &script,
            Duration::from_millis(10),
            Duration::from_millis(150),
        );
        script.wait_for_passes(2).await;
        assert_eq!(script.passes()[1], vec!["standing-a".to_string()]);
        tokio::time::sleep(Duration::from_millis(250)).await;
        handle.abort();
        assert!(
            script.probes().last().expect("probed").is_empty(),
            "the hold expired"
        );
    }

    #[tokio::test]
    async fn a_pass_that_claims_nothing_waits_the_probe_interval_instead_of_spinning() {
        let script = Script::new(vec![PAST], Vec::new());
        let (handle, _) = spawn(&script, Duration::from_millis(50), LONG);
        tokio::time::sleep(Duration::from_millis(220)).await;
        handle.abort();
        let passes = script.passes().len();
        assert!((3..=6).contains(&passes), "{passes} passes in 220ms");
    }

    #[tokio::test]
    async fn a_wake_probes_before_the_interval_runs_out() {
        let script = Script::new(vec![None, PAST, None], Vec::new());
        let (handle, wake) = spawn(&script, LONG, LONG);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(script.passes().is_empty());
        wake.notify_one();
        script.wait_for_passes(1).await;
        handle.abort();
    }
}

//! Admission of the lanes of one dispatch against the run's ceilings. A lane is admitted when it
//! starts, not when its batch is handed to the runner, so a wide fan-out stops at the cost ceiling
//! partway through rather than after every lane has run.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::panic::AssertUnwindSafe;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Instant;

use crate::deadline::{Deadline, RunCeiling};
use crate::plan::ir::{Task, TaskName};
use crate::plan::machine::BlockedReason;

/// Lanes of one dispatch in flight at once, unless the executor is configured otherwise.
pub const DEFAULT_WIDTH: NonZeroUsize = NonZeroUsize::MIN.saturating_add(7);

/// What a lane's attempt cost, and the result a lane that panicked settles as.
pub trait Settled {
    fn cost_usd(&self) -> f64;
    fn panicked() -> Self;
}

/// One lane of a batch: the task it runs and which attempt of that task it is.
pub trait BatchLane {
    fn task(&self) -> &Task;
    fn attempt(&self) -> u32;
}

/// Where an attempt is recorded before it runs, so a resumed run can charge one whose process
/// died before it settled (RFC-0002:C-PLAYBOOK-RESUME).
pub trait Journal: Sync {
    fn started(&self, task: &TaskName, attempt: u32, estimate_usd: f64);
}

/// What a resumed run charges for an attempt of `task` that started and never settled: nothing
/// for a task that cannot spend, otherwise one lane's equal share of the cost ceiling. A dispatch
/// interrupted at full width is charged at most the whole ceiling, and a task that kills its
/// process on every attempt exhausts the ceiling within `width` resumes.
pub fn estimate_usd(task: &Task, budget: f64, width: NonZeroUsize) -> f64 {
    if !task.task.spends() {
        return 0.0;
    }
    let lanes = u32::try_from(width.get()).map_or(f64::from(u32::MAX), f64::from);
    budget / lanes
}

/// Which ceiling refused a lane. The lane never started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    CostCeiling,
    WallClockCeiling,
}

impl From<Refusal> for BlockedReason {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::CostCeiling => BlockedReason::BudgetCeiling,
            Refusal::WallClockCeiling => BlockedReason::WallClockCeiling,
        }
    }
}

/// The run's ceilings as the lanes of one dispatch draw on them, possibly from several threads.
///
/// A lane that can spend is admitted while the run's settled spend plus what lanes in flight have
/// reported stays under the cost ceiling. The first refusal on cost refuses every later lane that
/// can spend, so dispatch stops at one point instead of resuming when an in-flight figure
/// settles lower. Any lane is refused once the run's wall-clock ceiling has fallen.
pub struct Admission<'j> {
    budget: f64,
    run_ceiling: Option<RunCeiling>,
    width: NonZeroUsize,
    tally: Mutex<Tally>,
    journal: Option<&'j dyn Journal>,
}

#[derive(Default)]
struct Tally {
    settled: f64,
    live: BTreeMap<u64, f64>,
    next: u64,
    closed: bool,
}

impl Tally {
    fn reached(&self, budget: f64) -> bool {
        self.settled + self.live.values().sum::<f64>() >= budget
    }
}

/// One admitted lane: the deadline it runs under, counted from when it was admitted, and the
/// handle it reports its spend through while it runs.
pub struct Lane<'a> {
    admission: &'a Admission<'a>,
    id: u64,
    deadline: Option<Deadline>,
}

impl Lane<'_> {
    pub fn deadline(&self) -> Option<Deadline> {
        self.deadline
    }

    /// What this lane's attempt has spent so far. A figure lower than one already reported is
    /// ignored: a lane's spend never goes down.
    pub fn spent_so_far(&self, usd: f64) {
        if !usd.is_finite() {
            return;
        }
        let mut tally = self.admission.tally();
        if let Some(live) = tally.live.get_mut(&self.id) {
            *live = live.max(usd);
        }
    }
}

impl Drop for Lane<'_> {
    fn drop(&mut self) {
        self.admission.tally().live.remove(&self.id);
    }
}

impl<'j> Admission<'j> {
    /// `spent` is what the run had settled before this dispatch.
    pub fn new(
        budget: f64,
        spent: f64,
        run_ceiling: Option<RunCeiling>,
        width: NonZeroUsize,
    ) -> Self {
        Admission {
            budget,
            run_ceiling,
            width,
            tally: Mutex::new(Tally {
                settled: spent,
                ..Tally::default()
            }),
            journal: None,
        }
    }

    /// Record every admitted lane in `journal` before it runs.
    pub fn journaled(self, journal: Option<&'j dyn Journal>) -> Self {
        Admission { journal, ..self }
    }

    fn tally(&self) -> MutexGuard<'_, Tally> {
        self.tally.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn admit(&self, lane: &impl BatchLane) -> Result<Lane<'_>, Refusal> {
        let task = lane.task();
        if self.run_ceiling.is_some_and(|ceiling| ceiling.reached()) {
            return Err(Refusal::WallClockCeiling);
        }
        let mut tally = self.tally();
        let spends = task.task.spends();
        if spends && (tally.closed || tally.reached(self.budget)) {
            tally.closed = true;
            return Err(Refusal::CostCeiling);
        }
        let id = tally.next;
        tally.next += 1;
        tally.live.insert(id, 0.0);
        drop(tally);
        if let Some(journal) = self.journal {
            journal.started(
                &task.name,
                lane.attempt(),
                estimate_usd(task, self.budget, self.width),
            );
        }
        Ok(Lane {
            admission: self,
            id,
            deadline: Deadline::for_attempt(Instant::now(), task.timeout, self.run_ceiling),
        })
    }

    /// Count what an admitted lane cost and retire its running figure.
    fn settle(&self, lane: Lane<'_>, attempt: &impl Settled) {
        let mut tally = self.tally();
        tally.live.remove(&lane.id);
        tally.settled += attempt.cost_usd();
    }

    /// Run `item` if the ceilings admit it, and count what it cost.
    pub fn run<A: Settled>(
        &self,
        item: &impl BatchLane,
        lane: impl FnOnce(&Lane<'_>) -> A,
    ) -> Result<A, Refusal> {
        let admitted = self.admit(item)?;
        let attempt = lane(&admitted);
        self.settle(admitted, &attempt);
        Ok(attempt)
    }

    /// Run every lane of `batch` on at most `width` threads. Lanes are admitted in
    /// batch order as each starts, so once one is refused on cost every later lane that can spend
    /// is too. Results are in batch order. A lane that panics settles as a measured failure.
    pub fn run_all<I, A, F>(&self, batch: &[I], lane: F) -> Vec<Result<A, Refusal>>
    where
        I: BatchLane + Sync,
        A: Settled + Send + Sync,
        F: Fn(&I, &Lane<'_>) -> A + Sync,
    {
        let guarded = |item: &I, admitted: &Lane<'_>| {
            std::panic::catch_unwind(AssertUnwindSafe(|| lane(item, admitted)))
                .unwrap_or_else(|_| A::panicked())
        };
        if let [only] = batch {
            return vec![self.run(only, |admitted| guarded(only, admitted))];
        }
        let next = Mutex::new(0usize);
        let slots: Vec<OnceLock<Result<A, Refusal>>> =
            batch.iter().map(|_| OnceLock::new()).collect();
        let take = || {
            let mut next = next.lock().unwrap_or_else(PoisonError::into_inner);
            let i = *next;
            let item = batch.get(i)?;
            *next += 1;
            Some((item, slots.get(i)?, self.admit(item)))
        };
        std::thread::scope(|scope| {
            for _ in 0..self.width.get().min(batch.len()) {
                scope.spawn(|| {
                    while let Some((item, slot, admitted)) = take() {
                        let settled = admitted.map(|admitted| {
                            let attempt = guarded(item, &admitted);
                            self.settle(admitted, &attempt);
                            attempt
                        });
                        let _ = slot.set(settled);
                    }
                });
            }
        });
        slots
            .into_iter()
            .map(|slot| slot.into_inner().unwrap_or_else(|| Ok(A::panicked())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use crate::deadline::{Bound, RunCeiling};
    use crate::plan::admission::{Admission, Refusal};
    use crate::plan::exec::{Attempt, AttemptOutcome, BatchItem};
    use crate::plan::ir::{Join, Stage, Task, TaskKind};

    fn task(name: &str, kind: TaskKind) -> Task {
        Task {
            name: name.into(),
            task: kind,
            depends_on: Vec::new(),
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join: Join::default(),
            stage: Stage::Iteration,
            emits: crate::plan::ir::Emits::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
        }
    }

    fn agent(name: &str) -> Task {
        task(
            name,
            TaskKind::Agent {
                prompt: "audit".into(),
                harness: None,
                model: None,
                effort: None,
                sandbox: None,
            },
        )
    }

    fn command(name: &str) -> Task {
        task(
            name,
            TaskKind::Command {
                command: "true".into(),
            },
        )
    }

    fn costing(usd: f64) -> Attempt {
        Attempt {
            outcome: AttemptOutcome::Pass(serde_json::json!({})),
            cost_usd: usd,
            repairs: Vec::new(),
        }
    }

    fn one(task: &Task) -> BatchItem<'_> {
        BatchItem {
            task,
            attempt: 1,
            inputs: BTreeMap::new(),
        }
    }

    fn width(n: usize) -> NonZeroUsize {
        NonZeroUsize::new(n).unwrap()
    }

    #[test]
    fn a_lane_that_can_spend_is_refused_once_settled_spend_reaches_the_ceiling() {
        let admission = Admission::new(1.0, 0.0, None, width(1));
        let lane = agent("audit[a]");
        assert!(admission.run(&one(&lane), |_| costing(0.6)).is_ok());
        assert!(admission.run(&one(&lane), |_| costing(0.4)).is_ok());
        assert_eq!(
            admission.run(&one(&lane), |_| costing(0.1)).err(),
            Some(Refusal::CostCeiling)
        );
    }

    #[test]
    fn spend_a_lane_reports_while_running_counts_against_the_next_admission() {
        let admission = Admission::new(1.0, 0.25, None, width(2));
        let first = agent("audit[a]");
        let second = agent("audit[b]");
        let mut refused = None;
        let ran = admission.run(&one(&first), |lane| {
            lane.spent_so_far(0.5);
            lane.spent_so_far(0.75);
            lane.spent_so_far(0.1);
            refused = admission.run(&one(&second), |_| costing(0.0)).err();
            costing(0.2)
        });
        assert!(ran.is_ok());
        assert_eq!(refused, Some(Refusal::CostCeiling));
    }

    #[test]
    fn a_refusal_on_cost_holds_after_an_in_flight_figure_settles_lower() {
        let admission = Admission::new(1.0, 0.0, None, width(2));
        let lane = agent("audit[a]");
        let _ = admission.run(&one(&lane), |admitted| {
            admitted.spent_so_far(2.0);
            assert!(admission.run(&one(&lane), |_| costing(0.0)).is_err());
            costing(0.1)
        });
        assert_eq!(
            admission.run(&one(&lane), |_| costing(0.0)).err(),
            Some(Refusal::CostCeiling),
            "dispatch stopped once and stays stopped"
        );
    }

    #[test]
    fn a_lane_that_cannot_spend_is_admitted_past_the_cost_ceiling() {
        let admission = Admission::new(1.0, 5.0, None, width(1));
        assert_eq!(
            admission
                .run(&one(&agent("audit[a]")), |_| costing(0.0))
                .err(),
            Some(Refusal::CostCeiling)
        );
        assert!(
            admission
                .run(&one(&command("apply")), |_| costing(0.0))
                .is_ok()
        );
    }

    #[test]
    fn every_lane_is_refused_once_the_wall_clock_ceiling_has_fallen() {
        let ceiling = RunCeiling::after(Instant::now(), Duration::ZERO, Duration::ZERO);
        let admission = Admission::new(10.0, 0.0, ceiling, width(1));
        for lane in [agent("audit[a]"), command("apply")] {
            assert_eq!(
                admission.run(&one(&lane), |_| costing(0.0)).err(),
                Some(Refusal::WallClockCeiling)
            );
        }
    }

    #[test]
    fn a_lanes_deadline_is_counted_from_its_own_admission() {
        let mut limited = agent("audit[a]");
        limited.timeout = Some("1h".parse().unwrap());
        let admission = Admission::new(10.0, 0.0, None, width(1));
        let before = Instant::now();
        let deadline = admission
            .run(&one(&limited), |lane| {
                let deadline = lane.deadline().unwrap();
                assert_eq!(deadline.bound, Bound::Task("1h".parse().unwrap()));
                assert!(deadline.at >= before + Duration::from_secs(3600));
                costing(0.0)
            })
            .map(|_| ());
        assert!(deadline.is_ok());
    }

    #[test]
    fn run_all_keeps_at_most_width_lanes_in_flight_and_answers_in_batch_order() {
        let tasks: Vec<Task> = (0..12).map(|i| command(&format!("lint[{i}]"))).collect();
        let batch: Vec<BatchItem<'_>> = tasks
            .iter()
            .map(|task| BatchItem {
                task,
                attempt: 1,
                inputs: BTreeMap::new(),
            })
            .collect();
        let admission = Admission::new(10.0, 0.0, None, width(3));
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let results = admission.run_all(&batch, |item, _| {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Attempt {
                outcome: AttemptOutcome::Pass(serde_json::json!({"lane": item.task.name.0})),
                cost_usd: 0.0,
                repairs: Vec::new(),
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= 3, "{peak:?}");
        let lanes: Vec<String> = results
            .into_iter()
            .map(|r| match r.map(|a| a.outcome) {
                Ok(AttemptOutcome::Pass(v)) => v["lane"].as_str().unwrap().to_owned(),
                _ => panic!("every lane passes"),
            })
            .collect();
        let names: Vec<String> = tasks.iter().map(|t| t.name.0.clone()).collect();
        assert_eq!(lanes, names);
    }

    #[test]
    fn run_all_stops_admitting_lanes_that_can_spend_at_the_ceiling() {
        let tasks: Vec<Task> = (0..8).map(|i| agent(&format!("audit[{i}]"))).collect();
        let batch: Vec<BatchItem<'_>> = tasks
            .iter()
            .map(|task| BatchItem {
                task,
                attempt: 1,
                inputs: BTreeMap::new(),
            })
            .collect();
        let admission = Admission::new(1.0, 0.0, None, width(2));
        let results = admission.run_all(&batch, |_, _| costing(0.3));
        let ran = results.iter().filter(|r| r.is_ok()).count();
        assert!((4..=5).contains(&ran), "{ran} lanes ran");
        assert!(results[..4].iter().all(Result::is_ok));
        assert!(
            results
                .iter()
                .skip_while(|r| r.is_ok())
                .all(|r| r.as_ref().err() == Some(&Refusal::CostCeiling)),
            "once a lane is refused every later one is"
        );
    }

    /// A lane is recorded as it is admitted, with its attempt and what a resume would charge
    /// for it, so a lane the ceiling refused leaves nothing to charge.
    #[test]
    fn an_admitted_lane_is_journaled_before_it_runs_and_a_refused_one_is_not() {
        #[derive(Default)]
        struct Recorded(std::sync::Mutex<Vec<(String, u32, f64)>>);
        impl crate::plan::admission::Journal for Recorded {
            fn started(&self, task: &crate::plan::ir::TaskName, attempt: u32, estimate_usd: f64) {
                self.0
                    .lock()
                    .unwrap()
                    .push((task.0.clone(), attempt, estimate_usd));
            }
        }
        let journal = Recorded::default();
        let admission = Admission::new(1.0, 0.0, None, width(4)).journaled(Some(&journal));
        let audit = agent("audit[a]");
        let apply = command("apply");
        let retried = BatchItem {
            task: &audit,
            attempt: 2,
            ..one(&audit)
        };
        let ran = admission.run(&retried, |_| {
            assert_eq!(
                journal.0.lock().unwrap().len(),
                1,
                "journaled before it ran"
            );
            costing(1.0)
        });
        assert!(ran.is_ok());
        assert_eq!(
            admission.run(&one(&audit), |_| costing(0.0)).err(),
            Some(Refusal::CostCeiling)
        );
        assert!(admission.run(&one(&apply), |_| costing(0.0)).is_ok());
        assert_eq!(
            *journal.0.lock().unwrap(),
            [
                ("audit[a]".to_owned(), 2, 0.25),
                ("apply".to_owned(), 1, 0.0)
            ]
        );
    }

    #[test]
    fn a_lane_whose_thread_panics_settles_failed_and_the_rest_still_run() {
        let tasks: Vec<Task> = (0..4).map(|i| command(&format!("lint[{i}]"))).collect();
        let batch: Vec<BatchItem<'_>> = tasks
            .iter()
            .map(|task| BatchItem {
                task,
                attempt: 1,
                inputs: BTreeMap::new(),
            })
            .collect();
        let admission = Admission::new(10.0, 0.0, None, width(2));
        let results = admission.run_all(&batch, |item, _| {
            assert_ne!(item.task.name.0, "lint[1]", "lane panicked");
            costing(0.0)
        });
        let failed: Vec<bool> = results
            .iter()
            .map(|r| matches!(r, Ok(a) if matches!(a.outcome, AttemptOutcome::Fail { .. })))
            .collect();
        assert_eq!(failed, [false, true, false, false]);
    }
}

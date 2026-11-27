//! The interface the executor dispatches each task attempt through.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::plan::admission::{Admission, Lane, Refusal};
use crate::plan::attempt::{Attempt, BatchItem};
use crate::plan::ir::{Task, TaskName};

/// Runs one attempt of an `Agent` or `Command` task. The engine implements this against the
/// harness and broker; tests program it directly. `TopK` never reaches the runner: reducers
/// are engine-owned.
///
/// A runner that executes work ends it at `deadline` and returns a measured failure carrying
/// [`Deadline::note`](crate::deadline::Deadline::note), never a transport failure: rerunning an attempt that exceeded its limit
/// would repeat the cost.
pub trait TaskRunner {
    /// Called once before the plan's clock starts, so no attempt's deadline pays for setup.
    fn prepare(&mut self) {}

    /// Run one attempt of `task` in `lane`, which carries its deadline and takes its spend and
    /// transcript; `None` runs it outside any admission.
    fn run(
        &mut self,
        task: &Task,
        attempt: u32,
        inputs: &BTreeMap<TaskName, Value>,
        lane: Option<&Lane<'_>>,
    ) -> Attempt;

    /// Stage every declared file produced by the tasks named, into the workspace this task is
    /// about to run in. An error refuses the dispatch rather than running a task whose inputs
    /// are not there.
    fn stage(&mut self, _task: &Task, _producers: &[&Task]) -> Result<(), String> {
        Ok(())
    }

    /// Called once per task after it settles, with whether it passed. A runner that owns
    /// durable state records what a passing task did and drops what a failing one did; the
    /// default keeps nothing, which is what a stateless runner wants.
    fn settled(&mut self, _task: &Task, _passed: bool) {}

    /// What the run may still spend, called before each dispatch. A runner that takes repair
    /// turns starts none once an attempt's own spend reaches it; the default takes none.
    fn budget_left(&mut self, _usd: f64) {}

    /// Run several isolation-marked tasks, possibly concurrently, each as a lane `admission`
    /// admits as it starts; a refused lane never runs. The executor only batches tasks that are
    /// simultaneously ready, so items never depend on each other. Results are in batch order.
    /// The default runs them serially through [`TaskRunner::run`]; a runner that can
    /// parallelize (per-task worktrees) overrides this.
    fn run_many(
        &mut self,
        batch: &[BatchItem<'_>],
        admission: &Admission,
    ) -> Vec<Result<Attempt, Refusal>> {
        batch
            .iter()
            .map(|b| admission.run(b, |lane| self.run(b.task, b.attempt, &b.inputs, Some(lane))))
            .collect()
    }
}

//! Crucible, the domain-agnostic core of the autoresearch harness.
//!
//! The loop is one shape: **propose → apply → measure → accept/reject → remember**,
//! bounded by a budget, steerable mid-flight, abortable from either side. An LLM agent
//! is the proposal policy; a frozen objective is the judge. Everything domain-specific
//! lives behind two traits, so a new problem is "implement [`World`] + [`Judge`]", the
//! engine (`run_loop`), the reporters, the session log, and the control plane
//! don't change.
//!
//! The two traits are deliberately split along the trust boundary its freeze demands
//! ("automate the setup, never the judge"):
//!
//! - [`World`] is the **agent-mutable** state being optimized: apply a candidate,
//!   snapshot it, roll back on discard. The engine treats the snapshot as opaque.
//! - [`Judge`] is the **frozen objective**: measure a candidate, rule keep/discard. The
//!   engine calls it; the agent never can.
//!
//! The built-in implementations are the any-repo batteries ([`crate::command_world::GitWorld`]
//! / [`crate::command_world::CommandWorld`] and [`crate::command_judge::CommandJudge`]), built
//! from a `crucible.toml` manifest; domains are manifests, not engine code.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// One measurement of the current candidate. Domain-neutral by design: `score` is the
/// fitness, `solved` is the win flag the measure step emits, and `detail` is free-form JSON
/// for anything extra (a bench might stash `cache_hit_rate`; the test gate stashes
/// `total`/`green`). The engine reads only `valid`/`score`/`solved`; everything else is for
/// the domain's `decide`/`detail` and the results row.
#[derive(Debug, Default, Clone)]
pub struct Reading {
    pub valid: bool,
    pub score: Option<f64>,
    /// Optional secondary scalar for functional gates whose primary score is effectively
    /// boolean (a pass/fail rung reporting 0.0/1.0): when two candidates tie on `score`,
    /// a strictly better `tiebreak` still keeps. Absent = ties discard, as ever.
    pub tiebreak: Option<f64>,
    pub solved: bool,
    pub note: String,
    pub detail: serde_json::Value,
}

/// The verdict on one candidate: keep it, and (domain-defined) whether it solved the goal.
pub struct Decision {
    pub keep: bool,
    pub solved: bool,
}

/// The run-so-far context the engine hands a [`Judge`] at measure time. A command judge turns
/// these into `CRUCIBLE_BASELINE_SCORE`/`CRUCIBLE_BASELINE_TOTAL`/`CRUCIBLE_BEST_SCORE` env
/// for its `measure` command (so a win condition like "score beat baseline" lives in the
/// command, not the engine). All `None` on the baseline measurement, before anything is known.
#[derive(Default, Clone, Copy)]
pub struct MeasureCtx {
    pub baseline_score: Option<f64>,
    pub baseline_total: Option<u64>,
    pub best_score: Option<f64>,
}

/// One publishable component of a multi-workspace (composite) world: a real git repo carrying the
/// agent's kept commits, with the pristine base reachable as an ancestor. The publish layer pushes
/// `head_sha` and `base_sha` (both local objects) to the component's fork as a branch-to-branch draft
/// PR, the same mechanism as the single-repo path, just per component. The fork repo is joined in by
/// name from the manifest, so the World stays repo-mapping-agnostic.
pub struct PublishComponent {
    /// The component name, joins to the manifest's per-component `pr_repo`.
    pub name: String,
    /// The component's checkout, where the kept commits live and `git push` runs.
    pub workspace: std::path::PathBuf,
    /// The pristine base sha (recorded in the baseline snapshot token).
    pub base_sha: String,
    /// The kept-candidate tip sha (recorded in the best snapshot token).
    pub head_sha: String,
}

/// Which way is better. `lower` (latency, failures) or `higher` (throughput, score).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Lower,
    Higher,
}

impl Direction {
    /// Strictly-better test for the keep rule (and the gate self-test's discrimination check).
    pub fn better(self, score: f64, best: f64) -> bool {
        match self {
            Direction::Lower => score < best,
            Direction::Higher => score > best,
        }
    }
}

/// The reversibly-mutable state the agent changes each turn.
///
/// The snapshot token is an **opaque string**: the engine captures one and hands it back to
/// [`World::restore`] on discard, never inspecting it. (A git world returns a commit sha; a
/// command world returns `"<git-sha>\t<domain-token>"`.) Object-safe on purpose, so the engine
/// can hold `&dyn World` and pick the domain at runtime from a manifest.
///
/// `Send + Sync` so an executor can own the world behind an `Arc` rather than borrowing it from
/// the driver's frame. Every method takes `&self`, so shared ownership costs nothing.
pub trait World: Send + Sync {
    /// Make the agent's freshly-edited candidate live, after its turn and before the judge
    /// measures. The default is a no-op: for code/agent-edit worlds the edit *is* the candidate
    /// (git memory captures it on the next snapshot). A deploy domain overrides this to
    /// build+push+set-image. A nonzero result means the candidate failed to apply (discard it).
    fn apply(&self) -> Result<()> {
        Ok(())
    }

    /// Capture the current state as a rollback token, committing the workspace as memory.
    /// `label` is the commit message for git-backed worlds (ignored by others).
    fn snapshot(&self, label: &str) -> Result<String>;

    /// Roll the world back to a token from [`World::snapshot`].
    fn restore(&self, snap: &str) -> Result<()>;

    /// The git commit recorded in `snap`, if this world keeps git memory, for the publish
    /// layer's `kept_shas`. Worlds without git memory return `None`.
    fn commit_sha(&self, _snap: &str) -> Option<String> {
        None
    }

    /// For a multi-workspace (composite) world, the per-component publish targets: each component's
    /// name, workspace, and the base/head git shas read out of the two snapshot tokens. `None` for a
    /// single-repo world (the publish layer uses [`World::commit_sha`] + the single `pr_repo` instead).
    /// `base_snap` is the pristine baseline token; `best_snap` is the kept-candidate token. The fork
    /// each component PRs against is joined in by name from the manifest, not the World's concern.
    fn publish_components(
        &self,
        _base_snap: &str,
        _best_snap: &str,
    ) -> Option<Vec<PublishComponent>> {
        None
    }

    /// The agent's staged edits this turn as (diff text, shortstat), for the results row. The
    /// default is empty; git-backed worlds override to stage + diff their workspace(s). A composite
    /// world diffs EVERY component repo and combines them (the base dir isn't a repo, so a single
    /// top-level diff would miss the per-component edits entirely).
    fn staged_diff(&self) -> (String, String) {
        (String::new(), String::new())
    }
}

/// The frozen objective. Outside the agent's reach by construction: the engine owns it,
/// the agent only ever sees the [`World`].
///
/// `Send + Sync` for the same reason as [`World`]: shared ownership by an executor, and every
/// method takes `&self`.
pub trait Judge: Send + Sync {
    /// Measure the currently-applied candidate. `ctx` carries the run-so-far numbers a
    /// command judge exposes to its measure command as env; domains that don't need them
    /// ignore it.
    fn measure(&self, ctx: &MeasureCtx) -> Result<Reading>;

    /// Keep this reading? `best_score`/`best_tiebreak` are the run-so-far context the engine
    /// tracks (the tiebreak is the kept best's secondary scalar, `None` when it had none).
    fn decide(&self, reading: &Reading, best_score: f64, best_tiebreak: Option<f64>) -> Decision;

    /// One-line objective status injected into the agent's prompt (e.g. "210 ms").
    fn status(&self, best_score: f64) -> String;

    /// Did the whole run improve on baseline? Drives the process exit code.
    fn improved(&self, best_score: f64, baseline_score: f64, solved_any: bool) -> bool;

    /// Human-readable detail column for the results row.
    fn detail(&self, reading: &Reading) -> String;

    /// The objective's display label for the frontends (e.g. "bench", "value", "p99_ms").
    /// Domain-neutral: the engine and reporters carry it as an opaque string.
    fn objective(&self) -> String;

    /// Which way is better, for engine-side score seeding (e.g. a skipped baseline starts at the
    /// worst value so any valid candidate is kept).
    fn direction(&self) -> Direction;
}

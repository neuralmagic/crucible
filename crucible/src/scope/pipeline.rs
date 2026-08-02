use crate::Paths;
use crate::agent::{self, AgentBackend};
use crate::check::{self, CheckOutcome};
use crate::event::{AgentEvent, RawStream};
use crate::identity::{self, RunIdentity};
use crate::init::MANIFEST_FILE;
use crate::manifest;
use crate::refine::{
    self, Attack, FailureEvidence, MIN_PROPOSED_SELFTEST_RUNS, RoundKind, RoundOutcome, RoundRecord,
};
use crate::scope::pack::{
    PACK_WORK_DIR, drop_repo_checkouts, ensure_out_dir, frozen_workspace_dir,
    normalize_frozen_manifest, scratch_dir, strip_controls_and_selftest, sync_pack_from_workspace,
};
use crate::scope::progress::{ActivityFeed, emit_progress};
use crate::scope::transcript::{transcript_event, transcript_note, write_seed_context};
use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Serialize;
use std::path::{Path, PathBuf};

const SCOPE_PROPOSE_PROMPT: &str = include_str!("../prompts/scope-propose.md");

/// The `{{GOAL_CONTRACT}}` section: de-prescribe the issue into a neutral problem framing (the
/// default), or (for an authoritative brief) carry its prescriptions into `goal.md` intact.
const GOAL_CONTRACT: &str = include_str!("../prompts/scope-goal-contract.md");
const GOAL_CONTRACT_AUTHORITATIVE: &str =
    include_str!("../prompts/scope-goal-contract-authoritative.md");

/// The confirmed tier a propose turn drafts against: threaded from the controller's ranker verdict
/// (`--tier t0|t1`) into the prompt's `{{TIER}}` slot, so the agent follows the right section of
/// `scope-propose.md` instead of guessing.
/// `T0` is the engine default when the flag is absent, every call site predating this flag never set
/// it, and a bare `crucible scope --propose` (no controller in front of it) should keep behaving exactly
/// as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ProposeTier {
    #[default]
    #[value(name = "t0")]
    T0,
    #[value(name = "t1")]
    T1,
}

impl ProposeTier {
    /// The `{{TIER}}` spelling the prompt substitutes, matches the ranker/DB vocabulary
    /// (`Tier::as_str` in the controller crate) so a human reading the rendered prompt recognizes
    /// it as the same tier the ledger shows.
    pub fn as_str(self) -> &'static str {
        match self {
            ProposeTier::T0 => "T0",
            ProposeTier::T1 => "T1",
        }
    }

    /// The `--tier` CLI spelling (the clap `#[value(name)]`s above), what the turn-pod wrapper
    /// renders into its `crucible scope --propose --tier …` invocation.
    pub fn cli_value(self) -> &'static str {
        match self {
            ProposeTier::T0 => "t0",
            ProposeTier::T1 => "t1",
        }
    }
}

/// One stage's result, independent of how the pipeline renders it (console lines or one JSON
/// object).
#[derive(Debug, Clone, Serialize)]
pub struct StageResult {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

/// A stage in the pipeline: given the running context, either advance it and report success, or
/// fail with the reason. S2 (propose) / S3 (preflight) / S4 (approval) each add one more impl
/// here, the runner in [`run`] doesn't otherwise change shape.
pub(super) trait Stage {
    fn name(&self) -> &'static str;
    fn run(&self, ctx: &mut ScopeCtx) -> Result<String>;
}

/// State threaded through the pipeline, built up one stage at a time.
pub(super) struct ScopeCtx {
    pub(super) pack: PathBuf,
    pub(super) manifest_path: PathBuf,
    pub(super) goal: Option<String>,
    pub(super) goal_source: Option<String>,
    pub(super) check_outcome: Option<CheckOutcome>,
    pub(super) identity: Option<RunIdentity>,
    /// The propose turn's cost (USD), set only when `--propose` ran a [`Propose`] stage. Summed
    /// across every refine round.
    pub(super) propose_cost: Option<f64>,
    /// The refine loop's per-round trail, empty on the hand-written pack path.
    pub(super) refine_rounds: Vec<RoundRecord>,
    /// The turns' preserved session NDJSON (`SessionEvent` lines, one `note` delimiter per round),
    /// accumulated across every propose/refine/adversary turn. Empty outside `--propose`.
    pub(super) transcript: String,
    /// The bounded live-activity feed the turn sinks drive (`--marker` only; disabled = silent).
    pub(super) activity: ActivityFeed,
}

/// Stage 1: resolve the run's goal text. `--issue` and `--goal-file` are mutually exclusive
/// (enforced by the CLI); absent both, the pack manifest's own `[agent].goal`/`goal_file` must
/// resolve.
pub(super) struct Ingest {
    pub(super) issue: Option<String>,
    pub(super) goal_file: Option<PathBuf>,
}

impl Stage for Ingest {
    fn name(&self) -> &'static str {
        "ingest"
    }

    fn run(&self, ctx: &mut ScopeCtx) -> Result<String> {
        let (goal, source) = if let Some(issue) = &self.issue {
            let (repo, number) = parse_issue(issue)?;
            (goal_from_issue(&repo, number)?, format!("--issue {issue}"))
        } else if let Some(f) = &self.goal_file {
            let goal = std::fs::read_to_string(f)
                .with_context(|| format!("reading --goal-file {}", f.display()))?;
            (goal, format!("--goal-file {}", f.display()))
        } else {
            manifest_goal(&ctx.manifest_path)?
        };
        let detail = format!("resolved goal from {source} ({} bytes)", goal.len());
        ctx.goal = Some(goal);
        ctx.goal_source = Some(source);
        Ok(detail)
    }
}

/// `--propose` mode's inputs: the code under test and the propose turn's own agent (real Claude
/// by default; a scripted `command` override for tests, threaded the same way a domain's
/// `[agent].backend = "command"` stands in for an LLM).
pub struct ProposeOpts {
    pub repo: String,
    pub max_cost: f64,
    pub agent_cmd_override: Option<String>,
    /// Same flag as the pipeline's `--force`: permit proposing into a non-empty `--out`.
    pub force: bool,
    /// Total rounds the refine loop may spend: round 1
    /// is the propose turn, each round after is a refine turn seeded with the prior round's failure
    /// evidence. Clamped to at least 1 (a lone propose turn, today's behavior).
    pub refine_rounds: u32,
    /// Skip the adversarial gaming-review turn after a round validates. Default is to run it, this
    /// exists for fast dev iteration on the pipeline itself, not for real use.
    pub skip_gaming_review: bool,
    /// Max concern→refine→re-review cycles the gaming review may spend (`--gaming-refine-rounds`).
    /// Every refined pack must re-validate and earn a fresh adversary look; the last look is final
    /// (still-concerns fails closed). Clamped to at least 1 (today's one-cycle behavior).
    pub gaming_refine_rounds: u32,
    /// The ranker's confirmed tier for this issue, rendered into the prompt's
    /// `{{TIER}}` slot. Defaults to `T0` (back-compat) when the caller doesn't set it.
    pub tier: ProposeTier,
    /// The real agent backend for the propose/refine/adversary turns (`--agent-backend`):
    /// `local` on a laptop, `openshell` in a turn pod whose sandbox image carries the claude
    /// CLI. `agent_cmd_override` wins over this, same precedence as `rank-grounded`.
    pub agent_backend: AgentBackend,
    /// Sandbox image for the `openshell` backend (`--sandbox-image`).
    pub sandbox_image: Option<String>,
    /// Emit a `CRUCIBLE_SCOPE_PROGRESS: {json}` line at each round boundary (set by `--marker`,
    /// alongside the terminal report marker), so a live log tail can show per-round progress.
    pub progress: bool,
    /// OpenShell compute driver for the turns' gateway (`--compute-driver`), mirroring
    /// `rank-grounded`. [`turn_args`] synthesizes a fresh `Args` via `Cli::parse_from`, which
    /// otherwise always defaults it to `Podman` regardless of the caller's deployment.
    pub compute_driver: crate::openshell::gateway::ComputeDriver,
    /// The goal is an authoritative brief (`--authoritative`): the propose/refine prompts tell
    /// the agent to carry its prescriptions into `goal.md` intact instead of de-prescribing them.
    pub authoritative: bool,
}

/// Stage 2 (`--propose` only): one agent turn drafts the pack into `ctx.pack`. Runs after
/// [`Ingest`] (needs `ctx.goal`) and before [`Validate`], nothing here is trusted; it only earns
/// the right to be mechanically checked.
pub(super) struct Propose {
    pub(super) opts: ProposeOpts,
}

impl Stage for Propose {
    fn name(&self) -> &'static str {
        "propose"
    }

    fn run(&self, ctx: &mut ScopeCtx) -> Result<String> {
        ensure_out_dir(&ctx.pack, self.opts.force)?;
        // ctx.pack is the host-side pack the pipeline validates and freezes; canonicalize it so
        // stage details and REJECTED.md name a stable absolute path. The agent never sees it:
        // the prompts spell the pack as PACK_WORK_DIR relative to the turn's cwd, because on the
        // openshell backend only the workspace round-trips the sandbox.
        ctx.pack = std::fs::canonicalize(&ctx.pack).unwrap_or_else(|_| ctx.pack.clone());
        ctx.manifest_path = ctx.pack.join(MANIFEST_FILE);
        let goal = ctx
            .goal
            .clone()
            .context("propose needs a resolved goal (ingest must have run first)")?;

        let scratch = scratch_dir("scope-propose-repo");
        crate::run::clone_repo(&self.opts.repo, None, &scratch)
            .context("checking out --repo for the propose turn")?;
        write_seed_context(&scratch, &goal)?;
        std::fs::create_dir_all(scratch.join(PACK_WORK_DIR))
            .with_context(|| format!("creating {PACK_WORK_DIR} in {}", scratch.display()))?;

        // The refine loop reuses this one scratch checkout across every round; clean it up on every
        // exit path (pass, decline, or exhaustion) so a bail doesn't leak the temp dir.
        let result = self.drive_refine_loop(ctx, &scratch, &goal);
        let _ = std::fs::remove_dir_all(&scratch);
        result
    }
}

impl Propose {
    /// Round 1 is the propose turn; each round after is a refine turn seeded with the prior round's
    /// failure evidence. A round passes validation ⇒ freeze proceeds; a round fails ⇒ refine again
    /// if rounds remain, else write `REJECTED.md` with the whole trail and fail the stage. An agent
    /// that declines outright (writes its own `REJECTED.md`) is terminal, no refine.
    fn drive_refine_loop(&self, ctx: &mut ScopeCtx, scratch: &Path, goal: &str) -> Result<String> {
        let rounds_max = self.opts.refine_rounds.max(1);
        // The pack path as the agent sees it: relative to its cwd (the workspace), identical on
        // both backends. Never ctx.pack, that host path doesn't exist inside the sandbox.
        let pack_rel = Path::new(PACK_WORK_DIR);
        let mut total_cost = 0.0;
        let mut last_evidence: Option<FailureEvidence> = None;

        for round in 1..=rounds_max {
            if round > 1 && self.budget_exhausted(total_cost) {
                write_rejection(ctx, round - 1, last_evidence.as_ref());
                let summary = last_evidence
                    .as_ref()
                    .map(FailureEvidence::describe)
                    .unwrap_or_else(|| "no evidence captured".to_string());
                bail!(
                    "scope refine stopped before round {round} for {}: cumulative turn cost \
                     ${total_cost:.4} already meets --max-cost ${:.2}, budget exhausted; last \
                     failure: {summary}",
                    ctx.pack.display(),
                    self.opts.max_cost
                );
            }
            let kind = if round == 1 {
                RoundKind::Propose
            } else {
                RoundKind::Refine
            };
            let prompt = match (&kind, &last_evidence) {
                (RoundKind::Refine, Some(ev)) => refine::render_refine_prompt(
                    goal,
                    pack_rel,
                    ev,
                    round,
                    self.opts.tier,
                    self.opts.authoritative,
                ),
                _ => render_propose_prompt(
                    goal,
                    pack_rel,
                    &self.opts.repo,
                    self.opts.tier,
                    self.opts.authoritative,
                ),
            };
            let doing = match (&kind, &last_evidence) {
                (RoundKind::Refine, Some(ev)) => {
                    format!(
                        "refining the pack on round {}'s failure: {}",
                        round - 1,
                        ev.describe()
                    )
                }
                _ => "drafting the pack from the goal".to_string(),
            };
            emit_progress(self.opts.progress, round, kind, &doing, total_cost);
            transcript_note(
                &mut ctx.transcript,
                &format!("round {round}: {} turn", kind.label()),
            );
            ctx.activity.begin_turn(total_cost);
            let cost = run_propose_turn(
                scratch,
                &prompt,
                &self.opts,
                &mut ctx.transcript,
                &mut ctx.activity,
            );
            total_cost += cost;
            ctx.propose_cost = Some(total_cost);
            sync_pack_from_workspace(scratch, &ctx.pack)?;

            // A deliberate decline (the agent wrote its own REJECTED.md) is a clean, terminal
            // answer, never something to refine. Our own exhaustion-REJECTED (below) only lands
            // after the loop, so this can only be the agent's.
            let rejected = ctx.pack.join("REJECTED.md");
            if rejected.exists() {
                let reason = std::fs::read_to_string(&rejected).unwrap_or_default();
                bail!("proposer declined this issue: {}", reason.trim());
            }

            let judge_block = read_judge_block(&ctx.manifest_path);
            match compile_and_validate_round(&ctx.manifest_path) {
                RoundVerdict::Passed(outcome) => {
                    ctx.refine_rounds.push(RoundRecord {
                        round,
                        kind,
                        judge_block,
                        cost,
                        outcome: RoundOutcome::Passed,
                    });
                    ctx.check_outcome = Some(outcome);
                    if self.opts.skip_gaming_review {
                        return self.finalize_pack(ctx, round, total_cost);
                    }
                    return self.run_gaming_review(ctx, scratch, goal, round, &mut total_cost);
                }
                RoundVerdict::Failed(evidence) => {
                    ctx.refine_rounds.push(RoundRecord {
                        round,
                        kind,
                        judge_block,
                        cost,
                        outcome: RoundOutcome::Failed {
                            evidence: evidence.clone(),
                        },
                    });
                    last_evidence = Some(evidence);
                }
            }
        }

        // Rounds exhausted with no passing gate: freeze the whole trail into REJECTED.md so a human
        // (and the controller's checkpoint UI) can see every round, and fail the stage.
        write_rejection(ctx, rounds_max, last_evidence.as_ref());
        let summary = last_evidence
            .as_ref()
            .map(FailureEvidence::describe)
            .unwrap_or_else(|| "no evidence captured".to_string());
        bail!(
            "scope refine exhausted {rounds_max} round(s) without a discriminating gate for {} \
             (total turn cost ${total_cost:.4}); last failure: {summary}",
            ctx.pack.display()
        )
    }

    /// The between-rounds budget gate: with `--max-cost` set, another agent turn never starts
    /// once the cumulative cost has already met it, the loop stops with an honest
    /// budget-exhausted failure instead of burning more rounds. (The terminal report still flags
    /// over-budget on top, for the round that crossed the line.)
    fn budget_exhausted(&self, total_cost: f64) -> bool {
        self.opts.max_cost > 0.0 && total_cost >= self.opts.max_cost
    }

    /// The freeze-stage detail string once a round has validated: which round it took, the total
    /// turn cost so far, and an over-budget note if `--max-cost` was exceeded.
    fn freeze_detail(&self, ctx: &ScopeCtx, round: u32, total_cost: f64) -> String {
        let mut detail = format!(
            "drafted {} in {round} round(s) (total turn cost ${total_cost:.4})",
            ctx.manifest_path.display()
        );
        if self.opts.max_cost > 0.0 && total_cost > self.opts.max_cost {
            detail.push_str(&format!(
                " -- OVER BUDGET: ${total_cost:.4} exceeds --max-cost ${:.2}",
                self.opts.max_cost
            ));
        }
        detail
    }

    /// Turn the winning drafted pack into a RUNNABLE, shippable one, then re-validate it, the last
    /// thing the propose stage does before the pipeline freezes it. Rewrites the manifest to be
    /// dispatchable ([`normalize_frozen_manifest`]), re-runs `crucible check` over the result so a
    /// rewrite bug fails the scope loudly here instead of shipping a broken pack, then de-prescribes
    /// it ([`strip_controls_and_selftest`], AFTER the self-test probe that needs the controls) and
    /// drops the run pod's own checkout ([`drop_repo_checkouts`]). The re-validated outcome replaces
    /// the stored one, so the freeze digest (computed later, over the rewritten, de-prescribed
    /// manifest) covers exactly what ships.
    fn finalize_pack(&self, ctx: &mut ScopeCtx, round: u32, total_cost: f64) -> Result<String> {
        normalize_frozen_manifest(
            &ctx.manifest_path,
            &self.opts.repo,
            self.opts.agent_backend,
            self.opts.sandbox_image.as_deref(),
        )
        .with_context(|| {
            format!(
                "normalizing the frozen pack manifest {}",
                ctx.manifest_path.display()
            )
        })?;

        // Re-validate WHILE the check-populated workspace still exists, so the probe reuses it
        // instead of cloning the freshly-pinned remote `[repo]` (a network round-trip the freeze
        // doesn't need). A failure here is a rewrite bug, fail the scope loudly, never ship it.
        match compile_and_validate_round(&ctx.manifest_path) {
            RoundVerdict::Passed(outcome) => ctx.check_outcome = Some(outcome),
            RoundVerdict::Failed(evidence) => bail!(
                "the frozen pack failed re-validation after the freeze rewrite for {}: {}",
                ctx.pack.display(),
                evidence.describe()
            ),
        }

        // De-prescribe the shipped pack: the self-test has already run (during the re-validation
        // above), so its reference fix has done its whole job. Strip `_controls/` and the
        // `[judge.selftest]` block now, AFTER the probe that needed them, BEFORE the freeze digest
        // (computed in the later `Freeze` stage over this rewritten manifest) so the digest covers
        // exactly the de-prescribed state that ships. A run-side `crucible check` then sees no
        // half-stripped selftest to trip on.
        strip_controls_and_selftest(&ctx.manifest_path, &ctx.pack).with_context(|| {
            format!(
                "de-prescribing the frozen pack {}",
                ctx.manifest_path.display()
            )
        })?;

        // Only now strip the checkout: the run phase rebuilds it from the pinned [repo].
        let ws_dir = frozen_workspace_dir(&ctx.manifest_path);
        drop_repo_checkouts(&ctx.pack, &ws_dir)?;
        Ok(self.freeze_detail(ctx, round, total_cost))
    }

    /// The adversarial gaming review: after a round validates, run the bounded
    /// adversarial gaming-review loop before freeze. `pass` freezes as normal; `concerns` earns a
    /// refine round seeded with the attacks followed by a re-validate and a fresh review, up to
    /// `--gaming-refine-rounds` cycles (the allowance is its own, never drawn from `--refine-rounds`).
    /// The last look is always final, concerns there, a failed re-validate, or a verdict that never
    /// parses all reject with the whole trail. Never called when `--skip-gaming-review` is set.
    fn run_gaming_review(
        &self,
        ctx: &mut ScopeCtx,
        scratch: &Path,
        goal: &str,
        passed_round: u32,
        total_cost: &mut f64,
    ) -> Result<String> {
        let cycles_max = self.opts.gaming_refine_rounds.max(1);
        let mut last_passed = passed_round;
        let mut refines_done = 0u32;
        loop {
            let review_round = last_passed + 1;
            let attacks = match self.adversary_turn(ctx, goal, review_round, total_cost) {
                AdversaryOutcome::Passed => {
                    return self.finalize_pack(ctx, last_passed, *total_cost);
                }
                AdversaryOutcome::Errored(detail) => {
                    let (note_stage, bail_stage) = if refines_done == 0 {
                        ("gaming-review", "")
                    } else {
                        ("gaming re-review", " on re-review")
                    };
                    write_rejection_note(
                        ctx,
                        review_round,
                        &format!("the {note_stage} turn produced no parseable verdict: {detail}"),
                    );
                    bail!(
                        "scope gaming review produced no parseable verdict{bail_stage} for {} \
                         (total turn cost ${:.4}): {detail}",
                        ctx.pack.display(),
                        *total_cost
                    )
                }
                AdversaryOutcome::Concerns(attacks) => attacks,
            };

            let evidence = FailureEvidence::Adversary { attacks };
            if refines_done >= cycles_max {
                write_rejection(ctx, review_round, Some(&evidence));
                bail!(
                    "scope gaming review still found concerns after {cycles_max} refine round(s) \
                     for {} (total turn cost ${:.4})",
                    ctx.pack.display(),
                    *total_cost
                )
            }
            if self.budget_exhausted(*total_cost) {
                write_rejection(ctx, review_round, Some(&evidence));
                bail!(
                    "scope gaming review found concerns for {} but the budget is exhausted \
                     (cumulative turn cost ${:.4} already meets --max-cost ${:.2}) — no refine round",
                    ctx.pack.display(),
                    *total_cost,
                    self.opts.max_cost
                );
            }

            let refine_round = review_round + 1;
            let prompt = refine::render_refine_prompt(
                goal,
                Path::new(PACK_WORK_DIR),
                &evidence,
                refine_round,
                self.opts.tier,
                self.opts.authoritative,
            );
            emit_progress(
                self.opts.progress,
                refine_round,
                RoundKind::Refine,
                "refining the pack on the gaming review's concerns",
                *total_cost,
            );
            transcript_note(
                &mut ctx.transcript,
                &format!("round {refine_round}: refine turn (gaming-review concerns)"),
            );
            ctx.activity.begin_turn(*total_cost);
            let cost = run_propose_turn(
                scratch,
                &prompt,
                &self.opts,
                &mut ctx.transcript,
                &mut ctx.activity,
            );
            *total_cost += cost;
            ctx.propose_cost = Some(*total_cost);
            sync_pack_from_workspace(scratch, &ctx.pack)?;

            let rejected = ctx.pack.join("REJECTED.md");
            if rejected.exists() {
                let reason = std::fs::read_to_string(&rejected).unwrap_or_default();
                bail!("proposer declined this issue: {}", reason.trim());
            }

            let judge_block = read_judge_block(&ctx.manifest_path);
            match compile_and_validate_round(&ctx.manifest_path) {
                RoundVerdict::Failed(ev) => {
                    ctx.refine_rounds.push(RoundRecord {
                        round: refine_round,
                        kind: RoundKind::Refine,
                        judge_block,
                        cost,
                        outcome: RoundOutcome::Failed {
                            evidence: ev.clone(),
                        },
                    });
                    write_rejection(ctx, refine_round, Some(&ev));
                    bail!(
                        "scope gaming review's refine round {refine_round} failed validation for \
                         {} (total turn cost ${:.4}): {}",
                        ctx.pack.display(),
                        *total_cost,
                        ev.describe()
                    )
                }
                RoundVerdict::Passed(outcome) => {
                    ctx.refine_rounds.push(RoundRecord {
                        round: refine_round,
                        kind: RoundKind::Refine,
                        judge_block,
                        cost,
                        outcome: RoundOutcome::Passed,
                    });
                    ctx.check_outcome = Some(outcome);
                    // The fix earns exactly one more look, not a free pass.
                    last_passed = refine_round;
                    refines_done += 1;
                }
            }
        }
    }

    /// Run one bounded, read-only adversarial gaming-review turn against the already-validated
    /// pack, record it as an `Adversary` round, and classify its outcome. Always appends exactly
    /// one [`RoundRecord`] to `ctx.refine_rounds` regardless of which branch it takes.
    fn adversary_turn(
        &self,
        ctx: &mut ScopeCtx,
        goal: &str,
        round: u32,
        total_cost: &mut f64,
    ) -> AdversaryOutcome {
        // The adversary's workspace IS the pack dir, so its cwd is the pack on both backends;
        // spell the path as `.`, ctx.pack (a host-absolute path) doesn't exist in the sandbox.
        let prompt = refine::render_adversary_prompt(goal, Path::new("."), &ctx.refine_rounds);
        emit_progress(
            self.opts.progress,
            round,
            RoundKind::Adversary,
            "adversarial gaming review of the validated pack",
            *total_cost,
        );
        transcript_note(
            &mut ctx.transcript,
            &format!("round {round}: adversary turn (gaming review)"),
        );
        ctx.activity.begin_turn(*total_cost);
        let (cost, transcript) = run_adversary_turn(
            &ctx.pack,
            &prompt,
            &self.opts,
            &mut ctx.transcript,
            &mut ctx.activity,
        );
        *total_cost += cost;
        ctx.propose_cost = Some(*total_cost);
        let judge_block = read_judge_block(&ctx.manifest_path);

        let (outcome, result) = match refine::parse_adversary_verdict(&transcript) {
            Ok(refine::AdversaryVerdict::Pass) => (RoundOutcome::Passed, AdversaryOutcome::Passed),
            Ok(refine::AdversaryVerdict::Concerns { attacks }) => (
                RoundOutcome::Failed {
                    evidence: FailureEvidence::Adversary {
                        attacks: attacks.clone(),
                    },
                },
                AdversaryOutcome::Concerns(attacks),
            ),
            Err(detail) => (
                RoundOutcome::Error {
                    detail: detail.clone(),
                },
                AdversaryOutcome::Errored(detail),
            ),
        };
        ctx.refine_rounds.push(RoundRecord {
            round,
            kind: RoundKind::Adversary,
            judge_block,
            cost,
            outcome,
        });
        result
    }
}

/// The adversary turn's classified result, for [`Propose::run_gaming_review`] to act on. Distinct
/// from [`refine::AdversaryVerdict`]: this also carries the malformed-output case, which has no
/// verdict to speak of.
enum AdversaryOutcome {
    Passed,
    Concerns(Vec<Attack>),
    /// The turn's output didn't parse as a verdict at all, fail-closed, never a pass.
    Errored(String),
}

/// Run the adversary turn against `pack` itself (its cwd, it reads the manifest, the measure
/// script, and the workspace directly rather than having file contents embedded in the prompt) and
/// return its cost plus the concatenated stdout text, for [`refine::parse_adversary_verdict`] to
/// read the final line from. Uses a throwaway scratch dir for the turn's own state/steer/session
/// bookkeeping so nothing extra lands in the frozen pack directory.
fn run_adversary_turn(
    pack: &Path,
    prompt: &str,
    opts: &ProposeOpts,
    session: &mut String,
    activity: &mut ActivityFeed,
) -> (f64, String) {
    let args = turn_args(opts);
    let model = args.model.clone();
    let meta = scratch_dir("scope-adversary-meta");
    let paths = Paths {
        workspace: pack.to_path_buf(),
        skills: None,
        steer: meta.join("STEER.md"),
        state: meta.join("state"),
        session_log: meta.join("state/session.jsonl"),
        control: meta.join("state/control.json"),
        escalation: meta.join("ESCALATION.json"),
        provisioning: meta.join("PROVISIONING_PENDING.json"),
    };
    let _ = std::fs::create_dir_all(&paths.state);
    let mut transcript = String::new();
    let cost = agent::run_turn(&args, &paths, prompt, false, |_line, stream, ev| {
        forward_error(ev);
        if let Some(ev) = ev {
            transcript_event(session, ev);
            activity.observe(&model, ev);
        }
        if stream != RawStream::Stdout {
            return;
        }
        match ev {
            Some(AgentEvent::Text { delta }) => transcript.push_str(delta),
            Some(AgentEvent::Raw { text, .. }) => {
                transcript.push_str(text);
                transcript.push('\n');
            }
            _ => {}
        }
    });
    let _ = std::fs::remove_dir_all(&meta);
    (cost, transcript)
}

/// Write `REJECTED.md` for a gaming-review outcome that has no [`FailureEvidence`] to speak of (a
/// malformed adversary verdict), a plain note plus the trail, instead of forcing the malformation
/// into the evidence shape.
fn write_rejection_note(ctx: &ScopeCtx, round: u32, note: &str) {
    let mut body = format!(
        "# REJECTED.md — {pack}\n\n\
         `crucible scope --propose` stopped at round {round}: {note}\n\n\
         ## Refine trail\n\n{trail}\n",
        pack = ctx.pack.display(),
        trail = refine::render_rounds_json(&ctx.refine_rounds),
    );
    body.push_str("\nThis pack was NOT frozen; no SCOPE.md was written.\n");
    let _ = std::fs::write(ctx.pack.join("REJECTED.md"), body);
}

/// One round's validation verdict: a passing `CheckOutcome` (the loop freezes it) or the concrete
/// evidence of why it failed (the loop refines on it).
enum RoundVerdict {
    Passed(CheckOutcome),
    Failed(FailureEvidence),
}

/// Validate a proposed pack for one refine round: structural preconditions, then the full
/// `crucible check`. Failures are classified into the stage-tagged [`FailureEvidence`].
///
/// Writes to the pack: a sibling `workflow.star` is compiled into `[[workflow.task]]` first,
/// since there is nothing to validate until it is. That happens even for rounds that fail.
fn compile_and_validate_round(manifest_path: &Path) -> RoundVerdict {
    if !manifest_path.exists() {
        return RoundVerdict::Failed(FailureEvidence::Structure {
            detail: format!(
                "no {} was written — the turn produced no pack to validate",
                MANIFEST_FILE
            ),
        });
    }
    if let Err(error) = crate::plan::starlark::materialize_sibling_manifest(manifest_path) {
        return RoundVerdict::Failed(FailureEvidence::Structure {
            detail: format!("workflow.star did not compile into [[workflow.task]]: {error:#}"),
        });
    }
    let m = match manifest::Manifest::load(manifest_path) {
        Ok(m) => m,
        Err(e) => {
            return RoundVerdict::Failed(FailureEvidence::Structure {
                detail: format!("the manifest didn't parse: {e:#}"),
            });
        }
    };
    match &m.judge.selftest {
        None => {
            return RoundVerdict::Failed(FailureEvidence::Structure {
                detail: "the manifest has no [judge.selftest] table — the controls are how a \
                     proposed gate proves it discriminates a real fix from a no-op"
                    .to_string(),
            });
        }
        Some(cfg) if cfg.runs < MIN_PROPOSED_SELFTEST_RUNS => {
            return RoundVerdict::Failed(FailureEvidence::Structure {
                detail: format!(
                    "[judge.selftest].runs = {} is below the proposed-pack floor of {} — one \
                     reading can't prove a noisy gate discriminates; average over at least {} runs",
                    cfg.runs, MIN_PROPOSED_SELFTEST_RUNS, MIN_PROPOSED_SELFTEST_RUNS
                ),
            });
        }
        Some(_) => {}
    }

    let outcome = match check::run(manifest_path) {
        Ok(o) => o,
        Err(e) => {
            return RoundVerdict::Failed(FailureEvidence::Structure {
                detail: format!("crucible check failed to run: {e:#}"),
            });
        }
    };
    if outcome.ok() {
        return RoundVerdict::Passed(outcome);
    }
    // A self-test that ran but didn't discriminate is its own evidence; anything else that failed
    // the check is a contract-probe problem (nonzero exit, no/invalid JSON line, missing files).
    match &outcome.selftest {
        Some(report) if !report.passed => {
            RoundVerdict::Failed(FailureEvidence::Selftest(report.into()))
        }
        _ => RoundVerdict::Failed(FailureEvidence::Contract {
            findings: outcome.findings.clone(),
            stderr_tail: outcome.measure_stderr_tail.clone(),
        }),
    }
}

/// Best-effort read of the manifest's `[judge]` region for a round record (empty if it's absent or
/// unreadable, exactly the STRUCTURE-failure case).
fn read_judge_block(manifest_path: &Path) -> String {
    std::fs::read_to_string(manifest_path)
        .map(|t| refine::extract_judge_block(&t))
        .unwrap_or_default()
}

/// Write `REJECTED.md` carrying the full round trail (human summary + the fenced-JSON records the
/// controller's checkpoint UI deserializes). Best-effort: a write failure here shouldn't mask the stage failure.
fn write_rejection(ctx: &ScopeCtx, rounds: u32, last: Option<&FailureEvidence>) {
    let mut body = format!(
        "# REJECTED.md — {pack}\n\n\
         `crucible scope --propose` refined {rounds} round(s) without producing a gate that \
         validates. The full trail is below for review.\n\n\
         ## Last failure\n\n{last}\n\n\
         ## Refine trail\n\n{trail}\n",
        pack = ctx.pack.display(),
        last = last
            .map(FailureEvidence::describe)
            .unwrap_or_else(|| "no evidence captured".to_string()),
        trail = refine::render_rounds_json(&ctx.refine_rounds),
    );
    body.push_str("\nThis pack was NOT frozen; no SCOPE.md was written.\n");
    let _ = std::fs::write(ctx.pack.join("REJECTED.md"), body);
}

fn repo_directive(repo: &str) -> String {
    if repo.contains("://") || repo.ends_with(".git") || repo.contains('@') {
        format!("url = \"{repo}\"")
    } else {
        let abs = std::fs::canonicalize(repo).unwrap_or_else(|_| PathBuf::from(repo));
        format!("path = \"{}\"", abs.display())
    }
}

fn render_propose_prompt(
    goal: &str,
    out_dir: &Path,
    repo: &str,
    tier: ProposeTier,
    authoritative: bool,
) -> String {
    let contract = if authoritative {
        GOAL_CONTRACT_AUTHORITATIVE
    } else {
        GOAL_CONTRACT
    };
    SCOPE_PROPOSE_PROMPT
        .replace("{{GOAL_CONTRACT}}", contract.trim_end())
        .replace("{{GOAL}}", goal)
        .replace("{{OUT_DIR}}", &out_dir.display().to_string())
        .replace("{{REPO_DIRECTIVE}}", &repo_directive(repo))
        .replace("{{TIER}}", tier.as_str())
}

/// Build the one-turn run args both scope turns share. Precedence mirrors `rank-grounded`: an
/// explicit `agent_cmd_override` (the scripted test double) wins, else the `--agent-backend`
/// choice; the sandbox image rides along for `openshell`.
fn turn_args(opts: &ProposeOpts) -> crate::Args {
    let mut args = crate::Cli::parse_from(["crucible"]).run;
    match &opts.agent_cmd_override {
        Some(cmd) => {
            args.agent_backend = AgentBackend::Command;
            args.agent_cmd = Some(cmd.clone());
        }
        None => args.agent_backend = opts.agent_backend,
    }
    args.sandbox_image = opts.sandbox_image.clone();
    args.compute_driver = opts.compute_driver;
    // Manifest-less turn: the Vertex agent env normally supplied by `[agent].env` comes from the
    // turn pod's own env instead.
    crate::openshell::relay_vertex_env(&mut args.env);
    args
}

/// The sinks below discard most events; agent errors always reach stderr, a backend that dies at
/// spawn/auth otherwise leaves no trace in pod logs (three silent rounds, $0.0000, no pack).
fn forward_error(ev: Option<&AgentEvent>) {
    if let Some(AgentEvent::Error {
        error_type,
        message,
    }) = ev
    {
        eprintln!("[crucible scope] agent error ({error_type}): {message}");
    }
}

/// Run the propose turn in `scratch` (cwd = the `--repo` checkout) and return its cost. The
/// backend comes from `opts` via [`turn_args`]; the real turn is the same `agent::run_turn` every
/// domain loop iteration uses. Every decoded event is preserved into `session`, the scratch dir
/// (and any session file in it) is deleted when the turn ends, so this recording is the only
/// transcript that survives.
fn run_propose_turn(
    scratch: &Path,
    prompt: &str,
    opts: &ProposeOpts,
    session: &mut String,
    activity: &mut ActivityFeed,
) -> f64 {
    let args = turn_args(opts);
    let model = args.model.clone();
    let paths = propose_paths(scratch);
    let _ = std::fs::create_dir_all(&paths.state);
    agent::run_turn(&args, &paths, prompt, false, |_line, _stream, ev| {
        forward_error(ev);
        if let Some(ev) = ev {
            transcript_event(session, ev);
            activity.observe(&model, ev);
        }
    })
}

fn propose_paths(scratch: &Path) -> Paths {
    Paths {
        workspace: scratch.to_path_buf(),
        skills: None,
        steer: scratch.join("STEER.md"),
        state: scratch.join("state"),
        session_log: scratch.join("state/session.jsonl"),
        control: scratch.join("state/control.json"),
        escalation: scratch.join("ESCALATION.json"),
        provisioning: scratch.join("PROVISIONING_PENDING.json"),
    }
}

/// Stage 2: the existing `crucible check` machinery, called as a library, this pipeline never
/// reimplements manifest validation.
pub(super) struct Validate;

impl Stage for Validate {
    fn name(&self) -> &'static str {
        "validate"
    }

    fn run(&self, ctx: &mut ScopeCtx) -> Result<String> {
        crate::plan::starlark::materialize_sibling_manifest(&ctx.manifest_path)
            .context("compiling sibling workflow.star before scope validation")?;
        // On the `--propose` path the refine loop already validated the winning pack and stored its
        // outcome; reuse it instead of re-running the whole check (contract probe + self-test). On
        // the hand-written path there's no stored outcome, so run it now. Either way the outcome
        // stays in `ctx` for the freeze report to render.
        if ctx.check_outcome.is_none() {
            ctx.check_outcome = Some(check::run(&ctx.manifest_path)?);
        }
        let outcome = ctx
            .check_outcome
            .as_ref()
            .context("validate has an outcome by construction")?;
        let ok = outcome.ok();
        let mut detail = if ok {
            "crucible check: OK".to_string()
        } else {
            format!(
                "crucible check: {} finding(s): {}",
                outcome.findings.len(),
                outcome.findings.join("; ")
            )
        };
        if !outcome.warnings.is_empty() {
            detail.push_str(&format!(" (warnings: {})", outcome.warnings.join("; ")));
        }
        if !ok {
            bail!("{detail}");
        }
        Ok(detail)
    }
}

/// Stage 3: compute the pack's `RunIdentity` and write `SCOPE.md`. Refuses to overwrite an
/// existing `SCOPE.md` unless `force`.
pub(super) struct Freeze {
    pub(super) force: bool,
}

impl Stage for Freeze {
    fn name(&self) -> &'static str {
        "freeze"
    }

    fn run(&self, ctx: &mut ScopeCtx) -> Result<String> {
        let scope_md = ctx.pack.join("SCOPE.md");
        if scope_md.exists() && !self.force {
            bail!(
                "{} already exists (pass --force to overwrite)",
                scope_md.display()
            );
        }
        let identity = compute_identity(&ctx.manifest_path)?;
        let body = render_scope_md(ctx, &identity);
        std::fs::write(&scope_md, body)
            .with_context(|| format!("writing {}", scope_md.display()))?;
        let detail = format!("wrote {} (digest {})", scope_md.display(), identity.digest);
        ctx.identity = Some(identity);
        Ok(detail)
    }
}

/// `--issue owner/repo#N` -> `(owner/repo, N)`.
fn parse_issue(issue: &str) -> Result<(String, u64)> {
    let (repo, number) = issue
        .split_once('#')
        .with_context(|| format!("--issue must be owner/repo#N, got {issue:?}"))?;
    let number: u64 = number
        .parse()
        .with_context(|| format!("--issue number isn't a valid integer: {issue:?}"))?;
    Ok((repo.to_string(), number))
}

/// Fetch the issue from the GitHub REST API and return `# title\n\nbody` as the goal text.
/// Honors `GITHUB_TOKEN`/`GH_TOKEN` for private repos and rate limits (unauthenticated works
/// for public repos), and `GITHUB_API_URL` for GHES, the same variables Actions sets.
fn goal_from_issue(repo: &str, number: u64) -> Result<String> {
    let api = std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into());
    let url = format!("{}/repos/{repo}/issues/{number}", api.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .user_agent("crucible-scope")
        .build()
        .context("building the GitHub API client")?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty());
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url} returned {status}");
    }
    let issue: serde_json::Value = resp
        .json()
        .with_context(|| format!("decoding the response from {url}"))?;
    let title = issue["title"].as_str().unwrap_or("");
    let body = issue["body"].as_str().unwrap_or("");
    if title.is_empty() && body.is_empty() {
        bail!("issue {repo}#{number} has no title or body");
    }
    Ok(format!("# {title}\n\n{body}\n"))
}

/// Fall back to the pack manifest's own `[agent].goal`/`goal_file` (composite and single-domain
/// manifests share the same `[agent]` shape).
fn manifest_goal(manifest_path: &Path) -> Result<(String, String)> {
    let manifest_dir = manifest_dir_of(manifest_path);
    let agent = load_agent_cfg(manifest_path)?;
    match (&agent.goal, &agent.goal_file) {
        (Some(g), _) => Ok((g.clone(), "pack manifest [agent].goal".to_string())),
        (None, Some(f)) => {
            let goal = std::fs::read_to_string(manifest_dir.join(f))
                .with_context(|| format!("reading pack manifest goal_file {f}"))?;
            Ok((goal, format!("pack manifest [agent].goal_file {f}")))
        }
        (None, None) => bail!(
            "neither --issue, --goal-file, nor the pack manifest's [agent].goal/goal_file \
             resolved a goal"
        ),
    }
}

/// The de-prescribed goal that actually ships to the run agent: the pack's `[agent].goal`/
/// `goal_file` as the propose turn framed it. SCOPE.md renders this instead of the raw upstream
/// issue so the freeze record can't become a backdoor carrying prescriptive issue text into a
/// shipped pack file. `None` if the manifest can't be read (the caller falls back to `ctx.goal`).
fn shipped_goal(manifest_path: &Path) -> Option<String> {
    manifest_goal(manifest_path).ok().map(|(goal, _)| goal)
}

fn load_agent_cfg(manifest_path: &Path) -> Result<manifest::AgentCfg> {
    if manifest::is_composite(manifest_path) {
        Ok(manifest::CompositeManifest::load(manifest_path)?.agent)
    } else {
        Ok(manifest::Manifest::load(manifest_path)?.agent)
    }
}

fn manifest_dir_of(manifest_path: &Path) -> PathBuf {
    manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Build the pack's `RunIdentity` (single-domain or composite), the same freeze fingerprint a
/// run stamps at startup, computed here before any loop iteration exists.
fn compute_identity(manifest_path: &Path) -> Result<RunIdentity> {
    let manifest_dir = manifest_dir_of(manifest_path);
    if manifest::is_composite(manifest_path) {
        let m = manifest::CompositeManifest::load_frozen(manifest_path)?;
        let components = m.resolve_components(&manifest_dir)?;
        let base = m.base_dir(&manifest_dir);
        identity::for_composite(manifest_path, &base, &components, &m)
    } else {
        let m = manifest::Manifest::load_frozen(manifest_path)?;
        let workspace = manifest_dir.join(&m.workspace.dir);
        identity::for_manifest(manifest_path, &manifest_dir, &workspace, &m)
    }
}

fn render_scope_md(ctx: &ScopeCtx, identity: &RunIdentity) -> String {
    // On the PROPOSE path the raw upstream issue (`ctx.goal`) only seeded the drafting turn, the
    // SHIPPED, de-prescribed `goal.md` is what the run agent actually sees, and it's what belongs
    // here. SCOPE.md rides in the pack tar, so echoing the raw issue (which routinely prescribes its
    // own fix) would smuggle the very prescription the goal.md contract strips right back into a
    // shipped pack file. The raw issue stays in the (unshipped) scope transcript and the `Source`
    // pointer below for human audit. On the hand-written path there's no such gap: `ctx.goal` (the
    // operator's --goal/--goal-file/inline) is authoritative, so keep rendering it.
    let shipped = shipped_goal(&ctx.manifest_path);
    let goal = if ctx.propose_cost.is_some() {
        shipped.as_deref().or(ctx.goal.as_deref())
    } else {
        ctx.goal.as_deref().or(shipped.as_deref())
    }
    .unwrap_or("")
    .trim();
    let goal_source = ctx.goal_source.as_deref().unwrap_or("unknown");
    let check_section = match &ctx.check_outcome {
        Some(o) if o.ok() => "PASS".to_string(),
        Some(o) => format!("FAIL: {}", o.findings.join("; ")),
        None => "not run".to_string(),
    };
    let warnings = ctx
        .check_outcome
        .as_ref()
        .map(|o| o.warnings.join("; "))
        .filter(|s| !s.is_empty())
        .map(|w| format!("\n\n**Warnings:** {w}"))
        .unwrap_or_default();
    let timestamp = jiff::Timestamp::now()
        .strftime("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let mut components = String::new();
    for c in &identity.components {
        let name = if c.name.is_empty() {
            "<single>"
        } else {
            &c.name
        };
        components.push_str(&format!(
            "- `{name}`: repo={} base_sha={}\n",
            c.repo, c.base_sha
        ));
    }

    let refine_section = render_refine_section(&ctx.refine_rounds);

    format!(
        "# SCOPE.md — {pack}\n\n\
         Generated by `crucible scope` at {timestamp}.\n\n\
         ## Goal\n\n{goal}\n\n**Source:** {goal_source}\n\n\
         ## Validate (`crucible check`)\n\n{check_section}{warnings}\n\n\
         {refine_section}\
         ## Freeze (RunIdentity)\n\n\
         - digest: `{digest}`\n\
         - manifest_hash: `{manifest_hash}`\n\
         - inject_hash: `{inject_hash}`\n\
         - measure_cmd: `{measure_cmd}`\n\
         - direction: `{direction}`\n\
         - components:\n{components}\n\
         ## Stages not yet run\n\n\
         {propose_line}\
         - **S3 preflight** — the isolation preflight attributing the gated metric to \
         the target.\n\
         - **S4 approval** — draft-PR review + freeze-on-approve; this `SCOPE.md` is not that \
         approval, only the mechanical freeze.\n",
        pack = ctx.pack.display(),
        digest = identity.digest,
        manifest_hash = identity.manifest_hash,
        inject_hash = identity.inject_hash,
        measure_cmd = identity.measure_cmd,
        direction = identity.direction,
        propose_line = match ctx.propose_cost {
            Some(cost) => format!(
                "- **S2 propose** — ran this invocation (turn cost ${cost:.4}); see the `propose` stage above.\n"
            ),
            None => "- **S2 propose** — an agent turn drafting the manifest/gate/controls (this \
                pack was hand-written, so there was nothing to propose).\n"
                .to_string(),
        },
    )
}

/// The refine-loop section of `SCOPE.md`: a per-round
/// summary plus the machine-readable fenced-JSON trail the controller's checkpoint UI deserializes. Empty (no
/// section) for a hand-written
/// pack, which never ran the loop.
fn render_refine_section(rounds: &[RoundRecord]) -> String {
    if rounds.is_empty() {
        return String::new();
    }
    let mut lines = String::new();
    for r in rounds {
        let verdict = match &r.outcome {
            RoundOutcome::Passed => "PASS — the gate discriminates".to_string(),
            RoundOutcome::Failed { evidence } => format!("FAIL — {}", evidence.describe()),
            RoundOutcome::Error { detail } => format!("ERROR — {detail}"),
        };
        lines.push_str(&format!(
            "### Round {} ({}, turn cost ${:.4})\n\n{verdict}\n\n",
            r.round,
            r.kind.label(),
            r.cost
        ));
        if !r.judge_block.is_empty() {
            lines.push_str(&format!(
                "Proposed `[judge]` block:\n\n```toml\n{}\n```\n\n",
                r.judge_block
            ));
        }
    }
    format!(
        "## Refine loop\n\n\
         {n} round(s); the machine-readable trail (lane J4 deserializes it) follows the summaries.\n\n\
         {lines}\
         **Round trail:**\n\n{trail}\n\n",
        n = rounds.len(),
        trail = refine::render_rounds_json(rounds),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentBackend;
    use crate::event::Tokens;
    use crate::refine::{FailureEvidence, RoundKind, RoundOutcome, RoundRecord, parse_rounds};
    use crate::scope::cli::{SCOPE_REPORT_MARKER, ScopeReport, execute};
    use crate::scope::pack::{
        CONTROLS_DIR, PACK_PAYLOAD_CAP_BYTES, PACK_TAR_CAP_BYTES, SCOPE_PACK_MARKER,
        pack_marker_line, pack_marker_payload, repo_pin, tar_pack_dir,
    };
    use crate::scope::progress::{
        ACTIVITY_MIN_INTERVAL, ACTIVITY_TEXT_CAP, ACTIVITY_TOOL_CAP, PROGRESS_DOING_CAP,
        SCOPE_ACTIVITY_MARKER, SCOPE_PROGRESS_MARKER, ScopeProgress, cap_doing,
    };
    use crate::scope::transcript::{cap_transcript, gzip_transcript};
    use base64::Engine as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    /// Pull the ```json fenced block back out of a rendered `SCOPE.md`/`REJECTED.md` so a test can
    /// prove the round trail deserializes into [`RoundRecord`]s (the contract the controller checkpoint UI relies on).
    fn fenced_rounds(markdown: &str) -> Vec<RoundRecord> {
        let start = markdown.find("```json").expect("a fenced json block");
        let rest = &markdown[start..];
        let end = rest[7..].find("```").expect("a closing fence") + 7;
        parse_rounds(&rest[..end + 3]).expect("round trail parses back")
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-scope-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    fn write_exec(path: &Path, content: &str) {
        fs::write(path, content).expect("write script");
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    /// A counter-shaped pack: `[repo] path = "."`, an explicit `setup_cmd` that seeds the
    /// workspace as its own git repo (so no real remote is needed), goal inline.
    fn scaffold_pack(dir: &Path) {
        write_exec(
            &dir.join("measure.sh"),
            "#!/bin/sh\necho '{\"valid\": true, \"score\": 1.0}'\n",
        );
        let manifest = r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && cp measure.sh workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "raise the score"
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            objective = "score"
            "#;
        fs::write(dir.join(MANIFEST_FILE), manifest).expect("write manifest");
    }

    /// A tiny git repo to stand in for `--repo` (the code under test), the propose stage clones
    /// this into its scratch workspace regardless of whether the turn is real or scripted.
    fn git_repo_fixture(dir: &Path) {
        fs::write(dir.join("README.md"), "hello\n").expect("seed file");
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .expect("spawn git")
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "seed",
        ]);
    }

    /// A scripted `command`-backend "proposer": writes a counter-shaped pack into the workspace-
    /// relative pack dir (`_scope_out/` under its cwd, the `--repo` scratch checkout), the one
    /// place a sandboxed turn's writes survive (openshell round-trips only the workspace), so the
    /// double honors the same contract a real sandboxed agent is physically held to.
    /// `with_selftest` toggles the mandatory `[judge.selftest]` table, for the "rejected before
    /// validate" test.
    fn scripted_proposer(with_selftest: bool) -> String {
        let out = Path::new(PACK_WORK_DIR);
        let selftest = if with_selftest {
            "\n[judge.selftest]\n\
             good_cmd = \"echo 100 > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm good\"\n\
             bad_cmd = \"echo 10 > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm bad\"\n\
             runs = 3\n"
        } else {
            ""
        };
        format!(
            "#!/bin/sh\nset -e\nmkdir -p '{out}'\ncat > '{out}/crucible.toml' <<'MANIFEST'\n\
             [repo]\n\
             path = \".\"\n\
             [workspace]\n\
             dir = \"workspace\"\n\
             setup_cmd = \"mkdir -p workspace && cp measure.sh workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline\"\n\
             [agent]\n\
             backend = \"command\"\n\
             agent_cmd = \"true\"\n\
             goal = \"fix the thing\"\n\
             [judge]\n\
             measure_cmd = \"./measure.sh\"\n\
             direction = \"higher\"\n\
             objective = \"score\"\n\
             {selftest}\
             MANIFEST\n\
             cat > '{out}/measure.sh' <<'MEASURE'\n\
             #!/bin/sh\n\
             v=$(cat value.txt 2>/dev/null || echo 0)\n\
             echo \"{{\\\"valid\\\": true, \\\"score\\\": $v}}\"\n\
             MEASURE\n\
             chmod +x '{out}/measure.sh'\n",
            out = out.display(),
        )
    }

    /// A scripted proposer that stakes out a *structurally valid* value-domain pack with the given
    /// controls and `runs`. Unlike [`scripted_proposer`] it lets the caller pick the good/bad
    /// staged values (so the self-test can be made to discriminate or not) and the run count (so
    /// the runs>=3 floor can be exercised). Writes to the workspace-relative pack dir, same
    /// contract as [`scripted_proposer`].
    fn values_proposer(good_val: i64, bad_val: i64, runs: u32) -> String {
        let out = Path::new(PACK_WORK_DIR);
        format!(
            "#!/bin/sh\nset -e\nmkdir -p '{out}'\ncat > '{out}/crucible.toml' <<MANIFEST\n\
             [repo]\n\
             path = \".\"\n\
             [workspace]\n\
             dir = \"workspace\"\n\
             setup_cmd = \"mkdir -p workspace && cp measure.sh workspace/ && echo 0 > workspace/value.txt && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline\"\n\
             [agent]\n\
             backend = \"command\"\n\
             agent_cmd = \"true\"\n\
             goal = \"fix the thing\"\n\
             [judge]\n\
             measure_cmd = \"./measure.sh\"\n\
             direction = \"higher\"\n\
             objective = \"score\"\n\
             [judge.selftest]\n\
             good_cmd = \"echo {good_val} > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm good\"\n\
             bad_cmd = \"echo {bad_val} > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm bad\"\n\
             runs = {runs}\n\
             MANIFEST\n\
             cat > '{out}/measure.sh' <<'MEASURE'\n\
             #!/bin/sh\n\
             v=$(cat value.txt 2>/dev/null || echo 0)\n\
             echo \"{{\\\"valid\\\": true, \\\"score\\\": $v}}\"\n\
             MEASURE\n\
             chmod +x '{out}/measure.sh'\n",
            out = out.display(),
        )
    }

    /// A value-domain proposer whose self-test applies a real reference "fix" staged in the pack's
    /// private `_controls/` dir (an `apply.sh` that raises the measured value), injected into the
    /// workspace so `good_cmd` can run it, the exact shape the freeze de-prescribe strip unwinds.
    /// A second, unrelated frozen inject (the measure script) rides along so a test can prove the
    /// strip drops only the control inject, not every inject. `good` applies the fix, `bad` is a
    /// no-op on the baseline, so the gate discriminates (100 > 0 under `higher`).
    fn controls_proposer() -> String {
        let out = Path::new(PACK_WORK_DIR);
        format!(
            "#!/bin/sh\nset -e\nmkdir -p '{out}/_controls'\ncat > '{out}/crucible.toml' <<MANIFEST\n\
             [repo]\n\
             path = \".\"\n\
             [workspace]\n\
             dir = \"workspace\"\n\
             setup_cmd = \"mkdir -p workspace && cp measure.sh workspace/ && echo 0 > workspace/value.txt && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline\"\n\
             [[workspace.inject]]\n\
             src = \"measure.sh\"\n\
             dst = \"measure.sh\"\n\
             frozen = true\n\
             [[workspace.inject]]\n\
             src = \"_controls/apply.sh\"\n\
             dst = \"_controls/apply.sh\"\n\
             frozen = false\n\
             [agent]\n\
             backend = \"command\"\n\
             agent_cmd = \"true\"\n\
             goal = \"fix the thing\"\n\
             [judge]\n\
             measure_cmd = \"./measure.sh\"\n\
             direction = \"higher\"\n\
             objective = \"score\"\n\
             [judge.selftest]\n\
             good_cmd = \"sh _controls/apply.sh && git add -A && git -c user.email=t@t -c user.name=t commit -qm good\"\n\
             bad_cmd = \"true\"\n\
             runs = 3\n\
             MANIFEST\n\
             cat > '{out}/measure.sh' <<'MEASURE'\n\
             #!/bin/sh\n\
             v=$(cat value.txt 2>/dev/null || echo 0)\n\
             echo \"{{\\\"valid\\\": true, \\\"score\\\": $v}}\"\n\
             MEASURE\n\
             chmod +x '{out}/measure.sh'\n\
             cat > '{out}/_controls/apply.sh' <<'APPLY'\n\
             #!/bin/sh\n\
             echo 100 > value.txt\n\
             APPLY\n\
             chmod +x '{out}/_controls/apply.sh'\n",
            out = out.display(),
        )
    }

    /// A scripted proposer that fails its first round (good staged BELOW bad, so a `direction =
    /// higher` gate can't discriminate) and fixes itself on every round after (good above bad),
    /// keyed off a persisted round counter, the fake-agent stand-in for "diagnose the evidence and
    /// fix the gate".
    fn flipping_proposer(counter: &Path) -> String {
        // Round 1: good=10 < bad=100 -> selftest fails. Round 2+: good=100 > bad=10 -> passes.
        let first = values_proposer(10, 100, 3);
        let fixed = values_proposer(100, 10, 3);
        // Strip the `#!/bin/sh` line off the reused bodies so they nest as plain statements.
        let strip = |s: &str| s.strip_prefix("#!/bin/sh\n").unwrap_or(s).to_string();
        format!(
            "#!/bin/sh\nset -e\nc='{counter}'\nn=$(cat \"$c\" 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > \"$c\"\nif [ \"$n\" -eq 1 ]; then\n{first}\nelse\n{fixed}\nfi\n",
            counter = counter.display(),
            first = strip(&first),
            fixed = strip(&fixed),
        )
    }

    /// A scripted proposer that declines instead of drafting a pack.
    fn rejecting_proposer(reason: &str) -> String {
        format!(
            "#!/bin/sh\nset -e\nmkdir -p '{out}'\necho '{reason}' > '{out}/REJECTED.md'\n",
            out = PACK_WORK_DIR,
        )
    }

    /// Wrap a drafting proposer script with an adversary branch. The adversary turn's cwd is the
    /// pack directory itself, which, by construction, only ever gets reviewed after a round has
    /// already validated and so always has a `crucible.toml`, while every propose/refine turn's
    /// cwd is the `--repo` scratch checkout, which never does. `review_script` is the shell body
    /// run when `crucible.toml` is found in cwd; it must print the verdict JSON as its last stdout
    /// line. The one scripted double serves both roles, exactly like a real Claude turn would.
    fn with_review(draft_script: &str, review_script: &str) -> String {
        let strip = |s: &str| s.strip_prefix("#!/bin/sh\n").unwrap_or(s).to_string();
        format!(
            "#!/bin/sh\nset -e\nif [ -f crucible.toml ]; then\n{review}\nelse\n{draft}\nfi\n",
            review = review_script,
            draft = strip(draft_script),
        )
    }

    /// [`propose_opts`] with the gaming review turned back on, for the J2 tests.
    fn propose_opts_review(repo: &Path, script: &Path) -> ProposeOpts {
        let mut opts = propose_opts(repo, script);
        opts.skip_gaming_review = false;
        opts
    }

    /// The shared J1 test double: `skip_gaming_review` is on because these tests exercise the
    /// propose/refine loop itself, not J2 (the gaming review's own tests reuse this and flip it).
    fn propose_opts(repo: &Path, script: &Path) -> ProposeOpts {
        ProposeOpts {
            repo: repo.display().to_string(),
            max_cost: 5.0,
            agent_cmd_override: Some(script.display().to_string()),
            force: false,
            refine_rounds: 3,
            skip_gaming_review: true,
            gaming_refine_rounds: 1,
            tier: ProposeTier::T0,
            agent_backend: AgentBackend::Local,
            sandbox_image: None,
            progress: false,
            compute_driver: crate::openshell::gateway::ComputeDriver::Podman,
            authoritative: false,
        }
    }

    /// `--agent-backend openshell --sandbox-image … --compute-driver kubernetes` reaches the turn
    /// args both scope turns run on, the exact plumbing the WorkPod wrapper depends on (it was
    /// silently `Local`/`Podman` before). A `Cli::parse_from(["crucible"])`-synthesized `Args`
    /// defaults every unset field, so `compute_driver` must be set explicitly or the turn's
    /// gateway boots the wrong compute driver regardless of the caller's deployment.
    #[test]
    fn turn_args_thread_the_backend_sandbox_image_and_compute_driver() {
        let mut opts = propose_opts(Path::new("unused"), Path::new("unused"));
        opts.agent_cmd_override = None;
        opts.agent_backend = AgentBackend::Openshell;
        opts.sandbox_image = Some("ghcr.io/neuralmagic/crucible-sandbox:latest".to_string());
        opts.compute_driver = crate::openshell::gateway::ComputeDriver::Kubernetes;
        let args = turn_args(&opts);
        assert_eq!(args.agent_backend, AgentBackend::Openshell);
        assert_eq!(
            args.sandbox_image.as_deref(),
            Some("ghcr.io/neuralmagic/crucible-sandbox:latest")
        );
        assert_eq!(
            args.compute_driver,
            crate::openshell::gateway::ComputeDriver::Kubernetes
        );
    }

    /// The scripted override still wins over the backend flag (rank-grounded's precedence).
    #[test]
    fn turn_args_let_the_agent_cmd_override_win() {
        let mut opts = propose_opts(Path::new("unused"), Path::new("/tmp/fake.sh"));
        opts.agent_backend = AgentBackend::Openshell;
        let args = turn_args(&opts);
        assert_eq!(args.agent_backend, AgentBackend::Command);
        assert_eq!(args.agent_cmd.as_deref(), Some("/tmp/fake.sh"));
    }

    #[test]
    fn propose_with_a_valid_pack_chains_through_validate_and_freeze() {
        let repo = tempdir("propose-repo");
        git_repo_fixture(&repo);
        let out = tempdir("propose-out");
        let goal_dir = tempdir("propose-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &scripted_proposer(true));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );
        assert_eq!(report.stages.len(), 4, "ingest, propose, validate, freeze");
        assert_eq!(report.stages[1].name, "propose");
        assert!(report.cost.is_some_and(|c| c >= 0.0));
        assert!(
            report
                .digest
                .as_deref()
                .is_some_and(|d| d.starts_with("v1:"))
        );

        let scope_md = fs::read_to_string(out.join("SCOPE.md")).expect("SCOPE.md written");
        assert!(scope_md.contains("S2 propose"));
        assert!(scope_md.contains("ran this invocation"));

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn propose_canonicalizes_a_relative_out_path() {
        // The host-side pipeline (stage details, REJECTED.md, validate/freeze) must name a
        // stable absolute --out path even when the caller passed a relative one. The agent never
        // sees this path, the prompts spell the pack workspace-relative.
        let repo = tempdir("propose-repo-rel");
        git_repo_fixture(&repo);
        let goal_dir = tempdir("propose-goal-rel");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, "#!/bin/sh\ntrue\n");

        let rel = PathBuf::from(format!("scope-relout-{}", std::process::id()));
        let report = execute(
            &rel,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        let propose = &report.stages[1];
        assert!(!propose.passed, "no-op proposer must fail the stage");
        let abs = fs::canonicalize(&rel).expect("out dir was created");
        assert!(
            propose.detail.contains(&abs.display().to_string()),
            "the stage must operate on the canonicalized out path, got: {}",
            propose.detail
        );
        let _ = fs::remove_dir_all(&rel);
        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn propose_without_selftest_fails_before_validate_runs() {
        let repo = tempdir("propose-repo-nost");
        git_repo_fixture(&repo);
        let out = tempdir("propose-out-nost");
        let goal_dir = tempdir("propose-goal-nost");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &scripted_proposer(false));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert_eq!(
            report.stages.len(),
            2,
            "propose fails, validate never runs: {:?}",
            report.stages
        );
        assert!(report.stages[0].passed, "ingest still resolves the goal");
        assert!(
            !report.stages[1].passed,
            "propose must reject a missing selftest"
        );
        assert_eq!(report.stages[1].name, "propose");
        assert!(report.stages[1].detail.contains("judge.selftest"));
        assert!(report.digest.is_none());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn propose_rejected_md_is_a_clean_stage_failure() {
        let repo = tempdir("propose-repo-rej");
        git_repo_fixture(&repo);
        let out = tempdir("propose-out-rej");
        let goal_dir = tempdir("propose-goal-rej");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(
            &script,
            &rejecting_proposer("needs a live rig, not a test gate"),
        );

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert_eq!(report.stages.len(), 2);
        assert!(!report.stages[1].passed);
        assert!(
            report.stages[1]
                .detail
                .contains("needs a live rig, not a test gate")
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn propose_refuses_a_nonempty_out_without_force() {
        let repo = tempdir("propose-repo-dirty");
        git_repo_fixture(&repo);
        let out = tempdir("propose-out-dirty");
        fs::write(out.join("stray.txt"), "leftover").expect("seed a stray file");
        let goal_dir = tempdir("propose-goal-dirty");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &scripted_proposer(true));

        let mut opts = propose_opts(&repo, &script);
        opts.force = false;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert_eq!(report.stages.len(), 2);
        assert!(!report.stages[1].passed);
        assert!(report.stages[1].detail.contains("--force"));
        assert!(
            out.join("stray.txt").exists(),
            "refused out dir is left untouched"
        );

        let mut opts = propose_opts(&repo, &script);
        opts.force = true;
        let forced = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            forced.stages.iter().all(|s| s.passed),
            "--force permits proposing into a nonempty --out: {:?}",
            forced.stages
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn propose_json_shape_includes_cost() {
        let repo = tempdir("propose-repo-json");
        git_repo_fixture(&repo);
        let out = tempdir("propose-out-json");
        let goal_dir = tempdir("propose-goal-json");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &scripted_proposer(true));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        let json = serde_json::to_value(&report).expect("serializes");
        assert!(json["cost"].is_number(), "{json:?}");
        assert!(json["stages"].as_array().unwrap().len() >= 2);

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    fn broken_pack(dir: &Path) {
        let manifest = r#"
            [repo]
            path = "."
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "raise the score"
            [judge]
            measure_cmd = "./does-not-exist.sh"
            direction = "higher"
            "#;
        fs::write(dir.join(MANIFEST_FILE), manifest).expect("write manifest");
    }

    #[test]
    fn full_pipeline_passes_on_a_valid_pack() {
        let dir = tempdir("valid");
        scaffold_pack(&dir);

        let report = execute(&dir, None, None, false, None);
        assert!(
            report.stages.iter().all(|s| s.passed),
            "all stages must pass: {:?}",
            report.stages
        );
        assert_eq!(report.stages.len(), 3);
        assert!(
            report
                .digest
                .as_deref()
                .is_some_and(|d| d.starts_with("v1:"))
        );

        let scope_md = fs::read_to_string(dir.join("SCOPE.md")).expect("SCOPE.md written");
        assert!(scope_md.contains("raise the score"));
        assert!(scope_md.contains("pack manifest [agent].goal"));
        assert!(scope_md.contains("digest: `v1:"));
        assert!(scope_md.contains("S2 propose"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_authored_scope_materializes_workflow_source_before_freeze() {
        let dir = tempdir("hand-workflow");
        scaffold_pack(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(dir.join("prompts/review.md"), "Review the candidate.\n").unwrap();
        fs::write(
            dir.join("workflow.star"),
            "workflow([agent(name = \"review\", prompt = prompt_file(\"prompts/review.md\"), required = False)])\n",
        )
        .unwrap();

        let report = execute(&dir, None, None, false, None);
        assert!(
            report.stages.iter().all(|stage| stage.passed),
            "{:?}",
            report.stages
        );
        let manifest = fs::read_to_string(dir.join(MANIFEST_FILE)).unwrap();
        assert!(manifest.contains("[[workflow.task]]"), "{manifest}");
        assert!(manifest.contains("Review the candidate."), "{manifest}");
        assert!(report.digest.is_some(), "compiled manifest must freeze");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_2_failure_stops_the_pipeline_before_freeze() {
        let dir = tempdir("broken");
        broken_pack(&dir);

        let report = execute(&dir, None, None, false, None);
        assert_eq!(
            report.stages.len(),
            2,
            "stops after validate, freeze never runs"
        );
        assert!(report.stages[0].passed, "ingest still resolves the goal");
        assert!(!report.stages[1].passed, "validate must fail");
        assert_eq!(report.stages[1].name, "validate");
        assert!(report.stages[1].detail.contains("crucible check"));
        assert!(report.digest.is_none());
        assert!(
            !dir.join("SCOPE.md").exists(),
            "no SCOPE.md on a failed validate stage"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn workflow_compile_failure_is_scope_structure_evidence() {
        let dir = tempdir("workflow-structure");
        scaffold_pack(&dir);
        fs::write(
            dir.join("workflow.star"),
            "workflow([agent(name = 'review', prompt = prompt_file('../escape.md'))])\n",
        )
        .unwrap();

        let RoundVerdict::Failed(FailureEvidence::Structure { detail }) =
            compile_and_validate_round(&dir.join(MANIFEST_FILE))
        else {
            panic!("bad workflow source must be structure evidence")
        };
        assert!(detail.contains("workflow.star did not compile"), "{detail}");
        assert!(detail.contains("may not contain `..`"), "{detail}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn goal_file_beats_manifest_goal() {
        let dir = tempdir("goal-file-precedence");
        scaffold_pack(&dir);
        let goal_file = dir.join("goal.md");
        fs::write(&goal_file, "the goal-file goal").expect("write goal file");

        let report = execute(&dir, None, Some(&goal_file), false, None);
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );
        let scope_md = fs::read_to_string(dir.join("SCOPE.md")).expect("SCOPE.md written");
        assert!(scope_md.contains("the goal-file goal"));
        assert!(!scope_md.contains("raise the score"));
        assert!(scope_md.contains("--goal-file"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn issue_source_fetches_the_github_api() {
        use std::io::{Read as _, Write as _};

        // Share the crate-wide env guard: `GITHUB_API_URL` is a process global, and other modules'
        // tests (`run`, `rank_grounded`) point it at their own listeners concurrently.
        let _env = crate::test_env_lock();
        let dir = tempdir("issue-source");
        scaffold_pack(&dir);

        // A real listener standing in for api.github.com (pointed at via GITHUB_API_URL):
        // serve one canned issue, hand the raw request back for assertion.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let mut read = 0;
            while !buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf[read..]).expect("read request");
                if n == 0 {
                    break;
                }
                read += n;
            }
            let body = r#"{"title": "fake issue owner/repo #42", "body": "reproduce and fix"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).expect("write response");
            String::from_utf8_lossy(&buf[..read]).into_owned()
        });

        unsafe {
            std::env::set_var("GITHUB_API_URL", format!("http://{addr}"));
        }
        let report = execute(&dir, Some("owner/repo#42"), None, false, None);
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        let request = server.join().expect("server thread");
        assert!(
            request.starts_with("GET /repos/owner/repo/issues/42 "),
            "unexpected request: {request}"
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );
        let scope_md = fs::read_to_string(dir.join("SCOPE.md")).expect("SCOPE.md written");
        assert!(scope_md.contains("fake issue owner/repo #42"));
        assert!(scope_md.contains("reproduce and fix"));
        assert!(scope_md.contains("--issue owner/repo#42"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope_md_overwrite_is_refused_without_force() {
        let dir = tempdir("overwrite");
        scaffold_pack(&dir);
        let first_report = execute(&dir, None, None, false, None);
        assert!(first_report.stages.iter().all(|s| s.passed));
        let first = fs::read_to_string(dir.join("SCOPE.md")).unwrap();

        let refused = execute(&dir, None, None, false, None);
        assert!(
            !refused.stages.last().unwrap().passed,
            "refuses without --force"
        );
        assert!(refused.stages.last().unwrap().detail.contains("--force"));
        let unchanged = fs::read_to_string(dir.join("SCOPE.md")).unwrap();
        assert_eq!(first, unchanged, "SCOPE.md untouched without --force");

        let forced = execute(&dir, None, None, true, None);
        assert!(forced.stages.iter().all(|s| s.passed), "--force overwrites");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn refine_loop_recovers_after_a_bad_first_round() {
        let repo = tempdir("refine-repo");
        git_repo_fixture(&repo);
        let out = tempdir("refine-out");
        let goal_dir = tempdir("refine-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let counter = goal_dir.join("round.count");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &flipping_proposer(&counter));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "the loop must recover and freeze: {:?}",
            report.stages
        );
        assert_eq!(report.rounds.len(), 2, "round 1 fails, round 2 fixes");
        assert_eq!(report.rounds[0].kind, RoundKind::Propose);
        assert!(
            matches!(
                report.rounds[0].outcome,
                RoundOutcome::Failed {
                    evidence: FailureEvidence::Selftest(_)
                }
            ),
            "round 1 fails on a non-discriminating self-test: {:?}",
            report.rounds[0].outcome
        );
        assert_eq!(report.rounds[1].kind, RoundKind::Refine);
        assert!(matches!(report.rounds[1].outcome, RoundOutcome::Passed));

        let scope_md = fs::read_to_string(out.join("SCOPE.md")).expect("SCOPE.md written");
        assert!(scope_md.contains("Refine loop"), "{scope_md}");
        assert!(scope_md.contains("Round 1") && scope_md.contains("Round 2"));
        let parsed = fenced_rounds(&scope_md);
        assert_eq!(parsed.len(), 2, "trail round-trips through serde");
        assert!(matches!(parsed[1].outcome, RoundOutcome::Passed));
        assert!(
            !out.join("REJECTED.md").exists(),
            "a recovered loop leaves no rejection"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn refine_loop_exhausts_to_rejected_with_full_trail() {
        let repo = tempdir("exhaust-repo");
        git_repo_fixture(&repo);
        let out = tempdir("exhaust-out");
        let goal_dir = tempdir("exhaust-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // good == bad every round: the gate never discriminates, so the loop must exhaust.
        write_exec(&script, &values_proposer(10, 10, 3));

        let mut opts = propose_opts(&repo, &script);
        opts.refine_rounds = 2;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            !report.stages.last().expect("a last stage").passed,
            "exhaustion fails the propose stage: {:?}",
            report.stages
        );
        assert_eq!(
            report.stages.len(),
            2,
            "stops at propose, no validate/freeze"
        );
        assert_eq!(report.rounds.len(), 2, "both rounds recorded");
        assert!(
            report
                .rounds
                .iter()
                .all(|r| matches!(r.outcome, RoundOutcome::Failed { .. })),
            "every round failed"
        );
        assert!(report.digest.is_none(), "nothing frozen");

        let rejected = fs::read_to_string(out.join("REJECTED.md")).expect("REJECTED.md written");
        assert!(rejected.contains("Refine trail"), "{rejected}");
        assert!(rejected.contains("NOT frozen"));
        let parsed = fenced_rounds(&rejected);
        assert_eq!(parsed.len(), 2, "the full trail is in REJECTED.md");
        assert!(!out.join("SCOPE.md").exists(), "no SCOPE.md on rejection");

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// The regression behind the $12.68 turn on llm-d/llm-d-router#60: on the openshell backend
    /// only `paths.workspace` round-trips the sandbox, so a pack written to the host-absolute
    /// `--out` path evaporates with the sandbox. The pipeline must therefore pick the pack up
    /// exclusively from the workspace-relative pack dir, a proposer that writes straight to the
    /// host out dir (something a sandboxed agent physically cannot do) produces no pack.
    #[test]
    fn pack_written_outside_the_workspace_is_not_picked_up() {
        let repo = tempdir("outside-repo");
        git_repo_fixture(&repo);
        let out = tempdir("outside-out");
        let goal_dir = tempdir("outside-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // The pre-fix contract: write a (complete, would-be-valid) pack to the absolute out dir.
        write_exec(
            &script,
            &format!(
                "#!/bin/sh\nset -e\nprintf 'not a pack' > '{}/crucible.toml'\n",
                out.display()
            ),
        );

        let mut opts = propose_opts(&repo, &script);
        opts.refine_rounds = 1;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            !report.stages[1].passed,
            "a pack outside the workspace must not validate: {:?}",
            report.stages
        );
        assert!(
            report.stages[1]
                .detail
                .contains("no crucible.toml was written"),
            "the failure is the missing manifest, not a parse error on the stray file: {}",
            report.stages[1].detail
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// The budget stop (the same live turn spent $12.68 against --max-cost 5 because the budget
    /// was only reported at the end): once the cumulative turn cost meets --max-cost, the loop
    /// must stop between rounds with an honest budget failure instead of starting another one.
    #[test]
    fn refine_loop_stops_between_rounds_when_the_budget_is_exhausted() {
        let repo = tempdir("budget-repo");
        git_repo_fixture(&repo);
        let out = tempdir("budget-out");
        let goal_dir = tempdir("budget-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // Each round reports a real $3 turn cost (a claude-style result event on stdout) and
        // drafts a gate that never discriminates (good == bad), so only the budget can stop it.
        let result_line = r#"{"v":1,"kind":"result","subtype":"success","stop_reason":"end_turn","duration_ms":1,"api_duration_ms":1,"ttft_ms":1,"turns":1,"cost_usd":3.0}"#;
        let body = values_proposer(10, 10, 3);
        let body = body.strip_prefix("#!/bin/sh\n").unwrap_or(&body);
        write_exec(
            &script,
            &format!("#!/bin/sh\nset -e\necho '{result_line}'\n{body}"),
        );

        let mut opts = propose_opts(&repo, &script);
        opts.max_cost = 5.0;
        opts.refine_rounds = 5;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            !report.stages.last().expect("a last stage").passed,
            "budget exhaustion fails the propose stage: {:?}",
            report.stages
        );
        assert_eq!(
            report.rounds.len(),
            2,
            "round 1 = $3 < $5, round 2 = $6 >= $5, round 3 never starts: {:?}",
            report.rounds
        );
        assert!(
            report.stages[1].detail.contains("budget exhausted"),
            "the failure is honest about why: {}",
            report.stages[1].detail
        );
        let total = report.cost.expect("cost recorded");
        assert!((total - 6.0).abs() < 1e-9, "two $3 rounds, got ${total}");
        assert!(
            out.join("REJECTED.md").exists(),
            "the trail lands in REJECTED.md for the door UI"
        );
        assert!(!out.join("SCOPE.md").exists(), "nothing frozen");

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn refine_loop_rejects_a_proposed_runs_below_three() {
        let repo = tempdir("runs-repo");
        git_repo_fixture(&repo);
        let out = tempdir("runs-out");
        let goal_dir = tempdir("runs-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // A perfectly discriminating gate (100 vs 10) but runs=1, below the proposed-pack floor.
        write_exec(&script, &values_proposer(100, 10, 1));

        let mut opts = propose_opts(&repo, &script);
        opts.refine_rounds = 2;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            !report.stages.last().expect("a last stage").passed,
            "runs<3 must reject: {:?}",
            report.stages
        );
        assert_eq!(report.rounds.len(), 2);
        match &report.rounds[0].outcome {
            RoundOutcome::Failed {
                evidence: FailureEvidence::Structure { detail },
            } => {
                assert!(
                    detail.contains("runs"),
                    "evidence names the floor: {detail}"
                );
                assert!(
                    detail.contains('3'),
                    "evidence names the floor value: {detail}"
                );
            }
            other => panic!("expected a Structure(runs) failure, got {other:?}"),
        }
        assert!(report.digest.is_none());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn json_shape_has_stages_and_digest() {
        let dir = tempdir("json");
        scaffold_pack(&dir);

        let report = execute(&dir, None, None, false, None);
        let json = serde_json::to_value(&report).expect("serializes");
        assert!(json["stages"].is_array());
        assert_eq!(json["stages"].as_array().unwrap().len(), 3);
        for stage in json["stages"].as_array().unwrap() {
            assert!(stage["name"].is_string());
            assert!(stage["passed"].is_boolean());
            assert!(stage["detail"].is_string());
        }
        assert!(json["digest"].as_str().unwrap().starts_with("v1:"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scope_report_marker_emits_the_exact_prefix() {
        let report = ScopeReport {
            stages: vec![StageResult {
                name: "ingest",
                passed: true,
                detail: "ok".to_string(),
            }],
            digest: Some("v1:abc".to_string()),
            cost: Some(1.23),
            rounds: Vec::new(),
            transcript: String::new(),
        };
        let compact = serde_json::to_string(&report).expect("serializes");
        let marker_line = format!("{SCOPE_REPORT_MARKER} {compact}");
        assert!(
            marker_line.starts_with("CRUCIBLE_SCOPE_REPORT:"),
            "marker prefix matches the controller's SCOPE_REPORT_MARKER: {marker_line}"
        );
        let payload = marker_line
            .strip_prefix(SCOPE_REPORT_MARKER)
            .expect("prefix")
            .trim();
        let parsed: serde_json::Value = serde_json::from_str(payload).expect("valid JSON");
        assert!(parsed["digest"].as_str().unwrap().starts_with("v1:"));
        assert!((parsed["cost"].as_f64().unwrap() - 1.23).abs() < 1e-9);
    }

    #[test]
    fn scope_progress_marker_line_round_trips() {
        let beat = ScopeProgress {
            round: 2,
            kind: RoundKind::Refine,
            doing: "refining the pack on round 1's failure: selftest failed".to_string(),
            cost_so_far: 0.42,
        };
        let line = beat.marker_line();
        assert!(
            line.starts_with("CRUCIBLE_SCOPE_PROGRESS: "),
            "marker prefix matches the controller's literal: {line}"
        );
        let payload = line
            .strip_prefix(SCOPE_PROGRESS_MARKER)
            .expect("prefix")
            .trim();
        let parsed: serde_json::Value = serde_json::from_str(payload).expect("valid JSON");
        assert_eq!(parsed["round"].as_u64(), Some(2));
        assert_eq!(
            parsed["kind"].as_str(),
            Some("refine"),
            "RoundKind snake_case spelling"
        );
        assert!(parsed["doing"].as_str().unwrap().starts_with("refining"));
        assert!((parsed["cost_so_far"].as_f64().unwrap() - 0.42).abs() < 1e-9);
    }

    #[test]
    fn progress_doing_is_capped_with_an_ellipsis() {
        let short = "refining";
        assert_eq!(cap_doing(short), short);
        let long = "x".repeat(PROGRESS_DOING_CAP + 50);
        let capped = cap_doing(&long);
        assert_eq!(capped.chars().count(), PROGRESS_DOING_CAP + 1);
        assert!(capped.ends_with('…'));
    }

    /// A parsed activity payload's fields, for the feed tests below.
    fn activity_json(line: &str) -> serde_json::Value {
        let payload = line
            .strip_prefix(SCOPE_ACTIVITY_MARKER)
            .expect("marker prefix")
            .trim();
        serde_json::from_str(payload).expect("activity payload is JSON")
    }

    fn tool_event(name: &str, summary: &str) -> AgentEvent {
        AgentEvent::Tool {
            name: name.to_string(),
            summary: summary.to_string(),
            subagent: false,
        }
    }

    #[test]
    fn activity_feed_is_silent_without_the_marker_flag() {
        let mut feed = ActivityFeed::new(false);
        let now = std::time::Instant::now();
        assert!(
            feed.line_for("claude-opus-4-6", &tool_event("Edit", "x"), now)
                .is_none()
        );
        assert!(
            feed.line_for(
                "claude-opus-4-6",
                &AgentEvent::Text { delta: "hi".into() },
                now
            )
            .is_none()
        );
    }

    #[test]
    fn activity_tool_lines_cap_the_summary_and_carry_the_running_cost() {
        let mut feed = ActivityFeed::new(true);
        feed.begin_turn(0.5);
        let now = std::time::Instant::now();
        // An authoritative in-turn cost sample rides subsequent lines on top of the base.
        let tokens = Tokens {
            cost_usd: Some(0.25),
            ..Tokens::default()
        };
        let _ = feed.line_for("claude-opus-4-6", &AgentEvent::Tokens(tokens), now);
        let long = "y".repeat(ACTIVITY_TOOL_CAP + 40);
        let line = feed
            .line_for("claude-opus-4-6", &tool_event("Bash", &long), now)
            .expect("tool events always emit");
        let v = activity_json(&line);
        assert_eq!(v["kind"], "tool");
        assert_eq!(v["name"], "Bash");
        let detail = v["detail"].as_str().expect("detail string");
        assert_eq!(detail.chars().count(), ACTIVITY_TOOL_CAP + 1);
        assert!(detail.ends_with('…'));
        assert!((v["cost_so_far"].as_f64().expect("cost") - 0.75).abs() < 1e-9);
    }

    #[test]
    fn activity_text_and_usage_share_one_rate_limit() {
        let mut feed = ActivityFeed::new(true);
        let t0 = std::time::Instant::now();
        let text = AgentEvent::Text {
            delta: "working on the manifest".into(),
        };
        assert!(feed.line_for("m", &text, t0).is_some(), "first text emits");
        assert!(
            feed.line_for("m", &text, t0 + std::time::Duration::from_secs(1))
                .is_none(),
            "a second within the interval is suppressed"
        );
        assert!(
            feed.line_for(
                "m",
                &AgentEvent::Tokens(Tokens::default()),
                t0 + std::time::Duration::from_secs(2)
            )
            .is_none(),
            "usage shares the same limiter"
        );
        let later = t0 + ACTIVITY_MIN_INTERVAL + std::time::Duration::from_secs(1);
        assert!(
            feed.line_for("m", &text, later).is_some(),
            "the interval elapsing reopens the lane"
        );
        // Tool lines never wait on the text limiter.
        assert!(
            feed.line_for("m", &tool_event("Read", "crucible.toml"), later)
                .is_some()
        );
    }

    #[test]
    fn activity_text_caps_the_snippet_and_skips_whitespace() {
        let mut feed = ActivityFeed::new(true);
        let now = std::time::Instant::now();
        assert!(
            feed.line_for(
                "m",
                &AgentEvent::Text {
                    delta: "  \n".into()
                },
                now
            )
            .is_none(),
            "whitespace-only text is not a beat"
        );
        let long = "z".repeat(ACTIVITY_TEXT_CAP + 30);
        let line = feed
            .line_for("m", &AgentEvent::Text { delta: long }, now)
            .expect("text emits");
        let v = activity_json(&line);
        assert_eq!(v["kind"], "text");
        let detail = v["detail"].as_str().expect("detail string");
        assert_eq!(detail.chars().count(), ACTIVITY_TEXT_CAP + 1);
    }

    #[test]
    fn activity_usage_estimates_cost_from_tokens_when_none_rides_the_event() {
        let mut feed = ActivityFeed::new(true);
        let tokens = Tokens {
            output: 1000,
            total: 1000,
            ..Tokens::default()
        };
        let line = feed
            .line_for(
                "claude-haiku-4-5",
                &AgentEvent::Tokens(tokens),
                std::time::Instant::now(),
            )
            .expect("usage emits");
        let v = activity_json(&line);
        assert_eq!(v["kind"], "usage");
        assert_eq!(v["detail"], "1000 tokens");
        // Haiku output = $5/MTok -> 1000 tokens = $0.005.
        assert!((v["cost_so_far"].as_f64().expect("cost") - 0.005).abs() < 1e-9);
    }

    #[test]
    fn activity_relays_openshell_stage_banners_but_not_other_logs() {
        let mut feed = ActivityFeed::new(true);
        let now = std::time::Instant::now();
        let stage = AgentEvent::Log {
            level: "stage".into(),
            label: "openshell".into(),
            value: Some("creating sandbox (pulling the image)".into()),
        };
        let line = feed.line_for("m", &stage, now).expect("stage emits");
        let v = activity_json(&line);
        assert_eq!(v["kind"], "stage");
        assert_eq!(v["detail"], "creating sandbox (pulling the image)");
        let other = AgentEvent::Log {
            level: "detail".into(),
            label: "Cost".into(),
            value: Some("$0.10".into()),
        };
        assert!(feed.line_for("m", &other, now).is_none());
        assert!(
            feed.line_for("m", &AgentEvent::Thinking { delta: "x".into() }, now)
                .is_none()
        );
    }

    #[test]
    fn activity_budget_exhaustion_emits_one_truncation_line_then_silence() {
        let mut feed = ActivityFeed::new(true);
        feed.bytes_left = 120; // room for roughly one line
        let now = std::time::Instant::now();
        let mut lines = Vec::new();
        for i in 0..10 {
            if let Some(line) =
                feed.line_for("m", &tool_event("Read", &format!("file-{i}.rs")), now)
            {
                lines.push(line);
            }
        }
        let last = lines.last().expect("at least the truncation line");
        assert_eq!(activity_json(last)["kind"], "truncated");
        assert_eq!(
            lines
                .iter()
                .filter(|l| activity_json(l)["kind"] == "truncated")
                .count(),
            1,
            "exactly one truncation line"
        );
        assert!(lines.len() < 10, "the budget stopped the feed: {lines:?}");
        // And the feed stays quiet afterwards, even for tool events.
        assert!(feed.line_for("m", &tool_event("Edit", "x"), now).is_none());
    }

    #[test]
    fn cap_transcript_leaves_a_small_transcript_alone() {
        let ndjson = "{\"kind\":\"note\",\"msg\":\"round 1: propose turn\"}\n";
        let (capped, dropped) = cap_transcript(ndjson, 1024);
        assert_eq!(capped, ndjson);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn cap_transcript_keeps_head_and_tail_with_an_honest_note() {
        // 100 lines of ~30 bytes; cap at ~10 lines' worth. Head keeps the start, tail keeps the
        // end, and the note between them says how much went missing.
        let lines: Vec<String> = (0..100)
            .map(|i| format!("{{\"kind\":\"note\",\"msg\":\"line {i:03}\"}}"))
            .collect();
        let ndjson = lines.join("\n") + "\n";
        let cap = 400;
        let (capped, dropped) = cap_transcript(&ndjson, cap);
        assert!(dropped > 0);
        assert!(capped.contains("line 000"), "head preserved");
        assert!(capped.contains("line 099"), "tail preserved");
        assert!(
            capped.contains("transcript truncated"),
            "honest truncation note present: {capped}"
        );
        // Every line (including the note) still parses, whole lines only, no torn JSON.
        for line in capped.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("intact NDJSON line");
        }
    }

    #[test]
    fn gzip_transcript_roundtrips() {
        use std::io::Read as _;
        let ndjson = "{\"kind\":\"note\",\"msg\":\"round 1: propose turn\"}\n";
        let gz = gzip_transcript(ndjson).expect("gzips");
        let mut back = String::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_string(&mut back)
            .expect("gunzips");
        assert_eq!(back, ndjson);
    }

    /// The pack marker payload round-trips the whole pack tree: base64 → gunzip → untar yields the
    /// exact files (nested dirs included) the scope froze. This is the controller's recovery path.
    #[test]
    fn pack_marker_payload_roundtrips_the_pack_tree() {
        use std::io::Read as _;
        let pack = tempdir("pack-blob");
        fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        fs::write(pack.join("SCOPE.md"), "identity: v1:beef\n").unwrap();
        fs::create_dir_all(pack.join("prompts")).unwrap();
        fs::write(pack.join("prompts/goal.md"), "fix the thing\n").unwrap();

        let payload = pack_marker_payload(&pack).expect("payload builds");
        let gz = base64::engine::general_purpose::STANDARD
            .decode(&payload)
            .expect("base64 decodes");
        let mut tar_bytes = Vec::new();
        flate2::read::GzDecoder::new(gz.as_slice())
            .read_to_end(&mut tar_bytes)
            .expect("gunzips");

        let dest = tempdir("pack-blob-out");
        tar::Archive::new(tar_bytes.as_slice())
            .unpack(&dest)
            .expect("untars");
        for rel in ["crucible.toml", "SCOPE.md", "prompts/goal.md"] {
            assert_eq!(
                fs::read(dest.join(rel)).unwrap_or_else(|_| panic!("{rel} restored")),
                fs::read(pack.join(rel)).unwrap(),
                "{rel} round-trips byte-identical"
            );
        }

        let _ = fs::remove_dir_all(&pack);
        let _ = fs::remove_dir_all(&dest);
    }

    /// A `.git` subtree in the pack (a checkout the propose agent left behind) is excluded from
    /// the tar: the run phase re-clones from the manifest, and git packfiles are incompressible,
    /// they blew the encoded budget on the first live checkpoint build.
    #[test]
    fn tar_pack_dir_excludes_git_subtrees() {
        let pack = tempdir("pack-gitless");
        fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        fs::create_dir_all(pack.join("workspace/.git/objects/pack")).unwrap();
        // Incompressible "packfile", with .git included this alone would overflow the payload cap.
        let mut buf = vec![0u8; 8 * 1024 * 1024];
        let mut x: u64 = 7;
        for b in &mut buf {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 33) as u8;
        }
        fs::write(
            pack.join("workspace/.git/objects/pack/pack-cafe.pack"),
            &buf,
        )
        .unwrap();
        fs::write(pack.join("workspace/main.go"), "package main\n").unwrap();

        let tar = tar_pack_dir(&pack).expect("a gitless tar emits");
        assert!(
            tar.len() < 1024 * 1024,
            "the .git payload is gone: {} bytes",
            tar.len()
        );
        let names: Vec<String> = tar::Archive::new(&tar[..])
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.ends_with("workspace/main.go")),
            "workspace files survive: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains(".git")),
            "no .git entries: {names:?}"
        );
        let _ = fs::remove_dir_all(&pack);
    }

    /// The cap binds the ENCODED payload, not the raw tar: a text-heavy pack well over the old
    /// 4 MiB tar bound (the live 12.6 MiB failure) compresses to a small line and must emit.
    #[test]
    fn pack_marker_payload_admits_a_large_compressible_pack() {
        let pack = tempdir("pack-compressible");
        fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        // 12 MiB of repetitive text, tars over the old 4 MiB cap, gzips to almost nothing.
        fs::write(
            pack.join("workspace.go"),
            "// prefix cache scorer\n".repeat(12 * 1024 * 1024 / 23),
        )
        .unwrap();
        let payload = pack_marker_payload(&pack).expect("a compressible pack emits");
        assert!(
            payload.len() <= PACK_PAYLOAD_CAP_BYTES,
            "the emitted payload fits the log budget: {} bytes",
            payload.len()
        );
        let _ = fs::remove_dir_all(&pack);
    }

    /// An incompressible pack whose ENCODED payload overflows the log budget refuses to emit, even
    /// though its raw tar passes the sanity cap.
    #[test]
    fn pack_marker_payload_refuses_an_incompressible_overflow() {
        let pack = tempdir("pack-incompressible");
        fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        // ~6 MiB of LCG noise: under the 64 MiB tar sanity cap, over 6 MiB once gzip fails to
        // shrink it and base64 adds 4/3.
        let mut buf = vec![0u8; 6 * 1024 * 1024];
        let mut x: u64 = 0x9e3779b97f4a7c15;
        for b in &mut buf {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *b = (x >> 33) as u8;
        }
        fs::write(pack.join("bloat.bin"), &buf).unwrap();
        let err = pack_marker_payload(&pack).expect_err("an oversize payload must refuse");
        assert!(
            format!("{err:#}").contains("cap"),
            "the error names the cap: {err:#}"
        );
        let _ = fs::remove_dir_all(&pack);
    }

    /// An oversize pack refuses to emit, an explicit error, never a truncated blob the controller
    /// could mistake for the real pack.
    #[test]
    fn tar_pack_dir_refuses_an_oversize_pack() {
        let pack = tempdir("pack-oversize");
        fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();
        fs::write(pack.join("bloat.bin"), vec![0u8; PACK_TAR_CAP_BYTES + 1]).unwrap();
        let err = tar_pack_dir(&pack).expect_err("over the cap must refuse");
        assert!(
            format!("{err:#}").contains("cap"),
            "the error names the cap: {err:#}"
        );
        let _ = fs::remove_dir_all(&pack);
    }

    fn stage(name: &'static str, passed: bool) -> StageResult {
        StageResult {
            name,
            passed,
            detail: String::new(),
        }
    }

    /// Only a survival emits the pack marker (base64 payload); a dead proposal emits nothing; an
    /// oversize survival emits the honest `{"error":…}` payload instead of a truncated blob.
    #[test]
    fn pack_marker_line_fires_only_on_survival_and_is_honest_about_oversize() {
        let pack = tempdir("pack-marker");
        fs::write(pack.join("crucible.toml"), "[repo]\nurl = \"x\"\n").unwrap();

        let survived = ScopeReport {
            stages: vec![stage("validate", true), stage("freeze", true)],
            digest: Some("v1:beef".into()),
            cost: Some(0.1),
            rounds: Vec::new(),
            transcript: String::new(),
        };
        let line = pack_marker_line(&survived, &pack).expect("a survival emits");
        let payload = line
            .strip_prefix(SCOPE_PACK_MARKER)
            .expect("the marker prefix")
            .trim();
        assert!(
            base64::engine::general_purpose::STANDARD
                .decode(payload)
                .is_ok(),
            "a surviving payload is base64: {payload}"
        );

        let dead = ScopeReport {
            stages: vec![stage("validate", false)],
            digest: None,
            cost: Some(0.1),
            rounds: Vec::new(),
            transcript: String::new(),
        };
        assert!(
            pack_marker_line(&dead, &pack).is_none(),
            "a dead proposal has no pack to deliver"
        );

        fs::write(pack.join("bloat.bin"), vec![0u8; PACK_TAR_CAP_BYTES + 1]).unwrap();
        let line = pack_marker_line(&survived, &pack).expect("an oversize survival still emits");
        let payload = line.strip_prefix(SCOPE_PACK_MARKER).unwrap().trim();
        let v: serde_json::Value = serde_json::from_str(payload).expect("an error object");
        assert!(
            v["error"].as_str().is_some_and(|e| e.contains("cap")),
            "the error payload names the cap: {payload}"
        );

        let _ = fs::remove_dir_all(&pack);
    }

    /// The propose path preserves what the turn streamed: a round-delimiter `note` line, then the
    /// agent's own output as nested session `agent` events, the exact NDJSON the SPA's session
    /// renderer folds.
    #[test]
    fn propose_records_a_round_delimited_transcript() {
        let repo = tempdir("transcript-repo");
        git_repo_fixture(&repo);
        let out = tempdir("transcript-out");
        let goal_dir = tempdir("transcript-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        let mut body = scripted_proposer(true);
        body.push_str("echo 'drafting the pack now'\n");
        write_exec(&script, &body);

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );

        let lines: Vec<&str> = report.transcript.lines().collect();
        assert!(!lines.is_empty(), "the transcript survived the turn");
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("first line parses");
        assert_eq!(first["kind"], "note");
        assert_eq!(first["msg"], "round 1: propose turn");
        assert!(
            lines.iter().any(|l| {
                serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| {
                    v["kind"] == "agent" && v["event"]["text"] == "drafting the pack now"
                })
            }),
            "the agent's streamed output is preserved as nested agent events: {lines:?}"
        );
        // The transcript never rides the report JSON itself (it has its own delivery path).
        let json = serde_json::to_value(&report).expect("serializes");
        assert!(json.get("transcript").is_none());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    // -- The adversarial gaming review -----------------------------------------------------------

    #[test]
    fn gaming_review_pass_freezes_with_verdict_recorded() {
        let repo = tempdir("gaming-pass-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-pass-out");
        let goal_dir = tempdir("gaming-pass-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(
            &script,
            &with_review(
                &values_proposer(100, 10, 3),
                "echo '{\"verdict\":\"pass\"}'",
            ),
        );

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts_review(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "a passing review must freeze: {:?}",
            report.stages
        );
        assert_eq!(
            report.rounds.len(),
            2,
            "round 1 propose, round 2 the adversary review: {:?}",
            report.rounds
        );
        assert_eq!(report.rounds[0].kind, RoundKind::Propose);
        assert_eq!(report.rounds[1].kind, RoundKind::Adversary);
        assert!(matches!(report.rounds[1].outcome, RoundOutcome::Passed));

        let scope_md = fs::read_to_string(out.join("SCOPE.md")).expect("SCOPE.md written");
        assert!(scope_md.contains("Round 2 (adversary"), "{scope_md}");
        assert!(scope_md.contains("gate discriminates"), "{scope_md}");
        assert!(!out.join("REJECTED.md").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn gaming_review_concerns_earns_one_refine_round_then_passes_on_re_review() {
        let repo = tempdir("gaming-fix-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-fix-out");
        let goal_dir = tempdir("gaming-fix-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let review_counter = goal_dir.join("review.count");
        let script = goal_dir.join("propose.sh");
        // First review: concerns. Second review (after the one refine round): pass.
        let review = format!(
            "n=$(cat '{c}' 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > '{c}'\nif [ \"$n\" -eq 1 ]; then\n  echo '{{\"verdict\":\"concerns\",\"attacks\":[{{\"kind\":\"self-report\",\"narrative\":\"n\",\"suggestion\":\"s\"}}]}}'\nelse\n  echo '{{\"verdict\":\"pass\"}}'\nfi",
            c = review_counter.display(),
        );
        write_exec(&script, &with_review(&values_proposer(100, 10, 3), &review));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts_review(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "recovers after one refine round: {:?}",
            report.stages
        );
        assert_eq!(report.rounds.len(), 4, "{:?}", report.rounds);
        assert_eq!(report.rounds[0].kind, RoundKind::Propose);
        assert_eq!(report.rounds[1].kind, RoundKind::Adversary);
        assert!(matches!(
            report.rounds[1].outcome,
            RoundOutcome::Failed {
                evidence: FailureEvidence::Adversary { .. }
            }
        ));
        assert_eq!(report.rounds[2].kind, RoundKind::Refine);
        assert!(matches!(report.rounds[2].outcome, RoundOutcome::Passed));
        assert_eq!(report.rounds[3].kind, RoundKind::Adversary);
        assert!(matches!(report.rounds[3].outcome, RoundOutcome::Passed));

        assert!(out.join("SCOPE.md").exists());
        assert!(!out.join("REJECTED.md").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn gaming_review_still_concerns_after_refine_rejects_with_attack_trail() {
        let repo = tempdir("gaming-stuck-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-stuck-out");
        let goal_dir = tempdir("gaming-stuck-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // Every review finds the same concern, the one refine round never actually closes it.
        write_exec(
            &script,
            &with_review(
                &values_proposer(100, 10, 3),
                "echo '{\"verdict\":\"concerns\",\"attacks\":[{\"kind\":\"boundary\",\"narrative\":\"still wide open\",\"suggestion\":\"narrow the boundary\"}]}'",
            ),
        );

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts_review(&repo, &script)),
        );
        assert!(
            !report.stages.last().expect("a last stage").passed,
            "still-concerns after the one refine round must reject: {:?}",
            report.stages
        );
        assert_eq!(report.rounds.len(), 4, "{:?}", report.rounds);
        assert_eq!(report.rounds[3].kind, RoundKind::Adversary);
        assert!(matches!(
            report.rounds[3].outcome,
            RoundOutcome::Failed {
                evidence: FailureEvidence::Adversary { .. }
            }
        ));
        assert!(report.digest.is_none(), "nothing frozen");
        assert!(!out.join("SCOPE.md").exists());

        let rejected = fs::read_to_string(out.join("REJECTED.md")).expect("REJECTED.md written");
        assert!(rejected.contains("still found concerns") || rejected.contains("Refine trail"));
        assert!(rejected.contains("still wide open"), "{rejected}");
        assert!(rejected.contains("narrow the boundary"), "{rejected}");

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn gaming_review_malformed_verdict_is_an_error_not_a_pass() {
        let repo = tempdir("gaming-malformed-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-malformed-out");
        let goal_dir = tempdir("gaming-malformed-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // The adversary "turn" babbles instead of emitting a verdict, must fail closed, not pass.
        write_exec(
            &script,
            &with_review(
                &values_proposer(100, 10, 3),
                "echo 'looks fine to me, no notes'",
            ),
        );

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts_review(&repo, &script)),
        );
        assert!(
            !report.stages.last().expect("a last stage").passed,
            "a malformed verdict must not freeze: {:?}",
            report.stages
        );
        assert_eq!(report.rounds.len(), 2, "{:?}", report.rounds);
        assert_eq!(report.rounds[1].kind, RoundKind::Adversary);
        match &report.rounds[1].outcome {
            RoundOutcome::Error { detail } => {
                assert!(detail.contains("did not parse"), "{detail}");
            }
            other => panic!("expected an Error outcome, got {other:?}"),
        }
        assert!(report.digest.is_none());
        assert!(!out.join("SCOPE.md").exists());

        let rejected = fs::read_to_string(out.join("REJECTED.md")).expect("REJECTED.md written");
        assert!(rejected.contains("no parseable verdict"), "{rejected}");

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn skip_gaming_review_flag_skips_the_adversary_turn() {
        let repo = tempdir("gaming-skip-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-skip-out");
        let goal_dir = tempdir("gaming-skip-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // If the review ever ran it would babble (malformed), proving it never runs.
        write_exec(
            &script,
            &with_review(&values_proposer(100, 10, 3), "echo 'never runs'"),
        );

        let mut opts = propose_opts(&repo, &script);
        opts.skip_gaming_review = true;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );
        assert_eq!(
            report.rounds.len(),
            1,
            "no adversary round when the flag is set: {:?}",
            report.rounds
        );
        assert!(out.join("SCOPE.md").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// A review script that finds concerns on its first `concern_reviews` looks (persisted counter)
    /// and passes on every look after, the scripted stand-in for an adversary whose attacks keep
    /// getting fixed.
    fn counting_review(counter: &Path, concern_reviews: u32) -> String {
        format!(
            "n=$(cat '{c}' 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > '{c}'\nif [ \"$n\" -le {concern_reviews} ]; then\n  echo '{{\"verdict\":\"concerns\",\"attacks\":[{{\"kind\":\"boundary\",\"narrative\":\"attack $n\",\"suggestion\":\"fix $n\"}}]}}'\nelse\n  echo '{{\"verdict\":\"pass\"}}'\nfi",
            c = counter.display(),
        )
    }

    /// `--gaming-refine-rounds 2`: concerns→fix→concerns→fix→pass survives. Each refined pack
    /// re-validates and earns its own fresh adversary look before the freeze.
    #[test]
    fn gaming_refine_rounds_two_survives_a_second_concern_cycle() {
        let repo = tempdir("gaming-n2-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-n2-out");
        let goal_dir = tempdir("gaming-n2-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let review_counter = goal_dir.join("review.count");
        let script = goal_dir.join("propose.sh");
        write_exec(
            &script,
            &with_review(
                &values_proposer(100, 10, 3),
                &counting_review(&review_counter, 2),
            ),
        );

        let mut opts = propose_opts_review(&repo, &script);
        opts.gaming_refine_rounds = 2;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            report.stages.iter().all(|s| s.passed),
            "two concern cycles then a pass must freeze: {:?}",
            report.stages
        );
        let kinds: Vec<RoundKind> = report.rounds.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RoundKind::Propose,
                RoundKind::Adversary,
                RoundKind::Refine,
                RoundKind::Adversary,
                RoundKind::Refine,
                RoundKind::Adversary,
            ],
            "{:?}",
            report.rounds
        );
        assert!(matches!(report.rounds[5].outcome, RoundOutcome::Passed));
        assert!(out.join("SCOPE.md").exists());
        assert!(!out.join("REJECTED.md").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// `--gaming-refine-rounds 2` with an adversary that never relents: the LAST look is final,
    /// concerns at cycle 2 reject with the whole trail, and the failure names the round count.
    #[test]
    fn gaming_refine_rounds_two_still_concerns_at_final_look_rejects() {
        let repo = tempdir("gaming-n2-stuck-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-n2-stuck-out");
        let goal_dir = tempdir("gaming-n2-stuck-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(
            &script,
            &with_review(
                &values_proposer(100, 10, 3),
                "echo '{\"verdict\":\"concerns\",\"attacks\":[{\"kind\":\"boundary\",\"narrative\":\"still wide open\",\"suggestion\":\"narrow the boundary\"}]}'",
            ),
        );

        let mut opts = propose_opts_review(&repo, &script);
        opts.gaming_refine_rounds = 2;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            !report.stages.last().expect("a last stage").passed,
            "still-concerns at the final look must reject: {:?}",
            report.stages
        );
        assert_eq!(report.rounds.len(), 6, "{:?}", report.rounds);
        assert_eq!(report.rounds[5].kind, RoundKind::Adversary);
        assert!(matches!(
            report.rounds[5].outcome,
            RoundOutcome::Failed {
                evidence: FailureEvidence::Adversary { .. }
            }
        ));
        assert!(
            report.stages[1].detail.contains("after 2 refine round(s)"),
            "the failure names the exhausted bound: {}",
            report.stages[1].detail
        );
        assert!(report.digest.is_none(), "nothing frozen");
        assert!(!out.join("SCOPE.md").exists());
        assert!(out.join("REJECTED.md").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// The per-round budget gate holds between gaming-review cycles: an over-budget turn stops
    /// with the budget message instead of paying for another refine round.
    #[test]
    fn gaming_refine_rounds_budget_stops_a_second_cycle() {
        let repo = tempdir("gaming-n2-budget-repo");
        git_repo_fixture(&repo);
        let out = tempdir("gaming-n2-budget-out");
        let goal_dir = tempdir("gaming-n2-budget-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // Every turn reports a real $3 cost; the adversary never relents. Trace against
        // --max-cost 10: propose $3, review $6 (concerns, under budget -> refine), refine $9,
        // re-review $12 (concerns, 12 >= 10 -> budget stop before cycle 2).
        let result_line = r#"{"v":1,"kind":"result","subtype":"success","stop_reason":"end_turn","duration_ms":1,"api_duration_ms":1,"ttft_ms":1,"turns":1,"cost_usd":3.0}"#;
        let body = with_review(
            &values_proposer(100, 10, 3),
            "echo '{\"verdict\":\"concerns\",\"attacks\":[{\"kind\":\"boundary\",\"narrative\":\"still wide open\",\"suggestion\":\"narrow the boundary\"}]}'",
        );
        let body = body
            .strip_prefix("#!/bin/sh\n")
            .unwrap_or(&body)
            .to_string();
        write_exec(
            &script,
            &format!("#!/bin/sh\nset -e\necho '{result_line}'\n{body}"),
        );

        let mut opts = propose_opts_review(&repo, &script);
        opts.gaming_refine_rounds = 2;
        opts.max_cost = 10.0;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            !report.stages.last().expect("a last stage").passed,
            "budget exhaustion fails the stage: {:?}",
            report.stages
        );
        assert_eq!(
            report.rounds.len(),
            4,
            "propose, review, refine, re-review — cycle 2 never starts: {:?}",
            report.rounds
        );
        assert!(
            report.stages[1].detail.contains("budget is exhausted"),
            "the failure is honest about why: {}",
            report.stages[1].detail
        );
        let total = report.cost.expect("cost recorded");
        assert!((total - 12.0).abs() < 1e-9, "four $3 turns, got ${total}");
        assert!(out.join("REJECTED.md").exists());
        assert!(!out.join("SCOPE.md").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// A scripted `command`-backend "T1 proposer": authors a two-file harness under `tools/`
    /// (the measure script plus a pinned workload fixture it reads) instead of a bare `measure.sh`,
    /// each with its own frozen `[[workspace.inject]]` entry, the shape the T1 path asks
    /// for. The score comes from `value.txt` (NOT frozen, the file the selftest controls legitimately
    /// stage, the "candidate's own edit"); `tools/fixture.txt` is
    /// the frozen, pinned workload input the harness also reads, proving a multi-file frozen
    /// harness round-trips through `crucible check`. `direction = "lower"` (a wall-clock-shaped
    /// metric). Writes to the workspace-relative pack dir, same contract as [`scripted_proposer`].
    fn t1_proposer() -> String {
        let out = Path::new(PACK_WORK_DIR);
        format!(
            "#!/bin/sh\nset -e\nmkdir -p '{out}/tools'\ncat > '{out}/crucible.toml' <<'MANIFEST'\n\
             [repo]\n\
             path = \".\"\n\
             [workspace]\n\
             dir = \"workspace\"\n\
             setup_cmd = \"mkdir -p workspace/tools && cp tools/bench.sh workspace/tools/ && cp tools/fixture.txt workspace/tools/ && echo 0 > workspace/value.txt && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline\"\n\
             [agent]\n\
             backend = \"command\"\n\
             agent_cmd = \"true\"\n\
             goal = \"speed up the thing\"\n\
             [judge]\n\
             measure_cmd = \"./tools/bench.sh\"\n\
             direction = \"lower\"\n\
             objective = \"perf\"\n\
             [[workspace.inject]]\n\
             src = \"tools/bench.sh\"\n\
             dst = \"tools/bench.sh\"\n\
             frozen = true\n\
             [[workspace.inject]]\n\
             src = \"tools/fixture.txt\"\n\
             dst = \"tools/fixture.txt\"\n\
             frozen = true\n\
             [judge.selftest]\n\
             good_cmd = \"echo 10 > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm good\"\n\
             bad_cmd = \"echo 100 > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm bad\"\n\
             runs = 3\n\
             MANIFEST\n\
             cat > '{out}/tools/bench.sh' <<'BENCH'\n\
             #!/bin/sh\n\
             pin=$(cat tools/fixture.txt 2>/dev/null || echo missing-fixture)\n\
             v=$(cat value.txt 2>/dev/null || echo 0)\n\
             if [ \"$pin\" != \"pinned-workload\" ]; then echo '{{\"valid\": false, \"score\": 0, \"note\": \"fixture not frozen\"}}'; exit 0; fi\n\
             echo \"{{\\\"valid\\\": true, \\\"score\\\": $v}}\"\n\
             BENCH\n\
             chmod +x '{out}/tools/bench.sh'\n\
             echo pinned-workload > '{out}/tools/fixture.txt'\n",
            out = out.display(),
        )
    }

    #[test]
    fn propose_authors_a_t1_tools_harness_and_passes_the_full_loop() {
        // Tier-dependent pack shape: a T1 pack (an authored tools/ harness with a
        // fixture, both frozen-injected) must validate exactly like a T0 measure.sh pack, the manifest
        // shape is identical, only measure_cmd's target and the extra inject entry differ.
        let repo = tempdir("t1-propose-repo");
        git_repo_fixture(&repo);
        let out = tempdir("t1-propose-out");
        let goal_dir = tempdir("t1-propose-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "make the reticulator faster").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &t1_proposer());

        let mut opts = propose_opts(&repo, &script);
        opts.tier = ProposeTier::T1;
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            report.stages.iter().all(|s| s.passed),
            "a T1 tools/ harness must validate + freeze like any other pack: {:?}",
            report.stages
        );
        assert!(out.join("tools/bench.sh").exists());
        assert!(out.join("tools/fixture.txt").exists());
        assert!(out.join("SCOPE.md").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    #[test]
    fn tier_renders_into_the_propose_and_refine_prompts() {
        // The confirmed tier must reach the agent-visible prompt: it must reach the
        // agent-visible prompt text, for both the propose turn and any later refine round, a refine
        // turn must know which tier's shape it's fixing, not just the propose turn.
        let out = PathBuf::from("/tmp/does-not-need-to-exist-for-rendering");
        let t0 = render_propose_prompt(
            "fix it",
            &out,
            "https://example.invalid/x.git",
            ProposeTier::T0,
            false,
        );
        assert!(t0.contains("Confirmed tier: T0"), "{t0}");
        let t1 = render_propose_prompt(
            "fix it",
            &out,
            "https://example.invalid/x.git",
            ProposeTier::T1,
            false,
        );
        assert!(t1.contains("Confirmed tier: T1"), "{t1}");

        let evidence = crate::refine::FailureEvidence::Structure {
            detail: "no [judge.selftest] table".to_string(),
        };
        let refine_t1 =
            refine::render_refine_prompt("fix it", &out, &evidence, 2, ProposeTier::T1, false);
        assert!(refine_t1.contains("Confirmed tier: T1"), "{refine_t1}");
    }

    #[test]
    fn tier_defaults_to_t0_when_the_cli_flag_is_absent() {
        // Back-compat: every call site predating the `--tier` flag never set --tier, and must keep
        // drafting T0-shaped packs exactly as before.
        use clap::Parser;
        let cli = crate::Cli::parse_from([
            "crucible",
            "scope",
            "--propose",
            "--issue",
            "a/b#1",
            "--repo",
            "x",
            "--out",
            "/tmp/x",
        ]);
        let crate::Cmd::Scope(args) = cli.command.expect("scope subcommand") else {
            panic!("expected Scope");
        };
        assert_eq!(args.tier, None, "no --tier flag on the CLI");
        assert_eq!(args.tier.unwrap_or_default(), ProposeTier::T0);
    }

    /// Like [`git_repo_fixture`] but with an `origin` remote set, mirroring a turn pod's
    /// `/tmp/crucible-turn-checkout` (a fresh clone whose origin is the upstream URL). Returns the
    /// HEAD SHA the freeze must pin as `ref`.
    fn git_repo_fixture_with_origin(dir: &Path, origin: &str) -> String {
        git_repo_fixture(dir);
        let git = |args: &[&str]| -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("spawn git");
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_string()
        };
        git(&["remote", "add", "origin", origin]);
        git(&["rev-parse", "HEAD"])
    }

    /// Freeze rewrites the propose turn's local `[repo] path` into `url` (the checkout's origin) +
    /// `ref` (its HEAD SHA), the un-runnable-pack failure #1 (the run pod can't clone the scope
    /// pod's local checkout path).
    #[test]
    fn freeze_pins_repo_to_origin_url_and_head_ref() {
        let repo = tempdir("pin-repo");
        let head = git_repo_fixture_with_origin(&repo, "https://example.com/upstream.git");
        let out = tempdir("pin-out");
        let goal_dir = tempdir("pin-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // scripted_proposer writes `[repo] path = "."`, exactly what the freeze must replace.
        write_exec(&script, &scripted_proposer(true));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );

        let manifest = fs::read_to_string(out.join(MANIFEST_FILE)).expect("frozen manifest");
        assert!(
            manifest.contains(r#"url = "https://example.com/upstream.git""#),
            "pinned the origin url: {manifest}"
        );
        assert!(
            manifest.contains(&format!(r#"ref = "{head}""#)),
            "pinned the HEAD sha: {manifest}"
        );
        assert!(
            !manifest.contains("path ="),
            "the scope pod's local path is gone: {manifest}"
        );
        // The real loader still parses the rewritten manifest.
        crate::manifest::Manifest::load(&out.join(MANIFEST_FILE))
            .expect("normalized manifest parses");

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// Freeze injects an empty `[deploy]` block when the drafted manifest lacks one, the deploy
    /// renderer requires the block even when every field defaults (un-runnable-pack failure #2).
    #[test]
    fn freeze_injects_an_empty_deploy_block() {
        let repo = tempdir("deploy-repo");
        git_repo_fixture(&repo);
        let out = tempdir("deploy-out");
        let goal_dir = tempdir("deploy-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        // scripted_proposer writes no [deploy] table.
        write_exec(&script, &scripted_proposer(true));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );

        let manifest = fs::read_to_string(out.join(MANIFEST_FILE)).expect("frozen manifest");
        assert!(
            manifest.contains("[deploy]"),
            "empty deploy injected: {manifest}"
        );
        let m = crate::manifest::Manifest::load(&out.join(MANIFEST_FILE)).expect("parses");
        assert!(
            m.deploy.is_some(),
            "the [deploy] block is present after freeze"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// When the scope turn itself ran sandboxed (`--agent-backend openshell --sandbox-image X`), the
    /// freeze pins the run pod to the same backend + image, replacing the drafted `backend =
    /// "command"/"local"` the scope argv had overridden (un-runnable-pack failure #3).
    #[test]
    fn freeze_pins_the_openshell_agent_backend_when_the_scope_ran_sandboxed() {
        let repo = tempdir("agent-repo");
        git_repo_fixture(&repo);
        let out = tempdir("agent-out");
        let goal_dir = tempdir("agent-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &scripted_proposer(true));

        // The scripted double still drives the turn (agent_cmd_override wins in turn_args), but the
        // opts record that the scope ran openshell, which is what the freeze pins.
        let mut opts = propose_opts(&repo, &script);
        opts.agent_backend = AgentBackend::Openshell;
        opts.sandbox_image = Some("ghcr.io/neuralmagic/crucible-sandbox:latest".to_string());
        let report = execute(&out, None, Some(&goal_file), false, Some(opts));
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );

        let manifest = fs::read_to_string(out.join(MANIFEST_FILE)).expect("frozen manifest");
        assert!(
            manifest.contains(r#"backend = "openshell""#),
            "backend pinned to openshell: {manifest}"
        );
        assert!(
            manifest.contains(r#"sandbox_image = "ghcr.io/neuralmagic/crucible-sandbox:latest""#),
            "sandbox image pinned: {manifest}"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// When the scope ran local (the dev path), the freeze leaves the drafted `[agent].backend`
    /// alone, no openshell rewrite.
    #[test]
    fn freeze_leaves_the_agent_backend_alone_on_the_local_path() {
        let repo = tempdir("local-repo");
        git_repo_fixture(&repo);
        let out = tempdir("local-out");
        let goal_dir = tempdir("local-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &scripted_proposer(true));

        // propose_opts defaults to AgentBackend::Local.
        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );

        let manifest = fs::read_to_string(out.join(MANIFEST_FILE)).expect("frozen manifest");
        assert!(
            manifest.contains(r#"backend = "command""#),
            "the drafted backend is untouched: {manifest}"
        );
        assert!(
            !manifest.contains("sandbox_image"),
            "no sandbox image pinned on the local path: {manifest}"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// Freeze drops the run pod's own checkout (`crucible check` populates `[workspace].dir` during
    /// validation) from the shipped pack, the run phase rebuilds it from the pinned `[repo]`
    /// (un-runnable-pack cleanup #4; #137 only kept `.git` out of the TAR, not the tree out of
    /// `--out`).
    #[test]
    fn freeze_drops_the_workspace_checkout_from_the_pack() {
        let repo = tempdir("ws-repo");
        git_repo_fixture(&repo);
        let out = tempdir("ws-out");
        let goal_dir = tempdir("ws-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &scripted_proposer(true));

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );
        assert!(
            !out.join("workspace").exists(),
            "the workspace checkout is gone from the frozen pack"
        );
        // The manifest + measure script still ship.
        assert!(out.join(MANIFEST_FILE).exists());
        assert!(out.join("measure.sh").exists());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// The freeze de-prescribes the pack: after the self-test has proven the gate discriminates,
    /// `_controls/` and the `[judge.selftest]` block (and the inject pulling the control into the
    /// workspace) are stripped, so no answer key ships, while unrelated injects and the gate
    /// itself survive. A valid, problem-framing-only pack still freezes (digest present).
    #[test]
    fn freeze_strips_controls_and_selftest_but_keeps_the_gate() {
        let repo = tempdir("deprescribe-repo");
        git_repo_fixture(&repo);
        let out = tempdir("deprescribe-out");
        let goal_dir = tempdir("deprescribe-goal");
        let goal_file = goal_dir.join("goal.md");
        fs::write(&goal_file, "fix the thing").expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &controls_proposer());

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );
        assert!(
            report.digest.is_some(),
            "a de-prescribed pack still freezes"
        );

        // The answer key is gone: no _controls dir, no selftest block, no inject referencing it.
        assert!(
            !out.join(CONTROLS_DIR).exists(),
            "the private controls dir is stripped from the shipped pack"
        );
        let manifest = fs::read_to_string(out.join(MANIFEST_FILE)).expect("manifest ships");
        assert!(
            !manifest.contains("[judge.selftest]"),
            "the selftest block is stripped: {manifest}"
        );
        assert!(
            !manifest.contains(CONTROLS_DIR),
            "no inject or cmd still references _controls: {manifest}"
        );

        // The gate and an unrelated inject survive untouched.
        assert!(
            out.join("measure.sh").exists(),
            "the gate script still ships"
        );
        assert!(
            manifest.contains("measure.sh"),
            "the unrelated frozen inject is kept: {manifest}"
        );

        // The frozen manifest loads with no selftest, and its identity reproduces the shipped
        // digest, proving the digest was computed AFTER the strip, over exactly what ships.
        // Canonicalize like the freeze does (it canonicalizes `--out`); `inject_hash` folds in the
        // absolute dst path, so a `/private`-vs-`/var` symlink difference would otherwise diverge.
        let out_canon = fs::canonicalize(&out).expect("canonicalize out");
        let loaded =
            manifest::Manifest::load_frozen(&out_canon.join(MANIFEST_FILE)).expect("loads");
        assert!(
            loaded.judge.selftest.is_none(),
            "shipped manifest carries no selftest"
        );
        let identity = compute_identity(&out_canon.join(MANIFEST_FILE)).expect("identity");
        assert_eq!(
            Some(identity.digest),
            report.digest,
            "freeze digest covers the de-prescribed manifest"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// SCOPE.md renders the SHIPPED, de-prescribed goal (the pack's `[agent].goal`), never the raw
    /// upstream issue text handed in via `--goal-file`, otherwise the human-facing freeze record
    /// would smuggle the prescription back into a shipped pack file.
    #[test]
    fn scope_md_renders_the_deprescribed_goal_not_the_raw_issue() {
        let repo = tempdir("scopemd-repo");
        git_repo_fixture(&repo);
        let out = tempdir("scopemd-out");
        let goal_dir = tempdir("scopemd-goal");
        let goal_file = goal_dir.join("goal.md");
        // The raw issue prescribes its own fix, exactly what must NOT reach the shipped pack.
        fs::write(
            &goal_file,
            "In plugin.go's makeserver(), change CacheNumBlocks to KvCacheMaxTokenCapacity/blocksize.",
        )
        .expect("write goal file");
        let script = goal_dir.join("propose.sh");
        write_exec(&script, &controls_proposer());

        let report = execute(
            &out,
            None,
            Some(&goal_file),
            false,
            Some(propose_opts(&repo, &script)),
        );
        assert!(
            report.stages.iter().all(|s| s.passed),
            "{:?}",
            report.stages
        );
        let scope_md = fs::read_to_string(out.join("SCOPE.md")).expect("SCOPE.md written");
        assert!(
            scope_md.contains("fix the thing"),
            "renders the de-prescribed shipped goal: {scope_md}"
        );
        assert!(
            !scope_md.contains("makeserver()") && !scope_md.contains("plugin.go"),
            "the raw prescriptive issue text does not leak into SCOPE.md: {scope_md}"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&out);
        let _ = fs::remove_dir_all(&goal_dir);
    }

    /// The rendered propose/refine prompts must keep the de-prescribe guardrail by default, a
    /// regression that silently drops it would quietly re-poison every scoped goal.
    #[test]
    fn scope_prompts_forbid_prescribing_the_fix() {
        let out = PathBuf::from("/tmp/does-not-need-to-exist-for-rendering");
        let propose = render_propose_prompt(
            "fix it",
            &out,
            "https://x.invalid/x.git",
            ProposeTier::T0,
            false,
        );
        assert!(
            propose.contains("frame the problem, NEVER the fix"),
            "propose prompt keeps the de-prescribe contract"
        );
        assert!(
            propose.contains("De-prescribe the upstream issue"),
            "propose prompt tells the agent to strip the issue's own fix"
        );
        let evidence = crate::refine::FailureEvidence::Structure {
            detail: "no [judge.selftest] table".to_string(),
        };
        let refine =
            refine::render_refine_prompt("fix it", &out, &evidence, 2, ProposeTier::T0, false);
        assert!(
            refine.contains("Keep `goal.md` de-prescribed"),
            "refine prompt keeps the de-prescribe guardrail"
        );
    }

    /// The authoritative branch: the rendered prompts drop the de-prescribe guardrail and instead
    /// tell the agent to preserve the brief's prescriptions; the goal text itself rides verbatim.
    #[test]
    fn authoritative_prompts_preserve_the_briefs_prescriptions() {
        let out = PathBuf::from("/tmp/does-not-need-to-exist-for-rendering");
        let goal = "swap the lookup path's allocator for a pooled arena";
        let propose =
            render_propose_prompt(goal, &out, "https://x.invalid/x.git", ProposeTier::T1, true);
        assert!(propose.contains(goal), "the brief rides verbatim");
        assert!(
            propose.contains("authoritative brief"),
            "propose prompt names the brief authoritative: {propose}"
        );
        assert!(
            propose.contains("Carry the brief into `goal.md` faithfully"),
            "propose prompt tells the agent to preserve the prescriptions"
        );
        assert!(
            !propose.contains("De-prescribe the upstream issue")
                && !propose.contains("EXPLICITLY FORBIDDEN"),
            "the de-prescribe contract is fully replaced: {propose}"
        );
        assert!(
            propose.contains("reference fix is private"),
            "the controls-privacy rule survives the swap"
        );

        let evidence = crate::refine::FailureEvidence::Structure {
            detail: "no [judge.selftest] table".to_string(),
        };
        let refine = refine::render_refine_prompt(goal, &out, &evidence, 2, ProposeTier::T1, true);
        assert!(
            refine.contains("carries an authoritative brief"),
            "refine prompt keeps the brief intact across rounds: {refine}"
        );
        assert!(
            !refine.contains("Keep `goal.md` de-prescribed"),
            "refine prompt drops the strip instruction for an authoritative brief"
        );
    }

    /// The rewrite helpers stand alone: an origin-less local fixture pins its absolute path as the
    /// url (still a real, cloneable source), and a checkout with `.git` is dropped.
    #[test]
    fn repo_pin_falls_back_to_the_absolute_path_without_an_origin() {
        let repo = tempdir("pin-fallback");
        git_repo_fixture(&repo); // no origin remote
        let (url, sha) = repo_pin(&repo.display().to_string());
        let abs = fs::canonicalize(&repo).unwrap();
        assert_eq!(url, abs.display().to_string(), "no origin -> absolute path");
        assert!(sha.is_some(), "HEAD sha is still pinned");
        let _ = fs::remove_dir_all(&repo);
    }
}

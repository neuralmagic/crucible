use crate::activity::ActivityFeed;
use crate::agent::AgentBackend;
use crate::deploy::ProposeTier;
use crate::init::MANIFEST_FILE;
use crate::refine::RoundRecord;
use crate::scope::pack::{SCOPE_PACK_MARKER, pack_gz, pack_marker_line};
use crate::scope::pipeline::{
    Freeze, Ingest, Propose, ProposeOpts, ScopeCtx, Stage, StageResult, Validate,
};
use crate::scope::transcript::{
    SCOPE_TRANSCRIPT_MARKER, TRANSCRIPT_CAP_BYTES, cap_transcript, gzip_transcript,
};
use anyhow::{Context, Result};
use base64::Engine as _;
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("--propose requires a goal source: --issue or --goal-file")]
struct ProposeNeedsGoal;

/// The prefix of the single-line scope-report marker `--marker` emits as the command's last output.
/// The controller's WorkPod log scraper matches the same shared literal from `crucible-contract`.
pub use crucible_contract::SCOPE_REPORT_MARKER;

/// The whole pipeline's result as one JSON object, for `--json`.
#[derive(Serialize)]
pub struct ScopeReport {
    pub stages: Vec<StageResult>,
    pub digest: Option<String>,
    /// The `--propose` turn's cost (USD), summed across refine rounds; `None` outside `--propose`.
    pub cost: Option<f64>,
    /// The refine loop's per-round trail; empty outside `--propose`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rounds: Vec<RoundRecord>,
    /// The turns' preserved session NDJSON. Never serialized into the report JSON (it can be MBs);
    /// it rides its own delivery path, the `--marker` transcript line or `--transcript-out`.
    #[serde(skip)]
    pub transcript: String,
}

/// Drive `ingest [-> propose] -> validate -> freeze` over `pack`, stopping at the first failing
/// stage. Pure: no printing, no process exit, [`run`] (the CLI entry point) owns rendering the
/// result and exiting nonzero on failure, so this is the part unit tests drive directly.
pub fn execute(
    pack: &Path,
    issue: Option<&str>,
    goal_file: Option<&Path>,
    force: bool,
    propose: Option<ProposeOpts>,
) -> ScopeReport {
    let manifest_path = pack.join(MANIFEST_FILE);
    let mut ctx = ScopeCtx {
        pack: pack.to_path_buf(),
        manifest_path,
        goal: None,
        goal_source: None,
        check_outcome: None,
        identity: None,
        propose_cost: None,
        refine_rounds: Vec::new(),
        transcript: String::new(),
        activity: ActivityFeed::new(
            crucible_contract::SCOPE_ACTIVITY_MARKER,
            propose.as_ref().is_some_and(|o| o.progress),
        ),
    };

    let mut stages: Vec<Box<dyn Stage>> = vec![Box::new(Ingest {
        issue: issue.map(str::to_string),
        goal_file: goal_file.map(Path::to_path_buf),
    })];
    if let Some(opts) = propose {
        stages.push(Box::new(Propose { opts }));
    }
    stages.push(Box::new(Validate));
    stages.push(Box::new(Freeze { force }));

    let mut results = Vec::with_capacity(stages.len());
    for stage in &stages {
        let result = match stage.run(&mut ctx) {
            Ok(detail) => StageResult {
                name: stage.name(),
                passed: true,
                detail,
            },
            Err(e) => StageResult {
                name: stage.name(),
                passed: false,
                detail: format!("{e:#}"),
            },
        };
        let failed = !result.passed;
        results.push(result);
        if failed {
            break;
        }
    }

    ScopeReport {
        stages: results,
        digest: ctx.identity.as_ref().map(|i| i.digest.clone()),
        cost: ctx.propose_cost,
        rounds: ctx.refine_rounds,
        transcript: ctx.transcript,
    }
}

/// CLI-level options for `crucible scope`, both the classic (hand-written pack) and `--propose`
/// modes. Also the clap `Args` group behind [`crate::Cmd::Scope`].
#[derive(clap::Args)]
pub struct ScopeArgs {
    /// Pack directory (holds its `crucible.toml`). Required unless `--propose`.
    #[arg(long)]
    pub pack: Option<PathBuf>,
    /// Resolve the goal from a GitHub issue (owner/repo#N).
    #[arg(long, conflicts_with = "goal_file")]
    pub issue: Option<String>,
    /// Resolve the goal from a file, instead of the pack manifest's own goal.
    #[arg(long)]
    pub goal_file: Option<PathBuf>,
    /// Overwrite existing `SCOPE.md`, or propose into a non-empty `--out`.
    #[arg(long)]
    pub force: bool,
    /// Emit the pipeline result as one JSON object instead of per-stage lines.
    #[arg(long)]
    pub json: bool,
    /// Draft a fresh pack via one agent turn. Needs `--out`, `--repo`, and a goal source.
    #[arg(long)]
    pub propose: bool,
    /// Code under test the drafted pack's `[repo]` should point at. `--propose` only.
    #[arg(long)]
    pub repo: Option<String>,
    /// Directory to draft the pack into. `--propose` only.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Cap on the propose turn's cost in USD.
    #[arg(long, default_value_t = 5.0)]
    pub max_cost: f64,
    /// Override the propose turn's agent with a `command`-backend script. Test-only.
    #[arg(long, hide = true)]
    pub propose_agent_cmd: Option<String>,
    /// Print the report + transcript on `CRUCIBLE_SCOPE_*:` marker lines for log scraping.
    #[arg(long)]
    pub marker: bool,
    /// Write the turns' gzipped session NDJSON to this path (local executor pickup).
    #[arg(long)]
    pub transcript_out: Option<PathBuf>,
    /// Total rounds the propose refine loop may spend. `1` disables refinement.
    #[arg(long, default_value_t = 3)]
    pub refine_rounds: u32,
    /// Skip the adversarial gaming-review turn (dev iteration only).
    #[arg(long)]
    pub skip_gaming_review: bool,
    /// Max concern→refine→re-review cycles the gaming review may spend.
    #[arg(long, default_value_t = 1)]
    pub gaming_refine_rounds: u32,
    /// Ranker's confirmed tier (`t0` / `t1`). Absent defaults to `t0`.
    #[arg(long, value_enum)]
    pub tier: Option<ProposeTier>,
    /// Real agent backend for the propose/adversary turns.
    #[arg(long, value_enum, default_value_t = crate::agent::AgentBackend::Local)]
    pub agent_backend: AgentBackend,
    /// Sandbox image for `--agent-backend openshell`.
    #[arg(long)]
    pub sandbox_image: Option<String>,
    /// Compute driver for the turns' gateway. Mirrors the loop's `--compute-driver`.
    #[arg(long, value_enum, default_value_t = crate::openshell::gateway::ComputeDriver::Podman)]
    pub compute_driver: crate::openshell::gateway::ComputeDriver,
    /// Goal is an authoritative brief: prompts carry prescriptions into `goal.md` intact.
    #[arg(long)]
    pub authoritative: bool,
}

/// CLI entry point: resolve `--propose` vs. the classic pack path, run the pipeline, print each
/// stage as it runs (or, with `json`, the whole [`ScopeReport`] as one object), and exit nonzero
/// if the pipeline stopped on a failure *or* the propose turn ran over `--max-cost`.
pub fn run(a: ScopeArgs) -> Result<()> {
    let (pack, propose_opts) = if a.propose {
        let out = a
            .out
            .context("--propose requires --out <dir> (the pack to draft into)")?;
        let repo = a
            .repo
            .context("--propose requires --repo <url|path> (the code under test)")?;
        if a.issue.is_none() && a.goal_file.is_none() {
            return Err(ProposeNeedsGoal.into());
        }
        (
            out,
            Some(ProposeOpts {
                repo,
                max_cost: a.max_cost,
                agent_cmd_override: a.propose_agent_cmd,
                force: a.force,
                refine_rounds: a.refine_rounds,
                skip_gaming_review: a.skip_gaming_review,
                gaming_refine_rounds: a.gaming_refine_rounds,
                tier: a.tier.unwrap_or_default(),
                agent_backend: a.agent_backend,
                sandbox_image: a.sandbox_image,
                progress: a.marker,
                compute_driver: a.compute_driver,
                authoritative: a.authoritative,
            }),
        )
    } else {
        (
            a.pack
                .context("scope needs --pack <dir> (or --propose --out <dir>)")?,
            None,
        )
    };

    let max_cost = propose_opts.as_ref().map(|o| o.max_cost).unwrap_or(0.0);
    // A controller-dispatched scope pod: adopt the dispatch as this turn's trace parent so the
    // `openshell_turn` nests under the controller's tree instead of floating orphaned. `None` (a
    // local run, or telemetry off) changes nothing. Closed right after the turn, not at fn end,
    // the failure tail exits via `process::exit`, which would skip the drop and lose the span.
    let turn_span =
        crate::engine::turn_span(crate::engine::TurnSpanKind::Scope, a.issue.as_deref());
    let report = {
        let _turn_guard = turn_span.as_ref().map(tracing::Span::enter);
        execute(
            &pack,
            a.issue.as_deref(),
            a.goal_file.as_deref(),
            a.force,
            propose_opts,
        )
    };
    drop(turn_span);
    let over_budget = report.cost.is_some_and(|c| max_cost > 0.0 && c > max_cost);
    let failed = report.stages.last().is_some_and(|r| !r.passed) || over_budget;

    // Tier 2 (large artifacts to drop-box): when the controller injected the ingest drop-box env, the
    // pack + transcript POST there (before the termination message) and ride the Tier 1 manifest.
    // Absent = the old marker path (a local run, or an old controller). `artifacts` is the manifest the
    // envelope carries.
    let ingest = crate::ingest_client::IngestConfig::from_env();
    let mut artifacts: Vec<crucible_contract::ArtifactRef> = Vec::new();

    // Deliver the preserved transcript (capped, gzipped). Over the drop-box it POSTs; on the marker
    // path it prints before the report so the report marker stays the command's last line for the
    // log scraper.
    if !report.transcript.is_empty() {
        let (capped, _dropped) = cap_transcript(&report.transcript, TRANSCRIPT_CAP_BYTES);
        let gz = gzip_transcript(&capped)?;
        if let Some(path) = &a.transcript_out {
            std::fs::write(path, &gz)
                .with_context(|| format!("writing --transcript-out {}", path.display()))?;
        }
        if a.json && a.marker {
            if let Some(cfg) = &ingest {
                artifacts.push(crate::ingest_client::post_artifact(
                    cfg,
                    crucible_contract::ArtifactKind::ScopeTranscript,
                    &gz,
                ));
            } else {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&gz);
                println!("{SCOPE_TRANSCRIPT_MARKER} {b64}");
            }
        }
    }

    // A surviving pack must reach the controller (`scope_executor = pod`, where this process's
    // filesystem dies with the pod). Over the drop-box it POSTs as the `scope-pack` artifact; on the
    // marker path it rides one log line before the report. A pack that can't be built at all still
    // emits the `{"error":…}` marker so the controller fails the scope loudly, never silently.
    if a.json && a.marker {
        let survived = report.digest.is_some() && report.stages.iter().all(|r| r.passed);
        if survived {
            match &ingest {
                Some(cfg) => match pack_gz(&pack) {
                    Ok(gz) => artifacts.push(crate::ingest_client::post_artifact(
                        cfg,
                        crucible_contract::ArtifactKind::ScopePack,
                        &gz,
                    )),
                    Err(e) => {
                        println!(
                            "{SCOPE_PACK_MARKER} {}",
                            serde_json::json!({ "error": format!("{e:#}") })
                        );
                    }
                },
                None => {
                    if let Some(line) = pack_marker_line(&report, &pack) {
                        println!("{line}");
                    }
                }
            }
        }
    }

    if a.json {
        let json = serde_json::to_string_pretty(&report)?;
        println!("{json}");
        if a.marker {
            let compact = serde_json::to_string(&report)?;
            println!("{SCOPE_REPORT_MARKER} {compact}");
            // The report core also rides the Tier 1 termination message (verdict in `/dev/termination-log`);
            // the controller prefers it and falls back to this marker. Its `artifacts` manifest is the
            // authoritative index of the Tier 2 uploads above (empty on the marker path), so a missing or
            // undelivered artifact is caught against the kubelet-authenticated message.
            crate::result_mode::emit_with_artifacts(
                crucible_contract::EnvelopeKind::ScopeReport,
                serde_json::to_value(&report)?,
                artifacts,
            );
        }
    } else {
        for r in &report.stages {
            println!(
                "[crucible scope] {}: {}",
                r.name,
                if r.passed { "PASS" } else { "FAIL" }
            );
            println!("  {}", r.detail);
        }
        if over_budget {
            eprintln!(
                "[crucible scope] OVER BUDGET: propose turn cost ${:.4} exceeds --max-cost ${:.2}",
                report.cost.unwrap_or_default(),
                max_cost
            );
        }
    }

    if failed {
        // `process::exit` skips `EngineCtx::Drop`; flush buffered spans first.
        crate::engine::flush();
        std::process::exit(1);
    }
    Ok(())
}

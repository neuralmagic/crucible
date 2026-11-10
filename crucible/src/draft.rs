//! `crucible draft`: one intent-to-pack drafting turn for the controller's draft studio. The
//! controller shells this (`POST /api/playbook-drafts/{id}/generate`) instead of any `claude`
//! subprocess: the loop image carries no `claude` CLI, and the engine already owns the
//! one-shot-turn machinery (`scope`, `rank-grounded`) this needs. The turn drafts a playbook pack
//! from a natural-language intent, the command validates the pack with the check's compile-only
//! slice, and failed findings seed refine rounds.
//!
//! Trust boundary: everything the turn writes is untrusted until a human signs it off in the
//! draft studio. The pack is never executed here — [`crate::cli::check::run_compile_only`] parses
//! and compiles but runs no pack content, because a draft is inert until launch. The pack lands
//! whole in `--out` even when the final round fails the check: a non-compiling save is a save, the
//! studio shows the diagnostics, and the human decides.
//!
//! The openshell round-trip invariant from `scope` applies: the agent writes only under its
//! workspace, so the pack is drafted into `{DRAFT_WORK_DIR}` *inside* the turn's scratch workspace
//! and mirrored onto the host `--out` after every round.

use crate::agent::event::AgentEvent;
use crate::agent::sandbox_env::{EnvSource, SandboxEnv, process_env};
use crate::agent::{self};
use crate::args::Paths;
use crate::cli::init::MANIFEST_FILE;
use crate::manifest::AgentBackend;
use anyhow::{Context, Result};
use crucible_contract::draft::DraftReport;
use crucible_contract::pack_tree::walk_dir;
use std::path::{Path, PathBuf};

const DRAFT_PROMPT: &str = include_str!("prompts/draft-pack.md");
const DRAFT_REFINE_PROMPT: &str = include_str!("prompts/draft-refine.md");

/// Where the drafting turn writes the pack, inside the turn's workspace (the scratch dir, the
/// agent's cwd). See the module doc for why the pack cannot go straight to `--out`.
const DRAFT_WORK_DIR: &str = "_draft_out";

/// The seed context the drafting turn reads: the intent, the workflow DSL reference, the worked
/// example, and the draft's current files when extending.
const CONTEXT_DIR: &str = "_draft_context";

/// The engine's own worked-example assets, embedded so the seed context travels with the binary
/// regardless of the invocation cwd (the same pattern `scope`'s seed context uses). CI checks
/// `examples/playbook`, so the embedded example cannot rot silently.
const EXAMPLE_FILES: &[(&str, &str)] = &[
    (
        "crucible.toml",
        include_str!("../../examples/playbook/crucible.toml"),
    ),
    (
        "workflow.star",
        include_str!("../../examples/playbook/workflow.star"),
    ),
    (
        "README.md",
        include_str!("../../examples/playbook/README.md"),
    ),
    ("role.sh", include_str!("../../examples/playbook/role.sh")),
    ("shape.sh", include_str!("../../examples/playbook/shape.sh")),
    (
        "roundup.sh",
        include_str!("../../examples/playbook/roundup.sh"),
    ),
    (
        "prompts/draft.md",
        include_str!("../../examples/playbook/prompts/draft.md"),
    ),
    (
        "prompts/polish.md",
        include_str!("../../examples/playbook/prompts/polish.md"),
    ),
    (
        "prompts/audit.md",
        include_str!("../../examples/playbook/prompts/audit.md"),
    ),
    (
        "reports/slack.md.j2",
        include_str!("../../examples/playbook/reports/slack.md.j2"),
    ),
];

/// CLI-level options for `crucible draft`. Also the clap `Args` group behind
/// [`crate::cli::Cmd::Draft`].
#[derive(clap::Args)]
pub struct DraftArgs {
    /// File holding the intent to draft from. A path, not inline text: intents run long, and argv
    /// is a surface logs capture.
    #[arg(long)]
    pub goal_file: PathBuf,
    /// Directory the drafted pack lands in. Refused when non-empty unless `--force`.
    #[arg(long)]
    pub out: PathBuf,
    /// Directory holding the draft's current files; seeded into the turn as extend-from context,
    /// so the agent carries them forward instead of starting over.
    #[arg(long)]
    pub context: Option<PathBuf>,
    /// Cap on the total cost of the drafting turns in USD.
    #[arg(long, default_value_t = 5.0)]
    pub max_cost: f64,
    /// Total turns: one drafting turn, then refine turns seeded with the check's findings.
    #[arg(long, default_value_t = 2)]
    pub refine_rounds: u32,
    /// Draft into `--out` even when it already holds files.
    #[arg(long)]
    pub force: bool,
    /// Emit the report as one JSON object on stdout instead of human lines.
    #[arg(long)]
    pub json: bool,
    /// Real agent backend for the turns; ignored when `--agent-cmd` is set.
    #[arg(long, value_enum, default_value_t = crate::manifest::AgentBackend::Local)]
    pub agent_backend: AgentBackend,
    /// Sandbox image for `--agent-backend openshell`.
    #[arg(long)]
    pub sandbox_image: Option<String>,
    /// Override the turns' agent with a `command`-backend script (test hook).
    #[arg(long, hide = true)]
    pub agent_cmd: Option<String>,
    /// The harness that runs the turns; the claude default when unset.
    #[arg(long, value_enum)]
    pub harness: Option<crate::manifest::Harness>,
    /// Override the turns' model.
    #[arg(long)]
    pub model: Option<String>,
    /// Compute driver for the turns' gateway. Mirrors the loop's `--compute-driver`.
    #[arg(long, value_enum, default_value_t = crate::openshell::gateway::ComputeDriver::Podman)]
    pub compute_driver: crate::openshell::gateway::ComputeDriver,
}

/// Why a drafting loop refused to start: a caller-facing refusal, not plumbing.
#[derive(Debug, thiserror::Error)]
pub enum DraftError {
    #[error("the intent file {path} is empty — a draft needs something to draft")]
    EmptyIntent { path: PathBuf },
}

/// The engine args for the drafting turns. Unlike `rank-grounded` the reasoning effort is left
/// alone: drafting a pack is the turn's whole job, not a one-line verdict.
fn turn_args(a: &DraftArgs) -> Result<crate::args::Args> {
    let mut args = crate::args::Args::defaults().context("constructing default args")?;
    args.harness = a.harness;
    match &a.agent_cmd {
        Some(cmd) => {
            args.agent_backend = AgentBackend::Command;
            args.agent_cmd = Some(cmd.clone());
        }
        None => args.agent_backend = a.agent_backend,
    }
    args.sandbox_image = a.sandbox_image.clone();
    args.compute_driver = a.compute_driver;
    if let Some(model) = &a.model {
        args.model = Some(model.clone());
    }
    // Manifest-less turn: the Vertex agent env normally supplied by `[agent].env` comes from the
    // turn pod's own env instead. Only for a Vertex-authenticated harness.
    if crate::agent::harness::resolve_auth(
        args.harness(),
        &crate::agent::inference::InferenceEnv::from_process_env()?,
    ) == crucible::harness::AuthProvider::Vertex
    {
        let mut env = SandboxEnv::default();
        env.relay(
            EnvSource::VertexRelay,
            crate::openshell::policy::VERTEX_RELAY_KEYS.iter().copied(),
            process_env,
        );
        args.env = env.into_vars();
    }
    Ok(args)
}

fn draft_paths(scratch: &Path) -> Paths {
    Paths {
        workspace: scratch.to_path_buf(),
        skills: None,
        steer: scratch.join("STEER.md"),
        state: scratch.join("state"),
        session_log: scratch.join("state/session.jsonl"),
        control: scratch.join("state/control.json"),
        admissions: scratch.join("state/admissions.jsonl"),
        escalation: scratch.join("ESCALATION.json"),
        provisioning: scratch.join("PROVISIONING_PENDING.json"),
    }
}

/// Seed the turn's `_draft_context/`: the intent, the DSL reference generated from the engine
/// itself, the worked example, and the draft's current files when a `--context` dir was given.
fn write_seed_context(scratch: &Path, goal: &str, context: Option<&Path>) -> Result<()> {
    let ctx = scratch.join(CONTEXT_DIR);
    std::fs::create_dir_all(&ctx).with_context(|| format!("creating {}", ctx.display()))?;
    std::fs::write(ctx.join("GOAL.md"), goal)?;
    std::fs::write(
        ctx.join("workflow-dsl.md"),
        crate::plan::starlark::reference::markdown(),
    )?;
    let example = ctx.join("example");
    for (name, text) in EXAMPLE_FILES {
        let path = example.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(context) = context {
        let current = ctx.join("current");
        crate::scope::pack::copy_tree(context, &current).with_context(|| {
            format!(
                "seeding the current draft files from {} into the turn",
                context.display()
            )
        })?;
    }
    Ok(())
}

/// Mirror the turn's workspace pack dir (`<scratch>/{DRAFT_WORK_DIR}`) onto the host `--out` dir
/// after every round, the same clear-then-copy mirror `scope` uses: a refine round that deletes a
/// file must not leave the stale copy behind for the check to read.
fn sync_pack_from_workspace(scratch: &Path, out: &Path) -> Result<()> {
    let work = scratch.join(DRAFT_WORK_DIR);
    if !work.is_dir() {
        // The turn wrote nothing (and removed the pre-created dir); the check reports the missing
        // manifest as this round's evidence.
        return Ok(());
    }
    crate::scope::pack::clear_dir(out)
        .with_context(|| format!("clearing --out {}", out.display()))?;
    crate::scope::pack::copy_tree(&work, out)
        .with_context(|| format!("mirroring {} -> {}", work.display(), out.display()))
}

/// The agent's refusal, when it wrote one. A decline is terminal — never refined.
fn read_rejected(out: &Path) -> Option<String> {
    let rejected = out.join("REJECTED.md");
    let text = std::fs::read_to_string(rejected).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// The compile-only check of the drafted pack, as findings. A missing manifest is its own finding
/// so the refine prompt says what actually happened, not a parse error about a file that isn't
/// there.
fn compile_findings(out: &Path) -> Result<Vec<String>> {
    let manifest = out.join(MANIFEST_FILE);
    if !manifest.is_file() {
        return Ok(vec![format!(
            "no {MANIFEST_FILE} in the drafted pack — the turn wrote no manifest"
        )]);
    }
    Ok(crate::cli::check::run_compile_only(&manifest, None)?.findings)
}

fn render_prompt(goal: &str, out_dir: &Path, extending: bool) -> String {
    let context_section = if extending {
        "- `current/` — the draft's current files. Extend them: carry forward every file that\n  \
         should survive unless the intent asks for its removal, and write the new files beside\n  \
         them."
    } else {
        "- `current/` — absent: this is a fresh draft, there are no current files to carry\n  \
         forward."
    };
    DRAFT_PROMPT
        .replace("{{GOAL}}", goal)
        .replace("{{OUT_DIR}}", &out_dir.display().to_string())
        .replace("{{CONTEXT_SECTION}}", context_section)
}

fn render_refine_prompt(goal: &str, out_dir: &Path, findings: &[String], round: u32) -> String {
    DRAFT_REFINE_PROMPT
        .replace("{{GOAL}}", goal)
        .replace("{{OUT_DIR}}", &out_dir.display().to_string())
        .replace("{{FINDINGS}}", &findings.join("\n"))
        .replace("{{ROUND}}", &round.to_string())
}

/// Run one drafting turn in `scratch` and return its outcome. The turn's own narration stays on
/// stderr; stdout carries only the report `run` prints, the contract the controller parses.
fn run_draft_turn(
    scratch: &Path,
    prompt: &str,
    a: &DraftArgs,
    engine: &tokio::runtime::Handle,
) -> Result<crate::agent::turn::TurnOutcome> {
    let mut args = turn_args(a)?;
    args.engine = Some(engine.clone());
    let paths = draft_paths(scratch);
    let _ = std::fs::create_dir_all(&paths.state);
    Ok(agent::run_turn(
        &args,
        &paths,
        prompt,
        false,
        |_line, _stream, ev| {
            // A backend that dies at spawn/auth must be visible in the log, not silently a $0 turn.
            if let Some(AgentEvent::Error {
                error_type,
                message,
            }) = ev
            {
                eprintln!("[crucible draft] agent error ({error_type}): {message}");
            }
        },
    ))
}

/// Drive the drafting loop and return its report. Pure of process exit / printing, [`run`] owns
/// rendering + the exit code, so tests drive this directly.
pub fn execute(a: &DraftArgs, engine: &tokio::runtime::Handle) -> Result<DraftReport> {
    let goal = std::fs::read_to_string(&a.goal_file)
        .with_context(|| format!("reading the intent file {}", a.goal_file.display()))?;
    let goal = goal.trim().to_string();
    if goal.is_empty() {
        return Err(DraftError::EmptyIntent {
            path: a.goal_file.clone(),
        }
        .into());
    }
    crate::scope::pack::ensure_out_dir(&a.out, a.force)?;
    let out = std::fs::canonicalize(&a.out).unwrap_or_else(|_| a.out.clone());

    let scratch = crate::scope::pack::scratch_dir("draft");
    let result = drive(a, &goal, &out, &scratch, engine);
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn drive(
    a: &DraftArgs,
    goal: &str,
    out: &Path,
    scratch: &Path,
    engine: &tokio::runtime::Handle,
) -> Result<DraftReport> {
    write_seed_context(scratch, goal, a.context.as_deref())?;
    std::fs::create_dir_all(scratch.join(DRAFT_WORK_DIR))
        .with_context(|| format!("creating {DRAFT_WORK_DIR} in {}", scratch.display()))?;
    let pack_rel = Path::new(DRAFT_WORK_DIR);
    let rounds_max = a.refine_rounds.max(1);

    let mut total_cost = 0.0;
    let mut rounds = 0;
    let mut findings: Vec<String> = Vec::new();
    let mut declined: Option<String> = None;
    for round in 1..=rounds_max {
        if round > 1 && a.max_cost > 0.0 && total_cost >= a.max_cost {
            findings.push(format!(
                "refinement stopped: ${total_cost:.4} already spent against the --max-cost ${:.2} cap",
                a.max_cost
            ));
            break;
        }
        rounds = round;
        let prompt = if round == 1 {
            render_prompt(goal, pack_rel, a.context.is_some())
        } else {
            render_refine_prompt(goal, pack_rel, &findings, round)
        };
        let turn = run_draft_turn(scratch, &prompt, a, engine)?;
        total_cost += turn.cost_usd;
        sync_pack_from_workspace(scratch, out)?;
        if let Some(reason) = read_rejected(out) {
            declined = Some(reason);
            break;
        }
        findings = compile_findings(out)?;
        if let Some(failure) = turn.failure() {
            // Fail closed: a turn that died mid-round leaves a pack that cannot be trusted as
            // complete even when what it wrote happens to compile.
            findings.push(format!("round {round}'s turn never completed: {failure}"));
        }
        if findings.is_empty() {
            break;
        }
    }

    let over_budget = a.max_cost > 0.0 && total_cost > a.max_cost;
    Ok(DraftReport {
        ok: declined.is_none() && findings.is_empty(),
        cost_usd: total_cost,
        over_budget,
        rounds,
        findings,
        declined,
    })
}

/// CLI entry point: run the loop, print the report (JSON or human lines), and exit nonzero when
/// the final round still fails the check, the agent declined, or the turns ran over `--max-cost`
/// (the `scope` visible-guard shape). The report is printed even on those failure paths — the
/// controller reads it, not the exit code.
pub fn run(a: DraftArgs, engine: tokio::runtime::Handle) -> Result<()> {
    // A controller-dispatched turn: adopt the dispatch as this turn's trace parent. Closed right
    // after the loop, not at fn end, the failure tail exits via `process::exit`, which would skip
    // the drop and lose the span.
    let turn_span = crate::agent::engine::turn_span(
        crate::agent::engine::TurnSpanKind::Draft,
        Some(&a.goal_file.display().to_string()),
    );
    let report = {
        let _turn_guard = turn_span.as_ref().map(tracing::Span::enter);
        execute(&a, &engine)?
    };
    drop(turn_span);

    if a.json {
        let line = serde_json::to_string(&report).context("serializing the draft report")?;
        println!("{line}");
    } else {
        let files = walk_dir(&a.out)
            .map(|pack| pack.tree.files().len())
            .unwrap_or(0);
        match &report.declined {
            Some(reason) => {
                eprintln!("[crucible draft] the drafting agent declined:");
                for line in reason.lines() {
                    eprintln!("  {line}");
                }
            }
            None => {
                println!(
                    "[crucible draft] drafted {files} files into {} ({} round{}, ${:.4})",
                    a.out.display(),
                    report.rounds,
                    if report.rounds == 1 { "" } else { "s" },
                    report.cost_usd
                );
                if report.findings.is_empty() {
                    println!("  check: OK");
                } else {
                    println!("  check findings:");
                    for finding in &report.findings {
                        println!("  - {finding}");
                    }
                }
            }
        }
        if report.over_budget {
            eprintln!(
                "[crucible draft] OVER BUDGET: the turns cost ${:.4} exceeds --max-cost ${:.2}",
                report.cost_usd, a.max_cost
            );
        }
    }

    if !report.ok || report.over_budget {
        // The marker the controller scrapes must survive the exit; flush before it, the exit
        // skips the OTLP layer's drops.
        crate::agent::engine::flush();
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-draft-test-{name}-{}-{}",
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
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, content).expect("write script");
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    fn goal_file(dir: &Path, intent: &str) -> PathBuf {
        let path = dir.join("intent.md");
        fs::write(&path, intent).expect("write intent");
        path
    }

    fn args(dir: &Path, script: &Path) -> DraftArgs {
        DraftArgs {
            goal_file: goal_file(dir, "rank vLLM serving flags by throughput"),
            out: dir.join("pack"),
            context: None,
            max_cost: 5.0,
            refine_rounds: 2,
            force: false,
            json: true,
            agent_backend: AgentBackend::Local,
            sandbox_image: None,
            agent_cmd: Some(script.display().to_string()),
            harness: None,
            model: None,
            compute_driver: crate::openshell::gateway::ComputeDriver::Podman,
        }
    }

    fn execute_args(a: &DraftArgs) -> DraftReport {
        let rt = tokio::runtime::Runtime::new().unwrap();
        execute(a, rt.handle()).expect("the drafting loop runs")
    }

    /// A minimal playbook pack that passes the compile-only check, as shell heredocs the stand-in
    /// can write into `_draft_out`. Split so a test can forget `role.sh` on the drafting round.
    const WRITE_MANIFEST: &str = "mkdir -p _draft_out && cat > _draft_out/crucible.toml <<'EOF'
[workspace]
inject = [\"role.sh\"]
[agent]
backend = \"command\"
agent_cmd = \"sh role.sh\"
goal = \"do the thing\"
[workflow]
type = \"playbook\"
file = \"workflow.star\"
EOF
cat > _draft_out/workflow.star <<'EOF'
one = command(name = \"one\", run = \"sh role.sh\")
workflow(type = \"playbook\", tasks = [one])
EOF
";
    const WRITE_ROLE: &str = "cat > _draft_out/role.sh <<'EOF'
#!/bin/sh
echo done
EOF
";
    fn write_pack_script() -> String {
        format!("{WRITE_MANIFEST}{WRITE_ROLE}")
    }

    #[test]
    fn a_stand_in_drafts_a_pack_that_lands_in_out_and_passes_the_check() {
        let dir = tempdir("lands");
        let script = dir.join("agent.sh");
        write_exec(&script, &format!("#!/bin/sh\n{}", write_pack_script()));
        let a = args(&dir, &script);

        let report = execute_args(&a);

        assert!(report.ok, "findings: {:?}", report.findings);
        assert_eq!(report.rounds, 1);
        assert_eq!(report.cost_usd, 0.0);
        for name in ["crucible.toml", "workflow.star", "role.sh"] {
            assert!(a.out.join(name).is_file(), "{name} did not land in --out");
        }
        // The compile-only slice executes no pack content: the full check would have bootstrapped
        // a workspace/ dir inside the pack.
        assert!(
            !a.out.join("workspace").exists(),
            "the drafting check must not bootstrap a workspace"
        );
    }

    #[test]
    fn a_failed_first_round_drives_a_refine_round_that_fixes_it() {
        let dir = tempdir("refine");
        let script = dir.join("agent.sh");
        // Round 1 writes a manifest whose inject names a file it does not write; the refine
        // prompt (the only round-2 input) carries the finding, and the round writes the file.
        write_exec(
            &script,
            &format!(
                "#!/bin/sh\ncase \"$CRUCIBLE_PROMPT\" in\n  *Refine*) {} ;;\n  *) {} ;;\nesac\n",
                write_pack_script(),
                WRITE_MANIFEST
            ),
        );
        let a = args(&dir, &script);

        let report = execute_args(&a);

        assert!(report.ok, "findings: {:?}", report.findings);
        assert_eq!(report.rounds, 2, "the bad first round must drive a refine");
        assert!(a.out.join("role.sh").is_file());
    }

    #[test]
    fn exhausted_rounds_leave_the_pack_and_its_findings_behind() {
        let dir = tempdir("exhausted");
        let script = dir.join("agent.sh");
        // A manifest that never resolves: the inject names a file no round writes.
        let broken =
            write_pack_script().replace("inject = [\"role.sh\"]", "inject = [\"missing.sh\"]");
        write_exec(
            &script,
            &format!("#!/bin/sh\n{broken}\nrm -f _draft_out/role.sh\n"),
        );
        let mut a = args(&dir, &script);
        a.refine_rounds = 1;

        let report = execute_args(&a);

        assert!(!report.ok);
        assert_eq!(report.rounds, 1);
        assert!(
            report.findings.iter().any(|f| f.contains("missing.sh")),
            "findings: {:?}",
            report.findings
        );
        assert!(
            a.out.join("crucible.toml").is_file(),
            "the pack lands even when the check fails"
        );
    }

    #[test]
    fn a_declined_turn_is_terminal_and_reported() {
        let dir = tempdir("declined");
        let script = dir.join("agent.sh");
        write_exec(
            &script,
            "#!/bin/sh\nmkdir -p _draft_out && echo 'needs a live cluster' > _draft_out/REJECTED.md\n",
        );
        let a = args(&dir, &script);

        let report = execute_args(&a);

        assert_eq!(report.declined.as_deref(), Some("needs a live cluster"));
        assert!(!report.ok);
        assert_eq!(
            report.rounds, 1,
            "a decline is terminal, no refine round runs"
        );
        assert!(
            !a.out.join("crucible.toml").exists(),
            "a declined turn drafts no pack"
        );
    }

    #[test]
    fn a_non_empty_out_dir_is_refused_without_force() {
        let dir = tempdir("refuse");
        let script = dir.join("agent.sh");
        write_exec(&script, "#!/bin/sh\ntrue\n");
        let a = args(&dir, &script);
        fs::create_dir_all(&a.out).unwrap();
        fs::write(a.out.join("prior.txt"), "here").unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = execute(&a, rt.handle()).expect_err("the non-empty out dir must refuse");

        assert!(
            err.to_string().contains("--force"),
            "the refusal names the escape: {err:#}"
        );
    }

    #[test]
    fn the_intent_file_must_hold_something() {
        let dir = tempdir("empty-intent");
        let script = dir.join("agent.sh");
        write_exec(&script, "#!/bin/sh\ntrue\n");
        let mut a = args(&dir, &script);
        a.goal_file = goal_file(&dir, "   \n");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = execute(&a, rt.handle()).expect_err("an empty intent must refuse");

        assert!(
            err.to_string().contains("empty"),
            "the refusal says why: {err:#}"
        );
    }
}

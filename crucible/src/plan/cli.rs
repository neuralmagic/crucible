//! The `crucible plan` CLI: `show` compiles and prints a plan; `run` executes one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::plan::exec::{Substrate, runnable_set};
use crate::plan::ir::{Direction, Plan, TaskKind, ValidPlan};
use xai_grok_mermaid::{MermaidTheme, RenderLimits, RenderParams, default_engine, render_checked};

#[derive(Debug, thiserror::Error)]
#[error("mermaid render failed: {detail}")]
struct MermaidRenderFailed {
    detail: String,
}

#[derive(Debug, thiserror::Error)]
#[error("plan did not reach a valid verdict ({exit})")]
struct NoValidVerdict {
    exit: String,
}

const MERMAID_COMMAND_PREVIEW_CHARS: usize = 72;

/// Compile scope-time workflow authoring syntax. JSON on stdout is stable enough for a
/// checked-in golden; `--manifest` additionally materializes the runtime TOML authority. A
/// source that declares params materializes as a file reference instead of frozen tasks, since
/// its graph is a function of arguments no materialization has.
pub fn compile_workflow(
    file: &Path,
    manifest: Option<&Path>,
    params: &BTreeMap<String, String>,
) -> Result<()> {
    let compiled = match manifest {
        Some(manifest) => crate::plan::starlark::materialize_manifest(file, manifest, params)?,
        None => crate::plan::starlark::compile_file_with(
            file,
            crate::plan::starlark::parent_or_cwd(file),
            params,
        )?,
    };
    for prompt_file in &compiled.prompt_files {
        eprintln!("embedded prompt: {}", prompt_file.display());
    }
    print!("{}", compiled.canonical_json);
    Ok(())
}

/// Read TOML (`.toml`) or JSON (anything else: the `PLAN.json` sentinel shape),
/// validate, and return the frozen plan.
pub fn load(path: &Path) -> Result<ValidPlan> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading plan {}", path.display()))?;
    let plan = if path.extension().is_some_and(|e| e == "toml") {
        Plan::from_toml_str(&src)?
    } else {
        Plan::from_json_str(&src)?
    };
    plan.validate()
        .with_context(|| format!("validating plan {}", path.display()))
}

/// Render the compiled plan: tasks in dependency-first order, plus the truncation verdict
/// for the given substrate caps (fail-closed preview of what `execute` would refuse).
pub fn render(plan: &ValidPlan, caps: &BTreeSet<String>) -> String {
    let p = plan.plan();
    let mut out = format!(
        "plan v{} — {} tasks, budget ${}\n",
        p.version,
        p.tasks.len(),
        p.budget.usd
    );
    if let Some(reason) = &p.reason {
        out.push_str(&format!("reason: {reason}\n"));
    }
    let runnable = runnable_set(plan, &Substrate { caps: caps.clone() });
    for t in plan.tasks_topo() {
        let ok = runnable.contains(&t.name);
        let deps = if t.depends_on.is_empty() {
            "-".to_string()
        } else {
            t.depends_on
                .iter()
                .map(|d| d.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut detail = match &t.task {
            TaskKind::Agent { model, harness, .. } => format!(
                "agent[{}/{}]",
                harness.as_deref().unwrap_or("default"),
                model.as_deref().unwrap_or("default")
            ),
            TaskKind::Command { command } => format!("command[{command}]"),
            TaskKind::Evaluate {
                command,
                threshold,
                direction,
            } => format!(
                "evaluate[{command}]{}",
                threshold
                    .zip(*direction)
                    .map(|(value, direction)| format!(" {direction:?} {value}"))
                    .unwrap_or_default()
            ),
            TaskKind::TopK { k, .. } => format!("top_k[k={k}]"),
            TaskKind::Engine { .. } => t.task.label().to_string(),
        };
        if let Some(session) = &t.session {
            detail.push_str(&format!(" session={session}"));
        }
        out.push_str(&format!(
            "  {:<20} {:<28} needs={:<8} {} deps: {}{}\n",
            t.name.0,
            detail,
            t.needs,
            if t.required { "required" } else { "advisory" },
            deps,
            if ok { "" } else { "  [UNRUNNABLE]" },
        ));
    }
    match plan
        .tasks_topo()
        .find(|t| t.required && !runnable.contains(&t.name))
    {
        Some(t) => out.push_str(&format!(
            "verdict: TRUNCATED — required task {} unrunnable with caps [{}]; execute would \
             refuse fail-closed\n",
            t.name,
            caps.iter().cloned().collect::<Vec<_>>().join(", ")
        )),
        None => out.push_str("verdict: runnable\n"),
    }
    out
}

/// Fill and text color per task kind, shared by both styling forms.
const CLASS_STYLES: [(&str, &str); 6] = [
    ("agent", "fill:#458588,color:#fbf1c7"),
    ("command", "fill:#98971a,color:#282828"),
    (
        "evaluate",
        "fill:#076678,color:#fbf1c7,stroke:#83a598,stroke-width:2px",
    ),
    (
        "grade",
        "fill:#d65d0e,color:#fbf1c7,stroke:#fe8019,stroke-width:3px",
    ),
    ("reduce", "fill:#d79921,color:#282828"),
    ("engine", "fill:#b16286,color:#fbf1c7"),
];

/// How node styling is spelled in the emitted source.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Styling {
    /// Idiomatic mermaid, and what GitHub and any UI graph view expect.
    ClassDef,
    /// A `style <id>` line per node. The vendored engine parses neither `classDef` nor the
    /// `:::` suffix, and drops the label of any node carrying one.
    PerNode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metadata {
    /// Omit task commands from pasteable Mermaid.
    Public,
    /// Include bounded command summaries in previews.
    Preview,
}

/// Render the compiled plan as mermaid flowchart source: pipeable into a terminal mermaid
/// renderer, pasteable into GitHub markdown, and the same source a UI graph view consumes.
pub fn render_mermaid(plan: &ValidPlan, caps: &BTreeSet<String>) -> String {
    render_mermaid_styled(plan, caps, Styling::ClassDef, Metadata::Public)
}

fn render_mermaid_styled(
    plan: &ValidPlan,
    caps: &BTreeSet<String>,
    styling: Styling,
    metadata: Metadata,
) -> String {
    let runnable = runnable_set(plan, &Substrate { caps: caps.clone() });
    let mut out = String::from("flowchart TD\n");
    let mut styles = String::new();
    let ids: BTreeMap<_, _> = plan
        .tasks_topo()
        .enumerate()
        .map(|(i, t)| (t.name.clone(), format!("t{i}")))
        .collect();
    let mut regular_nodes = Vec::new();
    let mut measurement_nodes = Vec::new();
    let mut edges = Vec::new();
    for t in plan.tasks_topo() {
        let ok = runnable.contains(&t.name);
        let (shape_open, shape_close, (class, props)) = match &t.task {
            TaskKind::Agent { .. } => ("([", "])", CLASS_STYLES[0]),
            TaskKind::Command { .. } => ("[", "]", CLASS_STYLES[1]),
            TaskKind::Evaluate { .. } => ("([", "])", CLASS_STYLES[2]),
            TaskKind::Engine {
                op: crate::plan::ir::EngineOp::Grade,
                ..
            } => ("{{", "}}", CLASS_STYLES[3]),
            TaskKind::TopK { .. } => ("{{", "}}", CLASS_STYLES[4]),
            TaskKind::Engine { .. } => ("[[", "]]", CLASS_STYLES[5]),
        };
        let mut detail = match &t.task {
            TaskKind::Agent { harness, model, .. } => format!(
                "<br/>{}/{}",
                mermaid_label(harness.as_deref().unwrap_or("default")),
                mermaid_label(model.as_deref().unwrap_or("default"))
            ),
            TaskKind::Command { command } if metadata == Metadata::Preview => {
                format!("<br/>run: {}", mermaid_command_preview(command))
            }
            TaskKind::Evaluate {
                command,
                threshold,
                direction,
            } if metadata == Metadata::Preview => {
                let mut detail = format!("<br/>run: {}", mermaid_command_preview(command));
                if let (Some(threshold), Some(direction)) = (threshold, direction) {
                    let comparison = match direction {
                        Direction::Lower => "&lt;=",
                        Direction::Higher => "&gt;=",
                    };
                    detail.push_str(&format!("<br/>pass: score {comparison} {threshold}"));
                }
                detail
            }
            TaskKind::Command { .. } | TaskKind::Evaluate { .. } | TaskKind::Engine { .. } => {
                String::new()
            }
            TaskKind::TopK { k, .. } => format!("<br/>k={k}"),
        };
        if let Some(session) = &t.session {
            detail.push_str(&format!("<br/>session: {}", mermaid_label(session)));
        }
        let marks = format!(
            "{}{}",
            if t.required { "" } else { " (advisory)" },
            if ok { "" } else { " ⛔" }
        );
        let id = &ids[&t.name];
        let class_suffix = match styling {
            Styling::ClassDef => format!(":::{class}"),
            Styling::PerNode => String::new(),
        };
        let node = format!(
            "    {id}{shape_open}\"{name}{detail}{marks}\"{shape_close}{class_suffix}\n",
            name = mermaid_label(&t.name.0),
        );
        if styling == Styling::PerNode {
            styles.push_str(&format!("    style {id} {props}\n"));
        }
        if matches!(
            t.task,
            TaskKind::Evaluate { .. }
                | TaskKind::Engine {
                    op: crate::plan::ir::EngineOp::Grade,
                    ..
                }
        ) {
            measurement_nodes.push(node);
        } else {
            regular_nodes.push(node);
        }
        for d in &t.depends_on {
            edges.push(format!("    {} --> {}\n", ids[d], ids[&t.name]));
        }
    }
    for node in regular_nodes {
        out.push_str(&node);
    }
    if !measurement_nodes.is_empty() {
        out.push_str("    subgraph measurement[\"Measurement\"]\n        direction TD\n");
        for node in measurement_nodes {
            out.push_str("    ");
            out.push_str(&node);
        }
        out.push_str("    end\n");
    }
    for edge in edges {
        out.push_str(&edge);
    }
    match styling {
        Styling::ClassDef => {
            for (name, props) in CLASS_STYLES {
                out.push_str(&format!("    classDef {name} {props}\n"));
            }
        }
        Styling::PerNode => out.push_str(&styles),
    }
    out
}

/// Serialize an admitted plan for the event stream.
/// The `Shutdown.outcome` token a finished plan reports, in the vocabulary
/// [`crate::recovery::ShutdownOutcome`] parses. Only a plan that ran its whole graph is
/// `finished`.
fn shutdown_outcome(out: &crate::plan::exec::PlanOutcome) -> &'static str {
    use crate::plan::exec::PlanExit;
    if out.valid {
        return "finished";
    }
    match out.exit {
        PlanExit::BudgetExceeded | PlanExit::TimeExceeded => "budget",
        PlanExit::Completed | PlanExit::Truncated { .. } | PlanExit::ShortCircuit { .. } => "error",
    }
}

pub(crate) fn plan_admitted_event(plan: &ValidPlan) -> crate::session::SessionEvent {
    let p = plan.plan();
    crate::session::SessionEvent::PlanAdmitted {
        plan_version: p.version,
        reason: p.reason.clone().unwrap_or_default(),
        budget_usd: p.budget.usd,
        tasks: plan
            .tasks_topo()
            .map(|t| crate::session::PlanTaskWire {
                name: t.name.0.clone(),
                kind: t.task.label().to_string(),
                depends_on: t.depends_on.iter().map(|d| d.0.clone()).collect(),
                session: t.session.clone().unwrap_or_default(),
                needs: t.needs.clone(),
                required: t.required,
                join: t.join.as_str().to_string(),
                stage: t.stage.as_str().to_string(),
                over: t
                    .over
                    .as_ref()
                    .map(crate::plan::ir::OutputRef::to_string)
                    .unwrap_or_default(),
                max_fanout: t.max_fanout.unwrap_or_default(),
            })
            .collect(),
    }
}

/// One terminal task result on the wire. `iter` is the loop round (0 for a standalone
/// `plan run`); fields belonging to other emitters stay at their defaults. `trace_id`/`span_id`
/// carry the emitter's current trace context (the iteration's span) so a RESULTS row links
/// straight to its trace; no active span leaves them empty.
pub(crate) fn task_result_event(
    plan_version: u32,
    iter: u32,
    task: &crate::plan::ir::Task,
    r: &crate::plan::exec::TaskResult,
) -> crate::session::SessionEvent {
    let (trace_id, span_id) = crate::engine::current_trace_env()
        .and_then(|(tp, _)| {
            let f: Vec<&str> = tp.split('-').collect();
            match f.as_slice() {
                [_, tid, sid, ..] => Some((tid.to_string(), sid.to_string())),
                _ => None,
            }
        })
        .unwrap_or_default();
    crate::session::SessionEvent::TaskResult {
        task: task.name.0.clone(),
        status: r.status.as_str().to_string(),
        plan_version,
        task_kind: task.task.label().to_string(),
        iter,
        digest: String::new(),
        job: String::new(),
        attempts: r.attempts,
        cost_usd: r.cost_usd,
        metric: None,
        output: r.output.clone(),
        note: r.note.clone().unwrap_or_default(),
        secs: 0.0,
        trace_id,
        span_id,
    }
}

fn mermaid_label(name: &str) -> String {
    name.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\r', '\n'], " ")
}

fn mermaid_command_preview(command: &str) -> String {
    let compact = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = compact.chars();
    let mut preview: String = chars.by_ref().take(MERMAID_COMMAND_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    mermaid_label(&preview)
}

pub fn show(path: &Path, caps: &BTreeSet<String>, mermaid: bool, render_img: bool) -> Result<()> {
    let plan = load(path)?;
    if render_img {
        return show_rendered(path, &plan, caps);
    }
    if mermaid {
        print!("{}", render_mermaid(&plan, caps));
    } else {
        print!("{}", render(&plan, caps));
    }
    Ok(())
}

/// Label size the preview layout asks for, in SVG px; the render scale derives from it.
const PREVIEW_FONT_PX: f32 = 12.0;

/// Roughly how much of a cell's height a terminal glyph occupies.
const GLYPH_FRACTION_OF_CELL: f32 = 0.8;

/// Render the plan's mermaid to PNG (offline, vendored engine) and display it inline when
/// the terminal speaks an image protocol; otherwise write `<plan>.png` next to the file.
fn show_rendered(path: &Path, plan: &ValidPlan, caps: &BTreeSet<String>) -> Result<()> {
    const THEME: MermaidTheme = MermaidTheme::Dark;
    let inline = crate::plan::term_img::detect().zip(crate::plan::term_img::geometry());

    // Match diagram text to terminal glyphs; let deep graphs scroll.
    let params = match inline {
        Some((_, geo)) => RenderParams {
            theme: THEME,
            target_width_px: 0,
            scale: (geo.cell_height_px * GLYPH_FRACTION_OF_CELL / PREVIEW_FONT_PX).clamp(1.0, 4.0),
            max_height_px: 0,
            ..RenderParams::default()
        },
        None => RenderParams::for_os_viewer(THEME, 1600, 0),
    };
    let diagram = render_png(plan, caps, &params)?;

    match inline {
        Some((proto, geo)) => {
            let fit_cols = (diagram.width_px > geo.width_px).then_some(geo.cols);
            print!(
                "{}",
                crate::plan::term_img::emit(proto, &diagram.png, fit_cols)
            );
        }
        None => {
            let out = path.with_extension("png");
            std::fs::write(&out, &diagram.png)
                .with_context(|| format!("writing {}", out.display()))?;
            println!(
                "terminal has no inline-image protocol; wrote {} ({}x{})",
                out.display(),
                diagram.width_px,
                diagram.height_px
            );
        }
    }
    Ok(())
}

/// Rasterize with per-node styles supported by the vendored engine.
fn render_png(
    plan: &ValidPlan,
    caps: &BTreeSet<String>,
    params: &RenderParams,
) -> Result<xai_grok_mermaid::RenderedDiagram> {
    // Compact layout used only for raster previews; the font size must stay the one the
    // render scale is derived from.
    let src = format!(
        "---
config:
  fontSize: {PREVIEW_FONT_PX}
  flowchart:
    nodeSpacing: 30
    rankSpacing: 30
    padding: 8
    wrappingWidth: 180
---
{}",
        render_mermaid_styled(plan, caps, Styling::PerNode, Metadata::Preview)
    );
    render_checked(
        default_engine().as_ref(),
        &src,
        params,
        &RenderLimits::default(),
    )
    .map_err(|e| {
        MermaidRenderFailed {
            detail: e.to_string(),
        }
        .into()
    })
}

/// Render a validated graph to PNG.
pub fn render_png_to(
    plan: &ValidPlan,
    caps: &BTreeSet<String>,
    output: &Path,
) -> Result<(u32, u32)> {
    let params = RenderParams::for_os_viewer(MermaidTheme::Dark, 1600, 0);
    let diagram = render_png(plan, caps, &params)?;
    std::fs::write(output, &diagram.png)
        .with_context(|| format!("writing workflow graph {}", output.display()))?;
    Ok((diagram.width_px, diagram.height_px))
}

/// Compile and execute a plan: real subprocesses, real outputs, the executor's real
/// semantics. With `--manifest`, agent tasks run through the real harness path; otherwise
/// the shell runner handles everything (`--agent-cmd` as the agent stand-in). Exits nonzero
/// when the plan is not valid.
/// What the launcher supplies. A playbook's source may not declare either, so a pack cannot
/// raise a limit its operator set; the engine refuses to dispatch one that arrives without both.
#[derive(Debug, Clone, Default)]
pub struct Ceilings {
    pub usd: Option<f64>,
    pub wall_clock: Option<std::time::Duration>,
    /// What the operator typed, so a rejection can quote it back rather than say "invalid".
    pub wall_clock_raw: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "a playbook needs {missing} from whoever launches it. Its source may not declare a limit its \
     operator set, so the engine will not dispatch a task without one."
)]
struct MissingCeiling {
    missing: String,
}

#[derive(Debug, thiserror::Error)]
#[error("--max-time {raw:?} is not a duration (try `90s`, `30m`, `2h`)")]
struct BadDuration {
    raw: String,
}

#[derive(Debug, thiserror::Error)]
#[error("{manifest} declares no [workflow]; pass --file, or give the manifest a graph to compile")]
struct NoGraph {
    manifest: String,
}

#[derive(Debug, thiserror::Error)]
#[error("--param {got:?} is not name=value")]
struct BadParam {
    got: String,
}

/// Split `name=value` pairs. The value may contain `=`; the name may not, so the first one wins.
pub fn parse_params(pairs: &[String]) -> Result<BTreeMap<String, String>> {
    pairs
        .iter()
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) if !name.is_empty() => Ok((name.to_string(), value.to_string())),
            _ => Err(BadParam { got: pair.clone() }.into()),
        })
        .collect()
}

pub fn run(
    path: Option<&Path>,
    params: &BTreeMap<String, String>,
    caps: &BTreeSet<String>,
    agent_cmd: Option<String>,
    manifest: Option<&Path>,
    ceilings: Ceilings,
) -> Result<()> {
    use crate::plan::exec::{ExecCfg, PlanExit, TaskRunner, execute};
    use crate::plan::runner::ShellRunner;

    if let (None, Some(raw)) = (ceilings.wall_clock, ceilings.wall_clock_raw.as_ref()) {
        return Err(BadDuration { raw: raw.clone() }.into());
    }

    // Either the plan was handed to us, or the manifest names the graph and we compile it.
    let mut evidence: Option<crate::Paths> = None;
    let (plan, mut runner, events): (ValidPlan, Box<dyn TaskRunner>, Option<std::fs::File>) =
        match (path, manifest) {
            (_, Some(m)) => {
                let (prepared, loaded) = crate::run::prep_plan_runner_with_params(m, params)?;
                let session_log = prepared.paths.session_log.clone();
                evidence = Some(prepared.paths.clone());
                let playbook = loaded
                    .workflow
                    .as_ref()
                    .is_some_and(|w| w.workflow_type == crate::manifest::WorkflowType::Playbook);
                if playbook {
                    let missing = match (ceilings.usd, ceilings.wall_clock) {
                        (None, None) => Some("--max-cost and --max-time"),
                        (None, Some(_)) => Some("--max-cost"),
                        (Some(_), None) => Some("--max-time"),
                        (Some(_), Some(_)) => None,
                    };
                    if let Some(missing) = missing {
                        return Err(MissingCeiling {
                            missing: missing.to_string(),
                        }
                        .into());
                    }
                }
                let mut plan = match path {
                    Some(p) => load(p)?,
                    None => {
                        let workflow = loaded.workflow.as_ref().ok_or_else(|| NoGraph {
                            manifest: m.display().to_string(),
                        })?;
                        crate::loop_graph::iteration_template(
                            Some(workflow),
                            &crate::manifest::WorkflowCaps::for_lane(workflow.workflow_type)
                                .with_persistent_sessions(),
                        )?
                    }
                };
                if let Some(usd) = ceilings.usd {
                    plan = plan.with_budget(usd)?;
                }
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&session_log)
                    .with_context(|| format!("opening {}", session_log.display()))?;
                (plan, Box::new(prepared), Some(f))
            }
            (Some(p), None) => {
                let mut plan = load(p)?;
                if let Some(usd) = ceilings.usd {
                    plan = plan.with_budget(usd)?;
                }
                (
                    plan,
                    Box::new(ShellRunner {
                        workdir: std::env::current_dir()
                            .context("resolving the working directory")?,
                        agent_cmd,
                    }),
                    None,
                )
            }
            (None, None) => unreachable!("clap requires --file without --manifest"),
        };
    // Manifest runs append plan wire events to the run's session log so tailers (and the
    // controller's ingest) see the graph and its live progress; shell runs have no state dir.
    let substrate = Substrate { caps: caps.clone() };
    let append = |f: &std::fs::File, ev: &crate::session::SessionEvent| {
        use std::io::Write;
        let mut w = f;
        let _ = writeln!(w, "{}", crate::session::encode(ev));
    };
    if let Some(f) = &events {
        append(f, &plan_admitted_event(&plan));
    }
    let out = execute(
        &plan,
        &substrate,
        ExecCfg {
            wall_clock: ceilings.wall_clock,
            ..ExecCfg::default()
        },
        runner.as_mut(),
        |task, result| {
            if let Some(f) = &events {
                append(f, &task_result_event(plan.plan().version, 0, task, result));
            }
        },
    );
    for t in plan.tasks_topo() {
        if let Some(r) = out.results.get(&t.name) {
            println!(
                "  {:<20} {:<10} attempts={} cost=${:.4}{}{}",
                t.name.0,
                r.status.as_str(),
                r.attempts,
                r.cost_usd,
                r.output
                    .as_ref()
                    .map(|v| format!("  out={v}"))
                    .unwrap_or_default(),
                r.note
                    .as_ref()
                    .map(|n| format!("  ({n})"))
                    .unwrap_or_default(),
            );
        }
    }
    let exit = match &out.exit {
        PlanExit::Completed => "completed".to_string(),
        PlanExit::Truncated { task } => format!("truncated at {task}"),
        PlanExit::ShortCircuit { task } => format!("short-circuited at {task}"),
        PlanExit::BudgetExceeded => "budget exceeded".to_string(),
        PlanExit::TimeExceeded => "wall-clock ceiling reached".to_string(),
    };
    if let Some(f) = &events {
        append(
            f,
            &crate::session::SessionEvent::Shutdown {
                outcome: shutdown_outcome(&out).to_string(),
                reason: exit.clone(),
            },
        );
    }
    if let Some(p) = &evidence {
        crate::run::deliver_run_evidence(p);
    }
    println!(
        "plan v{}: {} — spent ${:.4} of ${}",
        plan.plan().version,
        exit,
        out.spent_usd,
        plan.plan().budget.usd
    );
    if !out.valid {
        return Err(NoValidVerdict {
            exit: exit.to_string(),
        }
        .into());
    }
    println!("verdict: valid");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
        version = 1
        [budget]
        usd = 2.0
        [[task]]
        name = "propose"
        kind = "agent"
        prompt = "go"
        [[task]]
        name = "measure"
        kind = "command"
        command = "bench.sh"
        depends_on = ["propose"]
        needs = "gpu"
    "#;

    /// How a pack spells its graph is not a launcher's business. An undeclared `--param` is a
    /// mistake against the inline spelling exactly as it is against the file-backed one, and
    /// neither run reaches a task.
    #[test]
    fn an_undeclared_parameter_refuses_both_spellings_of_the_same_pack() {
        let dir =
            std::env::temp_dir().join(format!("crucible-param-parity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sentinel = dir.join("dispatched");
        let head = format!(
            "[repo]\npath = \".\"\n[agent]\nbackend = \"command\"\nagent_cmd = \"true\"\ngoal = \"g\"\n[workspace]\ndir = \"{}\"\n",
            dir.join("workspace").display()
        );
        let run_with_undeclared = |manifest: &Path| {
            run(
                None,
                &BTreeMap::from([("nope".to_string(), "1".to_string())]),
                &BTreeSet::new(),
                None,
                Some(manifest),
                Ceilings {
                    usd: Some(1.0),
                    wall_clock: Some(std::time::Duration::from_secs(60)),
                    wall_clock_raw: Some("60s".to_string()),
                },
            )
            .expect_err("an undeclared parameter is a mistake either way")
        };

        let inline = dir.join("inline.toml");
        std::fs::write(
            &inline,
            format!(
                "{head}[workflow]\ntype = \"playbook\"\n[[workflow.task]]\nname = \"a\"\nkind = \"command\"\ncommand = \"touch {}\"\n",
                sentinel.display()
            ),
        )
        .unwrap();
        let inline_error = format!("{:#}", run_with_undeclared(&inline));

        std::fs::write(
            dir.join("workflow.star"),
            format!(
                "workflow(type = \"playbook\", tasks = [command(name = \"a\", run = \"touch {}\")])\n",
                sentinel.display()
            ),
        )
        .unwrap();
        let backed = dir.join("backed.toml");
        std::fs::write(
            &backed,
            format!("{head}[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n"),
        )
        .unwrap();
        let backed_error = format!("{:#}", run_with_undeclared(&backed));

        assert!(inline_error.contains("nope"), "{inline_error}");
        assert!(backed_error.contains("nope"), "{backed_error}");
        assert!(!sentinel.exists(), "neither run may dispatch a task");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--file` is a compiled plan, which binds nothing; only a `--manifest` run compiles a
    /// graph. Refusing at parse time keeps a supplied value from being dropped downstream.
    #[test]
    fn a_precompiled_plan_takes_no_parameters() {
        use clap::Parser;
        let parse = |args: &[&str]| <crate::Cli>::try_parse_from(args).is_ok();
        assert!(
            !parse(&[
                "crucible", "plan", "run", "--file", "p.toml", "--param", "a=b"
            ]),
            "a compiled plan cannot bind parameters"
        );
        assert!(
            parse(&[
                "crucible",
                "plan",
                "run",
                "--manifest",
                "m.toml",
                "--param",
                "a=b"
            ]),
            "a manifest run compiles its graph"
        );
        assert!(
            parse(&[
                "crucible",
                "plan",
                "compile-workflow",
                "--file",
                "s.star",
                "--param",
                "a=b",
            ]),
            "compile-workflow's file is the source, not a plan"
        );
    }

    #[test]
    fn render_flags_truncation_without_caps() {
        let plan = Plan::from_toml_str(SRC).unwrap().validate().unwrap();
        let out = render(&plan, &BTreeSet::new());
        assert!(out.contains("[UNRUNNABLE]"));
        assert!(out.contains("verdict: TRUNCATED"));
    }

    #[test]
    fn render_runnable_with_caps() {
        let plan = Plan::from_toml_str(SRC).unwrap().validate().unwrap();
        let caps: BTreeSet<String> = ["gpu".to_string()].into();
        let out = render(&plan, &caps);
        assert!(!out.contains("UNRUNNABLE"));
        assert!(out.contains("verdict: runnable"));
    }

    #[test]
    fn mermaid_render_has_nodes_edges_and_truncation_marks() {
        let plan = Plan::from_toml_str(SRC).unwrap().validate().unwrap();
        let out = render_mermaid(&plan, &BTreeSet::new());
        assert!(out.starts_with("flowchart TD\n"));
        assert!(out.contains(r#"t0(["propose"#), "agent node shape: {out}");
        assert!(out.contains("t0 --> t1"), "edge: {out}");
        assert!(
            out.contains('⛔'),
            "gpu-gated task marked unrunnable: {out}"
        );
        let with_caps = render_mermaid(&plan, &["gpu".to_string()].into());
        assert!(!with_caps.contains('⛔'));
    }

    #[test]
    fn mermaid_uses_distinct_internal_ids_for_similar_task_names() {
        let src = r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "review/a"
            kind = "command"
            command = "true"
            [[task]]
            name = "review-a"
            kind = "command"
            command = "true"
        "#;
        let plan = Plan::from_toml_str(src).unwrap().validate().unwrap();
        let out = render_mermaid(&plan, &BTreeSet::new());
        assert!(out.contains("t0[\"review/a\"]"));
        assert!(out.contains("t1[\"review-a\"]"));
        assert!(!out.contains("run:"), "public source omits commands: {out}");
    }

    #[test]
    fn classdef_styling_is_the_pasteable_default() {
        let plan = Plan::from_toml_str(SRC).unwrap().validate().unwrap();
        let out = render_mermaid(&plan, &BTreeSet::new());
        assert!(out.contains(":::agent"), "class suffix: {out}");
        assert!(
            out.contains("classDef agent fill:#458588,color:#fbf1c7"),
            "classDef trailer: {out}"
        );
        assert!(!out.contains("style t0"), "no per-node styles: {out}");
    }

    #[test]
    fn per_node_styling_drops_the_suffix_the_preview_engine_cannot_parse() {
        let plan = Plan::from_toml_str(SRC).unwrap().validate().unwrap();
        let out =
            render_mermaid_styled(&plan, &BTreeSet::new(), Styling::PerNode, Metadata::Preview);
        assert!(!out.contains(":::"), "no class suffix: {out}");
        assert!(!out.contains("classDef"), "no classDef trailer: {out}");
        assert!(out.contains(r#"t0(["propose"#), "label survives: {out}");
        assert!(
            out.contains("style t0 fill:#458588,color:#fbf1c7"),
            "per-node style: {out}"
        );
    }

    #[test]
    fn both_styling_forms_agree_on_nodes_and_edges() {
        let plan = Plan::from_toml_str(SRC).unwrap().validate().unwrap();
        let strip = |s: String| {
            s.lines()
                .filter(|l| {
                    let l = l.trim();
                    !l.starts_with("classDef ") && !l.starts_with("style ")
                })
                .map(|l| l.split(":::").next().unwrap_or(l).to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            strip(render_mermaid(&plan, &BTreeSet::new())),
            strip(render_mermaid_styled(
                &plan,
                &BTreeSet::new(),
                Styling::PerNode,
                Metadata::Public,
            ))
        );
    }

    #[test]
    fn measurement_fanout_is_grouped_and_renders_to_png() {
        let src = r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "apply"
            kind = "engine"
            op = "apply"
            [[task]]
            name = "correctness"
            kind = "evaluate"
            command = "./correctness.sh"
            depends_on = ["apply"]
            [[task]]
            name = "latency"
            kind = "evaluate"
            command = "./latency.sh"
            threshold = 12.5
            direction = "lower"
            depends_on = ["correctness"]
            isolation = "worktree"
            [[task]]
            name = "racecheck"
            kind = "evaluate"
            command = "./racecheck.sh"
            depends_on = ["correctness"]
            isolation = "worktree"
            required = false
            [[task]]
            name = "grade"
            kind = "engine"
            op = "grade"
            source = "latency"
            depends_on = ["latency", "racecheck"]
            join = "passed"
        "#;
        let plan = Plan::from_toml_str(src).unwrap().validate().unwrap();
        let mermaid = render_mermaid(&plan, &BTreeSet::new());
        assert!(mermaid.contains("subgraph measurement[\"Measurement\"]"));
        assert!(mermaid.contains(":::evaluate"), "{mermaid}");
        assert!(mermaid.contains(":::grade"), "{mermaid}");
        assert!(
            mermaid.contains("classDef evaluate fill:#076678"),
            "{mermaid}"
        );
        assert!(
            !mermaid.contains("run:"),
            "public source omits commands: {mermaid}"
        );
        assert!(mermaid.contains("t1 --> t2"), "rung edge: {mermaid}");
        assert!(mermaid.contains("t1 --> t3"), "parallel fanout: {mermaid}");

        let raster =
            render_mermaid_styled(&plan, &BTreeSet::new(), Styling::PerNode, Metadata::Preview);
        assert!(!raster.contains(":::"), "{raster}");
        assert!(raster.contains("style t1 fill:#076678"), "{raster}");
        assert!(raster.contains("style t4 fill:#d65d0e"), "{raster}");
        assert!(raster.contains("correctness<br/>run: ./correctness.sh"));
        assert!(raster.contains("latency<br/>run: ./latency.sh<br/>pass: score &lt;= 12.5"));

        let output = std::env::temp_dir().join(format!(
            "crucible-measurement-render-{}.png",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&output);
        let (width, height) = render_png_to(&plan, &BTreeSet::new(), &output).unwrap();
        let png = std::fs::read(&output).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(width > 0 && height > 0);
        let _ = std::fs::remove_file(output);
    }

    #[test]
    fn mermaid_command_metadata_is_compact_escaped_and_truncated() {
        let command = format!("printf   '<unsafe> & ready'\n{}", "x".repeat(100));
        let preview = mermaid_command_preview(&command);

        assert!(!preview.contains('\n'));
        assert!(preview.contains("&lt;unsafe&gt; &amp; ready"));
        assert!(preview.ends_with('…'));
        assert_eq!(preview.matches("  ").count(), 0);
    }

    #[test]
    fn wire_events_round_trip_through_the_contract_codec() {
        let plan = Plan::from_toml_str(SRC).unwrap().validate().unwrap();
        let admitted = plan_admitted_event(&plan);
        let back = crate::session::decode(&crate::session::encode(&admitted)).unwrap();
        match back {
            crate::session::SessionEvent::PlanAdmitted {
                plan_version,
                tasks,
                ..
            } => {
                assert_eq!(plan_version, 1);
                assert_eq!(tasks.len(), 2);
                assert_eq!(tasks[0].kind, "agent");
                assert_eq!(tasks[1].depends_on, vec!["propose".to_string()]);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let t = plan.get(&"measure".into()).unwrap();
        let r = crate::plan::exec::TaskResult {
            status: crate::plan::exec::TaskStatus::Pass,
            attempts: 1,
            cost_usd: 0.25,
            output: Some(serde_json::json!({"score": 3})),
            note: None,
            fanout: None,
        };
        let back = crate::session::decode(&crate::session::encode(&task_result_event(1, 0, t, &r)))
            .unwrap();
        match back {
            crate::session::SessionEvent::TaskResult {
                task,
                status,
                task_kind,
                cost_usd,
                output,
                ..
            } => {
                assert_eq!(task, "measure");
                assert_eq!(status, "pass");
                assert_eq!(task_kind, "command");
                assert_eq!(cost_usd, 0.25);
                assert_eq!(output.unwrap()["score"], 3);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn load_rejects_invalid_plan_files() {
        let dir = std::env::temp_dir().join("crucible-test-plan-cli");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("bad.toml");
        std::fs::write(&path, "version = 1\n[budget]\nusd = 1.0\n").unwrap();
        let err = load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("no tasks"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Process-global env vars, removed on drop so a panicking assertion cannot leak the drop-box
    /// URL into every later test in this process. Pair with [`crate::test_env_lock`].
    struct ScopedEnv(Vec<&'static str>);

    impl ScopedEnv {
        fn set(vars: &[(&'static str, String)]) -> Self {
            for (k, v) in vars {
                unsafe { std::env::set_var(k, v) };
            }
            Self(vars.iter().map(|(k, _)| *k).collect())
        }
    }

    impl Drop for ScopedEnv {
        fn drop(&mut self) {
            for k in &self.0 {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    /// Read one HTTP POST off `s`, answer 200, and return `(path, body)`. The drop-box client is
    /// real `reqwest`, so the receiver has to be a real socket.
    fn read_one_post(s: &mut std::net::TcpStream) -> (String, Vec<u8>) {
        use std::io::{Read, Write};
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        let (head_end, len) = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .expect("a content-length");
                if buf.len() >= i + 4 + len {
                    break (i, len);
                }
            }
            let n = s.read(&mut chunk).expect("read");
            assert!(n > 0, "the client hung up mid-request");
            buf.extend_from_slice(&chunk[..n]);
        };
        let path = String::from_utf8_lossy(&buf[..head_end])
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .expect("a request line")
            .to_string();
        let body = buf[head_end + 4..head_end + 4 + len].to_vec();
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .expect("respond");
        let _ = s.flush();
        (path, body)
    }

    /// The ingest contract a pod-dispatched playbook rides: a manifest `plan run` publishes its
    /// session to the Tier 2 drop-box exactly as a loop run does, on the failing path too.
    #[test]
    fn manifest_plan_run_delivers_its_session_to_the_dropbox() {
        let _guard = crate::test_env_lock();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let receiver = std::thread::spawn(move || {
            listener
                .incoming()
                .take(2)
                .map(|s| read_one_post(&mut s.expect("accept")))
                .collect::<Vec<_>>()
        });

        let dir = std::env::temp_dir().join(format!("crucible-dropbox-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let token = dir.join("token");
        std::fs::write(&token, "fake-token").expect("token");

        let manifest_for = |name: &str, script: &str| {
            let pack = dir.join(name);
            std::fs::create_dir_all(&pack).expect("mkdir pack");
            let probe = pack.join("probe.sh");
            std::fs::write(&probe, script).expect("probe");
            std::fs::write(
                pack.join("workflow.star"),
                format!(
                    "workflow(type = \"playbook\", tasks = [command(name = \"probe\", run = \"sh {}\")])\n",
                    probe.display()
                ),
            )
            .expect("workflow");
            std::fs::write(
                pack.join("crucible.toml"),
                r#"
                [repo]
                path = "."
                [workspace]
                dir = "workspace"
                setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
                [agent]
                backend = "command"
                agent_cmd = "true"
                goal = "deliver the session"
                [workflow]
                type = "playbook"
                file = "workflow.star"
                "#,
            )
            .expect("manifest");
            pack.join("crucible.toml")
        };

        let passing = manifest_for("pass", "echo '{\"ok\": true}'\n");
        let failing = manifest_for("fail", "echo '{\"ok\": false}'\nexit 1\n");

        let _ingest = ScopedEnv::set(&[
            (
                crucible_contract::ENV_INGEST_URL,
                format!("http://127.0.0.1:{port}"),
            ),
            (
                crucible_contract::ENV_INGEST_TOKEN_PATH,
                token.display().to_string(),
            ),
            (crucible_contract::ENV_POD_NAME, "crucible-run-pod".into()),
        ]);

        let ceilings = || Ceilings {
            usd: Some(1.0),
            wall_clock: Some(std::time::Duration::from_secs(60)),
            wall_clock_raw: Some("60s".to_string()),
        };
        run(
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            Some(&passing),
            ceilings(),
        )
        .expect("the passing playbook reaches a verdict");
        run(
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            Some(&failing),
            ceilings(),
        )
        .expect_err("the failing playbook has no valid verdict");

        drop(_ingest);

        let posts = receiver.join().expect("receiver");
        assert_eq!(posts.len(), 2, "both runs deliver");
        for ((path, body), outcome) in posts.iter().zip(["finished", "error"]) {
            assert_eq!(path, "/api/pods/crucible-run-pod/artifacts/run-session");
            assert_eq!(&body[..2], b"\x1f\x8b", "the body is gzip");
            let mut text = String::new();
            std::io::Read::read_to_string(
                &mut flate2::read::GzDecoder::new(body.as_slice()),
                &mut text,
            )
            .expect("gunzip");
            assert!(text.contains("probe"), "the session's tasks: {text}");
            let last = text.lines().last().expect("a delivered session");
            let ev = crate::session::decode(last).expect("the last line decodes");
            match ev {
                crate::session::SessionEvent::Shutdown { outcome: got, .. } => {
                    assert_eq!(got, outcome, "{text}")
                }
                other => panic!("the session must end in a shutdown, got {other:?}: {text}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! The `crucible plan` CLI: `show` compiles and prints a plan; `run` executes one.

use crate::args::AgentOverride;
use crate::knobs::Harness;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::plan::exec::{Substrate, runnable_set};
use crate::plan::ir::{TaskKind, ValidPlan};
pub(crate) use crate::plan::run::{Ceilings, NoValidVerdict, Rerun, load, parse_params};
use crucible::crucible::Direction;
use xai_grok_mermaid::{MermaidTheme, RenderLimits, RenderParams, default_engine, render_checked};

#[derive(Debug, thiserror::Error)]
#[error("mermaid render failed: {detail}")]
struct MermaidRenderFailed {
    detail: String,
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
    for schema_file in &compiled.schema_files {
        eprintln!("embedded schema: {}", schema_file.display());
    }
    print!("{}", compiled.canonical_json);
    Ok(())
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
                harness.map_or("default", Harness::as_str),
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
            TaskKind::Route { questions, decider } => format!(
                "route[{}; {}]",
                questions
                    .keys()
                    .map(|q| q.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                match decider {
                    crate::plan::ir::Decider::Model { min_confidence, .. } =>
                        format!("model >= {min_confidence}"),
                    crate::plan::ir::Decider::Output { task } => format!("from {task}"),
                    crate::plan::ir::Decider::Human { .. } => "human".to_string(),
                }
            ),
            TaskKind::Report { .. } => "report".to_string(),
            TaskKind::Engine { .. } => t.task.label().to_string(),
        };
        if let Some(session) = &t.session {
            detail.push_str(&format!(" session={session}"));
        }
        if let Some(when) = &t.when {
            detail.push_str(&format!(" when={when}"));
        }
        if let Some(timeout) = &t.timeout {
            detail.push_str(&format!(" timeout={timeout}"));
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
    let over: BTreeMap<_, _> = plan
        .tasks_topo()
        .map(|t| (t.name.clone(), t.over.as_ref()))
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
            TaskKind::Route { .. } => ("{", "}", CLASS_STYLES[4]),
            TaskKind::Report { .. } => ("[[", "]]", CLASS_STYLES[3]),
            TaskKind::Engine { .. } => ("[[", "]]", CLASS_STYLES[5]),
        };
        let mut detail = match &t.task {
            TaskKind::Agent { harness, model, .. } => format!(
                "<br/>{}/{}",
                mermaid_label(harness.map_or("default", Harness::as_str)),
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
            TaskKind::Command { .. }
            | TaskKind::Evaluate { .. }
            | TaskKind::Report { .. }
            | TaskKind::Engine { .. } => String::new(),
            TaskKind::TopK { k, .. } => format!("<br/>k={k}"),
            TaskKind::Route { questions, .. } => questions
                .keys()
                .map(|q| format!("<br/>{}?", mermaid_label(q.as_str())))
                .collect(),
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
            let aligned = t.over.is_some() && over.get(d).copied().flatten() == t.over.as_ref();
            let arrow = if aligned { "==>" } else { "-->" };
            let mut labels = Vec::new();
            if let Some(when) = t.when.as_ref().filter(|when| &when.task == d) {
                let answers: Vec<&str> = when
                    .any_option
                    .then_some("any option")
                    .into_iter()
                    .chain(when.is.iter().map(|l| l.as_str()))
                    .collect();
                labels.push(format!(
                    "{}: {}",
                    mermaid_label(when.question.as_str()),
                    mermaid_label(&answers.join(" / "))
                ));
            }
            for keyed in t.keyed.iter().filter(|keyed| &keyed.task == d) {
                labels.push(format!("{}[item]", mermaid_label(&keyed.field.0)));
            }
            let (from, to) = (&ids[d], &ids[&t.name]);
            edges.push(if labels.is_empty() {
                format!("    {from} {arrow} {to}\n")
            } else {
                format!("    {from} {arrow}|\"{}\"| {to}\n", labels.join(" · "))
            });
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

#[cfg(any(feature = "autoresearch", test))]
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

/// What a launch supplies beyond the graph and its parameters.
#[derive(Debug, Clone, Default)]
pub struct RunOpts {
    pub ceilings: Ceilings,
    pub compute_driver: crate::openshell::gateway::ComputeDriver,
    pub agent: AgentOverride,
    /// Continue the playbook run the manifest's state dir holds instead of starting one.
    pub resume: bool,
    /// The manifest variant the run reads.
    pub variant: Option<crate::manifest::variant::VariantName>,
    /// Rerun a finished playbook run from some of its tasks instead of starting afresh.
    pub rerun: Option<Rerun>,
    /// The engine runtime the run's sandboxed turns run on.
    pub engine: Option<tokio::runtime::Handle>,
}

/// The full engine: agent turns through the harness, model-decided routes, and a decision desk
/// when the controller handed the run one.
struct FullEngine {
    compute_driver: crate::openshell::gateway::ComputeDriver,
    agent: AgentOverride,
    runtime: Option<tokio::runtime::Handle>,
}

impl crate::plan::run::Engine for FullEngine {
    type Dispatch = crate::args::Args;

    fn prepare(
        &mut self,
        manifest: &Path,
        variant: Option<&crate::manifest::variant::VariantName>,
        params: &BTreeMap<String, String>,
        start: crate::cli::setup::Start,
    ) -> Result<(
        crate::plan::harness::HarnessRunner,
        crate::manifest::Manifest,
    )> {
        crate::cli::setup::prep_plan_runner_with_params(
            manifest,
            variant,
            params,
            self.compute_driver,
            self.agent.clone(),
            self.runtime.clone(),
            start,
        )
    }

    fn desk<'a>(
        &self,
        log: Option<&'a std::fs::File>,
    ) -> Option<Box<dyn crucible::plan::decide::DecisionDesk + 'a>> {
        crate::plan::desk::HttpDesk::from_env(log)
            .map(|desk| Box::new(desk) as Box<dyn crucible::plan::decide::DecisionDesk + 'a>)
    }

    fn substrate(&self, caps: BTreeSet<String>) -> Result<Substrate> {
        Ok(Substrate::detecting(
            caps,
            &crucible::inference::from_process_env()?,
        ))
    }

    fn trace_context(&self) -> Option<(String, String)> {
        crate::plan::events::trace_context()
    }
}

/// Compile and execute a plan: real subprocesses, real outputs, the executor's real
/// semantics. With `--manifest`, agent tasks run through the real harness path; otherwise
/// the shell runner handles everything (`--agent-cmd` as the agent stand-in). Exits nonzero
/// when the plan is not valid.
pub fn run(
    path: Option<&Path>,
    params: &BTreeMap<String, String>,
    caps: &BTreeSet<String>,
    agent_cmd: Option<String>,
    manifest: Option<&Path>,
    opts: RunOpts,
) -> Result<()> {
    use crate::plan::run::{Launch, Source};
    let RunOpts {
        ceilings,
        compute_driver,
        agent,
        resume,
        variant,
        rerun,
        engine,
    } = opts;
    let source = match (path, manifest) {
        (plan, Some(manifest)) => Source::Manifest { manifest, plan },
        (Some(plan), None) => Source::Plan {
            plan,
            runner: Box::new(crate::plan::runner::ShellRunner {
                workdir: std::env::current_dir().context("resolving the working directory")?,
                agent_cmd,
                env: crucible::host_env::HostEnv::inherited(),
                buildit_socket: None,
                task_servers: Vec::new(),
                github: false,
                model_route: Some(crate::plan::route::model_attempt_from_env),
            }),
        },
        (None, None) => unreachable!("clap requires --file without --manifest"),
    };
    crate::plan::run::run(
        &mut FullEngine {
            compute_driver,
            agent,
            runtime: engine,
        },
        source,
        params,
        caps,
        Launch {
            ceilings,
            resume,
            variant,
            rerun,
        },
    )
}

#[cfg(test)]
mod tests {
    use crate::manifest::variant::VariantName;
    use crate::plan::cli::*;
    use crate::plan::ir::Plan;

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

    /// A plan file carries its own tasks, so its harness and effort are checked as it loads.
    #[test]
    fn a_plan_file_naming_a_harness_no_turn_can_read_is_refused() {
        let dir = crate::testing::tempdir("plan-file-knobs");
        let file = dir.join("plan.toml");
        std::fs::write(
            &file,
            SRC.replace("prompt = \"go\"", "prompt = \"go\"\nharness = \"gpt\""),
        )
        .unwrap();
        let err = load(&file).map(|_| ()).expect_err("refused");
        assert!(
            format!("{err:#}").contains("harness \"gpt\" is not claude, codex, opencode or pi"),
            "{err:#}"
        );
        std::fs::write(&file, SRC).unwrap();
        load(&file).expect("a plan with no knobs set loads");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// How a pack spells its graph is not a launcher's business. An undeclared `--param` is a
    /// mistake against the inline spelling exactly as it is against the file-backed one, and
    /// neither run reaches a task.
    #[test]
    fn an_undeclared_parameter_refuses_both_spellings_of_the_same_pack() {
        let _guard = crucible::test_support::env_lock();
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
                RunOpts {
                    ceilings: Ceilings {
                        usd: Some(1.0),
                        wall_clock: Some(std::time::Duration::from_secs(60)),
                        wall_clock_raw: Some("60s".to_string()),
                    },
                    ..Default::default()
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

    /// A variant's tables replace the base's whole for the run that selects it, and a resume
    /// of that run must select it again.
    #[test]
    fn a_variant_run_reads_its_own_graph_and_resumes_only_as_itself() {
        let _guard = crucible::test_support::env_lock();
        let dir = crate::testing::tempdir("plan-run-variant");
        let base_ran = dir.join("base-ran");
        let offline_ran = dir.join("offline-ran");
        for (file, sentinel) in [("base.star", &base_ran), ("offline.star", &offline_ran)] {
            std::fs::write(
                dir.join(file),
                format!(
                    "workflow(type = \"playbook\", tasks = [command(name = \"a\", run = \"touch {} && echo '{{}}'\")])\n",
                    sentinel.display()
                ),
            )
            .expect("workflow");
        }
        let manifest = dir.join("crucible.toml");
        std::fs::write(
            &manifest,
            r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            [workflow]
            type = "playbook"
            file = "base.star"
            [variant.offline.workflow]
            type = "playbook"
            file = "offline.star"
            "#,
        )
        .expect("manifest");
        let launch = |variant: Option<&str>, resume: bool| {
            run(
                None,
                &BTreeMap::new(),
                &BTreeSet::new(),
                None,
                Some(&manifest),
                RunOpts {
                    ceilings: Ceilings {
                        usd: Some(1.0),
                        wall_clock: Some(std::time::Duration::from_secs(60)),
                        wall_clock_raw: Some("60s".to_string()),
                    },
                    variant: variant.map(|v| VariantName::parse(v).expect("variant name")),
                    resume,
                    ..Default::default()
                },
            )
        };

        launch(Some("offline"), false).expect("the offline variant runs");
        assert!(offline_ran.exists(), "the variant's graph ran");
        assert!(!base_ran.exists(), "the base [workflow] was replaced whole");

        let error = launch(None, true).expect_err("a resume under the base is another run");
        assert!(
            format!("{error:#}").contains(
                "reads the base manifest, but the interrupted run read [variant.offline]"
            ),
            "{error:#}"
        );
        launch(Some("missing"), false).expect_err("an undeclared variant is refused");
        launch(Some("offline"), true).expect("a resume under the same variant continues");
        assert!(!base_ran.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--file` is a compiled plan, which binds nothing; only a `--manifest` run compiles a
    /// graph. Refusing at parse time keeps a supplied value from being dropped downstream.
    #[test]
    fn a_precompiled_plan_takes_no_parameters() {
        use clap::Parser;
        let parse = |args: &[&str]| <crate::cli::Cli>::try_parse_from(args).is_ok();
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
    fn render_states_each_tasks_own_time_limit() {
        let mut plan = Plan::from_toml_str(SRC).unwrap();
        plan.tasks[0].timeout = Some("90m".parse().unwrap());
        let out = render(&plan.validate().unwrap(), &BTreeSet::new());
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[1].contains(" timeout=90m"), "{out}");
        assert!(!lines[2].contains("timeout="), "{out}");
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

    const PER_ELEMENT: &str = r#"
        version = 1
        [budget]
        usd = 1.0
        [[task]]
        name = "scan"
        kind = "command"
        command = "true"
        emits = ["items", "notes"]
        [[task]]
        name = "classify"
        kind = "command"
        command = "true"
        depends_on = ["scan"]
        emits = ["area"]
        [[task]]
        name = "gate"
        kind = "route"
        depends_on = ["classify"]
        decider = { kind = "output", task = "classify" }
        [task.questions.area]
        instructions = "Which component?"
        type = "choice"
        drop = ["frontend", "uncertain"]
        options = [{ label = "scheduler" }, { label = "frontend" }]
        [[task]]
        name = "fix"
        kind = "command"
        command = "true"
        depends_on = ["scan", "gate"]
        over = { task = "scan", field = "items" }
        max_fanout = 4
        keyed = [{ task = "scan", field = "notes" }]
        when = { task = "gate", question = "area", is = ["scheduler"] }
        [[task]]
        name = "check"
        kind = "command"
        command = "true"
        depends_on = ["scan", "fix"]
        over = { task = "scan", field = "items" }
        max_fanout = 4
        [[task]]
        name = "roll"
        kind = "command"
        command = "true"
        depends_on = ["check"]
    "#;

    fn mermaid_edge<'a>(out: &'a str, ids: &BTreeMap<&str, &str>, from: &str, to: &str) -> &'a str {
        let (from, to) = (ids[from], ids[to]);
        out.lines()
            .find(|line| {
                let line = line.trim();
                line.starts_with(&format!("{from} ")) && line.ends_with(&format!(" {to}"))
            })
            .unwrap_or_else(|| panic!("an edge {from} -> {to}: {out}"))
            .trim()
    }

    #[test]
    fn mermaid_draws_aligned_edges_thick_and_labels_when_and_keyed_edges() {
        let plan = Plan::from_toml_str(PER_ELEMENT)
            .unwrap()
            .validate()
            .unwrap();
        let out = render_mermaid(&plan, &BTreeSet::new());
        let ids: BTreeMap<&str, &str> = [
            ("scan", "t0"),
            ("classify", "t1"),
            ("gate", "t2"),
            ("fix", "t3"),
            ("check", "t4"),
            ("roll", "t5"),
        ]
        .into();
        assert!(
            out.contains(r#"t2{"gate<br/>area?"}"#),
            "route shape: {out}"
        );
        assert_eq!(mermaid_edge(&out, &ids, "fix", "check"), "t3 ==> t4");
        assert_eq!(
            mermaid_edge(&out, &ids, "gate", "fix"),
            r#"t2 -->|"area: scheduler"| t3"#,
            "an unmapped route's when edge reads the node"
        );
        assert_eq!(
            mermaid_edge(&out, &ids, "scan", "fix"),
            r#"t0 -->|"notes[item]"| t3"#
        );
        assert_eq!(
            mermaid_edge(&out, &ids, "scan", "check"),
            "t0 --> t4",
            "the producer of the list is not aligned with what maps over it"
        );
        assert_eq!(mermaid_edge(&out, &ids, "check", "roll"), "t4 --> t5");
    }

    #[test]
    fn mermaid_draws_a_per_element_when_on_a_mapped_route_as_a_thick_labelled_edge() {
        let src = r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "scan"
            kind = "command"
            command = "true"
            emits = ["items"]
            [[task]]
            name = "gate"
            kind = "route"
            depends_on = ["scan"]
            needs = "decision"
            over = { task = "scan", field = "items" }
            max_fanout = 4
            decider = { kind = "model", min_confidence = 0.8 }
            [task.questions.area]
            instructions = "Which component?"
            type = "choice"
            drop = ["frontend", "uncertain"]
            options = [{ label = "scheduler" }, { label = "frontend" }]
            [[task]]
            name = "fix"
            kind = "command"
            command = "true"
            depends_on = ["scan", "gate"]
            over = { task = "scan", field = "items" }
            max_fanout = 4
            when = { task = "gate", question = "area", is = ["scheduler"] }
        "#;
        let plan = Plan::from_toml_str(src).unwrap().validate().unwrap();
        let out = render_mermaid(&plan, &BTreeSet::new());
        let ids: BTreeMap<&str, &str> = [("scan", "t0"), ("gate", "t1"), ("fix", "t2")].into();
        assert_eq!(
            mermaid_edge(&out, &ids, "gate", "fix"),
            r#"t1 ==>|"area: scheduler"| t2"#
        );
    }

    #[test]
    fn mermaid_uses_distinct_internal_ids_for_similar_task_names() {
        let src = r#"
            version = 1
            [budget]
            usd = 1.0
            [[task]]
            name = "review.a"
            kind = "command"
            command = "true"
            [[task]]
            name = "review-a"
            kind = "command"
            command = "true"
        "#;
        let plan = Plan::from_toml_str(src).unwrap().validate().unwrap();
        let out = render_mermaid(&plan, &BTreeSet::new());
        assert!(out.contains("t0[\"review.a\"]"));
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
        let admitted = crate::plan::events::plan_admitted_event(&plan, None);
        let back =
            crate::report::session::decode(&crate::report::session::encode(&admitted)).unwrap();
        match back {
            crate::report::session::SessionEvent::PlanAdmitted {
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
            status: crucible_contract::session::TaskStatus::Pass,
            attempts: 1,
            cost_usd: 0.25,
            output: Some(serde_json::json!({"score": 3})),
            note: None,
            fanout: None,
            blocked: None,
            transport: None,
            repairs: Vec::new(),
        };
        let back = crate::report::session::decode(&crate::report::session::encode(
            &crate::plan::events::task_result_event(1, 0, t, &r, None, 0.0),
        ))
        .unwrap();
        match back {
            crate::report::session::SessionEvent::TaskResult {
                task,
                status,
                task_kind,
                cost_usd,
                output,
                ..
            } => {
                assert_eq!(task, "measure");
                assert_eq!(status, crucible_contract::session::TaskStatus::Pass);
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

    /// Process-global env vars, removed on drop so a panicking assertion cannot leak the controller
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

    /// Read one HTTP POST off `s`, answer 200, and return `(path, body)`. The evidence client is
    /// real `reqwest`, so the receiver has to be a real socket.
    fn read_one_post(s: &mut std::net::TcpStream) -> (String, Vec<u8>) {
        let (line, _, body) = crate::testing::read_one_request(s);
        let path = line
            .split_whitespace()
            .nth(1)
            .expect("a request path")
            .to_string();
        (path, body)
    }

    /// The evidence contract a pod-dispatched playbook rides (RFC-0004:C-RUN-EVIDENCE): a
    /// manifest `plan run` uploads its git memory and then its session to its own run under its
    /// run token, on the failing path too.
    #[test]
    fn manifest_plan_run_uploads_its_evidence_to_its_run() {
        let _guard = crucible::test_support::env_lock();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let receiver = std::thread::spawn(move || {
            let mut got = Vec::new();
            for s in listener.incoming() {
                let request = crate::testing::read_one_request(&mut s.expect("accept"));
                if request.0.starts_with("GET /done ") {
                    break;
                }
                got.push(request);
            }
            got
        });

        let dir = std::env::temp_dir().join(format!("crucible-evidence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
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
                crucible_contract::ENV_CONTROLLER_URL,
                format!("http://127.0.0.1:{port}/"),
            ),
            (crucible_contract::ENV_POD_TOKEN, "crr_id_secret".into()),
            (crucible_contract::ENV_RUN_NAME, "acme_x_7-1700".into()),
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
            RunOpts {
                ceilings: ceilings(),
                ..Default::default()
            },
        )
        .expect("the passing playbook reaches a verdict");
        run(
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            Some(&failing),
            RunOpts {
                ceilings: ceilings(),
                ..Default::default()
            },
        )
        .expect_err("the failing playbook has no valid verdict");

        drop(_ingest);
        reqwest::blocking::get(format!("http://127.0.0.1:{port}/done")).expect("stop");

        let requests = receiver.join().expect("receiver");
        let lines: Vec<&str> = requests.iter().map(|(l, _, _)| l.as_str()).collect();
        let commits = "PUT /api/evidence/runs/acme_x_7-1700/run-commits HTTP/1.1";
        let session = "PUT /api/evidence/runs/acme_x_7-1700/run-session HTTP/1.1";
        assert_eq!(
            lines,
            [commits, session, commits, session],
            "each run uploads its git memory, then its session last"
        );
        for (_, headers, body) in &requests {
            assert!(
                headers.contains("authorization: bearer crr_id_secret"),
                "{headers}"
            );
            assert_eq!(&body[..2], b"\x1f\x8b", "the body is gzip");
        }
        let sessions = requests
            .iter()
            .filter(|(l, _, _)| l.as_str() == session)
            .map(|(_, _, body)| body);
        for (body, outcome) in sessions.zip(["finished", "error"]) {
            let mut text = String::new();
            std::io::Read::read_to_string(
                &mut flate2::read::GzDecoder::new(body.as_slice()),
                &mut text,
            )
            .expect("gunzip");
            assert!(text.contains("probe"), "the session's tasks: {text}");
            let last = text.lines().last().expect("a delivered session");
            let ev = crate::report::session::decode(last).expect("the last line decodes");
            match ev {
                crate::report::session::SessionEvent::Shutdown { outcome: got, .. } => {
                    assert_eq!(got, outcome, "{text}")
                }
                other => panic!("the session must end in a shutdown, got {other:?}: {text}"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run whose required tasks held reports `pass`, though a branch was not taken and advisory
    /// tasks failed and skipped. The report reads the engine's verdict, not a status count.
    #[test]
    fn a_report_carries_the_engines_verdict_past_untaken_and_advisory_tasks() {
        let _guard = crucible::test_support::env_lock();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let receiver = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            read_one_post(&mut s)
        });

        let dir = std::env::temp_dir().join(format!("crucible-verdict-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("reports")).expect("mkdir");
        std::fs::write(
            dir.join("reports/card.j2"),
            "template verdict {{ verdict }}",
        )
        .expect("template");
        std::fs::write(
            dir.join("workflow.star"),
            r#"
pick = command(name = "pick", run = "echo '{\"lane\": \"a\"}'", emits = ["lane"])
gate = route(
    name = "gate",
    depends_on = [pick],
    source = pick,
    questions = {"lane": choice(ask = "Which lane?", options = ["a", "b"])},
)
a = command(name = "a", run = "echo '{}'", depends_on = [gate], when = gate.lane, answers = "a")
b = command(name = "b", run = "echo '{}'", depends_on = [gate], when = gate.lane, otherwise = True)
lint = command(name = "lint", run = "exit 1", required = False)
optional = command(name = "optional", run = "echo '{\"status\": \"skipped\"}'", required = False)
publish = report(name = "publish", destination = {"kind": "slack"}, template = "reports/card.j2")
workflow(type = "playbook", tasks = [pick, gate, a, b, lint, optional, publish])
"#,
        )
        .expect("workflow");
        std::fs::write(
            dir.join("crucible.toml"),
            r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "report the verdict"
            [workflow]
            type = "playbook"
            file = "workflow.star"
            "#,
        )
        .expect("manifest");

        let storage = dir.join("storage");
        let _env = ScopedEnv::set(&[
            ("FORGE_STORAGE_ROOT", storage.display().to_string()),
            ("SLACK_WEBHOOK_URL", format!("http://127.0.0.1:{port}/hook")),
        ]);
        run(
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            Some(&dir.join("crucible.toml")),
            RunOpts {
                ceilings: Ceilings {
                    usd: Some(1.0),
                    wall_clock: Some(std::time::Duration::from_secs(60)),
                    wall_clock_raw: Some("60s".to_string()),
                },
                ..Default::default()
            },
        )
        .expect("the required tasks held");
        drop(_env);

        let (_, body) = receiver.join().expect("receiver");
        let card = String::from_utf8(body).expect("utf-8");
        assert!(card.contains("template verdict pass"), "{card}");
        assert!(card.contains("*Verdict* pass"), "{card}");

        let report: crucible_contract::RunReport = serde_json::from_slice(
            &std::fs::read(storage.join(crucible_contract::REPORT_FILE)).expect("report"),
        )
        .expect("decode");
        assert_eq!(report.verdict, crucible_contract::RunVerdict::Pass);
        let status = |name: &str| {
            report
                .tasks
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.status)
        };
        use crucible_contract::session::TaskStatus;
        assert_eq!(status("b"), Some(TaskStatus::NotTaken));
        assert_eq!(status("lint"), Some(TaskStatus::Fail));
        assert_eq!(status("optional"), Some(TaskStatus::Skipped));
        assert_eq!(status("publish"), Some(TaskStatus::Pass));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_mcp_server_that_fails_to_start_ends_the_run_with_its_stderr() {
        let _env = crucible::test_support::env_lock();
        let dir = crate::testing::tempdir("mcp-start-fails");
        std::fs::write(dir.join("tool.py"), "print('ok')\n").unwrap();
        std::fs::write(
            dir.join("workflow.star"),
            "t = command(name = \"t\", run = \"python3 tool.py\")\nworkflow(type = \"playbook\", tasks = [t])\n",
        )
        .unwrap();
        let manifest = dir.join("crucible.toml");
        std::fs::write(
            &manifest,
            r#"
            [workspace]
            inject = ["tool.py"]
            [agent]
            backend = "openshell"
            goal = "g"
            mcp = ["buildit"]
            [mcp.buildit]
            bin = "sh"
            args = ["-c", "echo 'buildit: KUBECONFIG names no cluster' >&2; exit 3"]
            [workflow]
            type = "playbook"
            file = "workflow.star"
            "#,
        )
        .unwrap();

        let error = run(
            None,
            &BTreeMap::new(),
            &BTreeSet::new(),
            None,
            Some(&manifest),
            RunOpts {
                ceilings: Ceilings {
                    usd: Some(1.0),
                    wall_clock: Some(std::time::Duration::from_secs(60)),
                    wall_clock_raw: Some("60s".to_string()),
                },
                ..Default::default()
            },
        )
        .expect_err("the run cannot start its server");

        assert!(error.is::<crate::cli::setup::EndedAtSetup>(), "{error:#}");
        let log = std::fs::read_to_string(dir.join("state/session.jsonl")).unwrap();
        let last = log.lines().last().and_then(crate::report::session::decode);
        let Some(crate::report::session::SessionEvent::Shutdown { outcome, reason }) = last else {
            panic!("the session log does not end in a shutdown: {log}");
        };
        assert_eq!(outcome, "error");
        assert!(
            reason.contains("[mcp.buildit] (`sh`) exited")
                && reason.ends_with("buildit: KUBECONFIG names no cluster"),
            "{reason}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

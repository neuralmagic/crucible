//! The `crucible plan` CLI: `show` compiles and prints a plan; `run` executes one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::plan::exec::{Substrate, runnable_set};
use crate::plan::ir::{Plan, TaskKind, ValidPlan};

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
        let detail = match &t.task {
            TaskKind::Agent { model, harness, .. } => format!(
                "agent[{}/{}]",
                harness.as_deref().unwrap_or("default"),
                model.as_deref().unwrap_or("default")
            ),
            TaskKind::Command { command } => format!("command[{command}]"),
            TaskKind::TopK { k, .. } => format!("top_k[k={k}]"),
            TaskKind::Engine { .. } => t.task.label().to_string(),
        };
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

/// Render the compiled plan as mermaid flowchart source: pipeable into a terminal mermaid
/// renderer, pasteable into GitHub markdown, and the same source a UI graph view consumes.
pub fn render_mermaid(plan: &ValidPlan, caps: &BTreeSet<String>) -> String {
    let runnable = runnable_set(plan, &Substrate { caps: caps.clone() });
    let mut out = String::from("flowchart TD\n");
    let ids: BTreeMap<_, _> = plan
        .tasks_topo()
        .enumerate()
        .map(|(i, t)| (t.name.clone(), format!("t{i}")))
        .collect();
    for t in plan.tasks_topo() {
        let ok = runnable.contains(&t.name);
        let (shape_open, shape_close, class) = match &t.task {
            TaskKind::Agent { .. } => ("([", "])", "agent"),
            TaskKind::Command { .. } => ("[", "]", "command"),
            TaskKind::TopK { .. } => ("{{", "}}", "reduce"),
            TaskKind::Engine { .. } => ("[[", "]]", "engine"),
        };
        let detail = match &t.task {
            TaskKind::Agent { harness, model, .. } => format!(
                "<br/>{}/{}",
                mermaid_label(harness.as_deref().unwrap_or("default")),
                mermaid_label(model.as_deref().unwrap_or("default"))
            ),
            TaskKind::Command { .. } | TaskKind::Engine { .. } => String::new(),
            TaskKind::TopK { k, .. } => format!("<br/>k={k}"),
        };
        let marks = format!(
            "{}{}",
            if t.required { "" } else { " (advisory)" },
            if ok { "" } else { " ⛔" }
        );
        out.push_str(&format!(
            "    {id}{shape_open}\"{name}{detail}{marks}\"{shape_close}:::{class}\n",
            id = ids[&t.name],
            name = mermaid_label(&t.name.0),
        ));
        for d in &t.depends_on {
            out.push_str(&format!("    {} --> {}\n", ids[d], ids[&t.name]));
        }
    }
    out.push_str(
        "    classDef agent fill:#458588,color:#fbf1c7\n\
         classDef command fill:#98971a,color:#282828\n\
         classDef reduce fill:#d79921,color:#282828\n\
         classDef engine fill:#b16286,color:#fbf1c7\n",
    );
    out
}

/// Serialize an admitted plan for the event stream.
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
                needs: t.needs.clone(),
                required: t.required,
            })
            .collect(),
    }
}

/// One terminal task result on the wire. `iter` is the loop round (0 for a standalone
/// `plan run`); fields belonging to other emitters stay at their defaults.
pub(crate) fn task_result_event(
    plan_version: u32,
    iter: u32,
    task: &crate::plan::ir::Task,
    r: &crate::plan::exec::TaskResult,
) -> crate::session::SessionEvent {
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
        trace_id: String::new(),
        span_id: String::new(),
    }
}

fn mermaid_label(name: &str) -> String {
    name.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\r', '\n'], " ")
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

/// Render the plan's mermaid to PNG (offline, vendored engine) and display it inline when
/// the terminal speaks an image protocol; otherwise write `<plan>.png` next to the file.
fn show_rendered(path: &Path, plan: &ValidPlan, caps: &BTreeSet<String>) -> Result<()> {
    use xai_grok_mermaid::{
        MermaidTheme, RenderLimits, RenderParams, default_engine, render_checked,
    };

    let src = render_mermaid(plan, caps);
    let engine = default_engine();
    let params = RenderParams {
        theme: MermaidTheme::Dark,
        ..RenderParams::default()
    };
    let diagram = render_checked(engine.as_ref(), &src, &params, &RenderLimits::default())
        .map_err(|e| anyhow::anyhow!("mermaid render failed: {e}"))?;
    match crate::plan::term_img::detect() {
        Some(proto) => {
            print!("{}", crate::plan::term_img::emit(proto, &diagram.png));
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

/// Compile and execute a plan: real subprocesses, real outputs, the executor's real
/// semantics. With `--manifest`, agent tasks run through the real harness path; otherwise
/// the shell runner handles everything (`--agent-cmd` as the agent stand-in). Exits nonzero
/// when the plan is not valid.
pub fn run(
    path: &Path,
    caps: &BTreeSet<String>,
    agent_cmd: Option<String>,
    manifest: Option<&Path>,
) -> Result<()> {
    use crate::plan::exec::{ExecCfg, PlanExit, TaskRunner, execute};
    use crate::plan::runner::ShellRunner;

    let plan = load(path)?;
    let substrate = Substrate { caps: caps.clone() };
    // Manifest runs append plan wire events to the run's session log so tailers (and the
    // controller's ingest) see the graph and its live progress; shell runs have no state dir.
    let (mut runner, events): (Box<dyn TaskRunner>, Option<std::fs::File>) = match manifest {
        Some(m) => {
            let r = crate::run::prep_plan_runner(m)?;
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&r.paths.session_log)
                .with_context(|| format!("opening {}", r.paths.session_log.display()))?;
            (Box::new(r), Some(f))
        }
        None => (
            Box::new(ShellRunner {
                workdir: std::env::current_dir().context("resolving the working directory")?,
                agent_cmd,
            }),
            None,
        ),
    };
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
        ExecCfg::default(),
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
    };
    println!(
        "plan v{}: {} — spent ${:.4} of ${}",
        plan.plan().version,
        exit,
        out.spent_usd,
        plan.plan().budget.usd
    );
    if !out.valid {
        anyhow::bail!("plan did not reach a valid verdict ({exit})");
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
}

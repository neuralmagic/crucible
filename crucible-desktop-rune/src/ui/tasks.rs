//! The run's tasks as cards: what each one is, what it ran on, what it did and what it left.

use crucible_contract::session::{BlockedReasonKind, TaskStatus, TransportCause};
use rune::{Color, El, ScrollSpec, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Spinner};
use rune_theme_opencode::icons as oc;
use serde_json::Value;

use crate::App;
use crate::gfx;
use crate::logline::{Activity, LogLine, activity};
use crate::session::{Instance, InstanceCounts, LinkView, Outcome, TaskState, TaskView};
use crate::ui::{Paint, Tone, flag, fx, meter, mono, pulsing, tag};

const LINK_CHIPS: usize = 4;
const OUTPUT_CHARS: usize = 12_000;
const INSTANCE_CHIPS: usize = 48;

pub(crate) fn status_color(status: TaskStatus, p: &Paint) -> Color {
    match status {
        TaskStatus::Pass => p.pass,
        TaskStatus::Skipped | TaskStatus::NotTaken => p.c.text_muted,
        TaskStatus::Blocked => p.fail.mix(p.c.text_muted, 0.5),
        TaskStatus::Fail | TaskStatus::Transport | TaskStatus::Truncated => p.fail,
    }
}

pub(crate) fn task_list(
    app: &App,
    tasks: &[TaskView],
    log: &[LogLine],
    empty: El,
    p: &Paint,
) -> El {
    let mut list = col()
        .key("tasks")
        .role("list")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(6.0)
        .pr(4.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(10.0);
    if tasks.is_empty() {
        return list.child(empty);
    }
    for task in tasks {
        let open = app.expanded.as_deref() == Some(task.name.as_str());
        list = list.child(card(task, &activity(log, &task.name), open, p));
    }
    list
}

fn card(task: &TaskView, activity: &Activity, open: bool, p: &Paint) -> El {
    let state = task.state();
    let name = task.name.clone();

    let mut title = row().w_full().gap(8.0).center_y().child(
        text(task.name.clone())
            .font(mono(12.5, 500.0))
            .color(p.c.text_base)
            .shrink()
            .truncate(),
    );
    if task.is_fanout() {
        let total = task.instances.len();
        title = title.child(tag(
            &if total > 0 {
                format!("fan-out \u{d7}{total}")
            } else {
                "fan-out".to_string()
            },
            p,
        ));
    }
    if !task.required {
        title = title.child(flag("advisory", Tone::Warning, None, p));
    }
    if let TaskState::Running(attempt) = state
        && attempt > 1
    {
        title = title.child(el().grow(1.0)).child(
            text(format!("try {attempt}"))
                .font(p.k.ty.small_medium)
                .color(p.live),
        );
    }

    let mut body = col().w_full().gap(6.0).child(title);
    let facts = facts(task, activity);
    if !facts.is_empty() {
        body = body.child(
            text(facts.join("  \u{b7}  "))
                .font(p.k.ty.caption())
                .color(p.c.text_muted)
                .truncate(),
        );
    }
    if !task.instances.is_empty() {
        body = body.child(fanout_bar(task.instance_counts(), &task.name, p));
    }
    if let Some(line) = headline(task, state, activity, p) {
        body = body.child(line);
    }
    let links = task.links();
    if !links.is_empty() {
        body = body.child(link_chips(&links, p));
    }
    if open {
        body = body.child(expanded(task, p));
    }

    row()
        .key(format!("task-{}", task.name))
        .role("listitem")
        .label(format!("{} \u{2014} {}", task.name, state_label(state)))
        .w_full()
        .gap(10.0)
        .px(12.0)
        .py(10.0)
        .radius(p.k.radius.lg)
        .fill(if open { p.c.layer_02 } else { p.c.layer_01 })
        .border(
            p.hair,
            if open {
                p.c.border_base
            } else {
                p.c.border_muted
            },
        )
        .hover(p.c.hover)
        .press(p.c.pressed)
        .focusable(true)
        .on_press(move |app: &mut App| app.toggle_task(&name))
        .children(
            state
                .is_running()
                .then(|| fx(gfx::SHIMMER, gfx::data(p.live, 0.08), 0.0)),
        )
        .child(row().size(16.0, 18.0).center().child(state_mark(state, p)))
        .child(body.grow(1.0).min_w(0.0))
}

/// The task's state as an icon: a ring while pending, a spinner while running, then a check, a
/// cross or a ban sign.
pub(crate) fn state_mark(state: TaskState, p: &Paint) -> El {
    let glyph = |g, ink| icon(g, 16.0).color(ink);
    match state {
        TaskState::Pending => el()
            .size(12.0, 12.0)
            .radius(6.0)
            .border(1.0, p.c.text_faint),
        TaskState::Running(_) => pulsing(
            Spinner::new().size(14.0).color(p.live).build(p.k),
            p.live,
            16.0,
        ),
        TaskState::Settled(status) => match status {
            TaskStatus::Pass => glyph(&oc::CIRCLE_CHECK, p.pass),
            TaskStatus::Skipped | TaskStatus::NotTaken => {
                glyph(&oc::CIRCLE_BAN_SIGN, p.c.icon_faint)
            }
            TaskStatus::Blocked => glyph(&oc::CIRCLE_BAN_SIGN, status_color(status, p)),
            TaskStatus::Fail | TaskStatus::Transport | TaskStatus::Truncated => {
                glyph(&oc::CIRCLE_X, p.fail)
            }
        },
    }
}

/// Only states that need attention get a colour; the rest read as plain text.
fn label_ink(state: TaskState, p: &Paint) -> Color {
    match state {
        TaskState::Running(_) => p.live,
        TaskState::Settled(
            TaskStatus::Fail | TaskStatus::Transport | TaskStatus::Truncated | TaskStatus::Blocked,
        ) => p.fail,
        _ => p.c.text_faint,
    }
}

fn state_label(state: TaskState) -> String {
    match state {
        TaskState::Running(attempt) if attempt > 1 => format!("running \u{b7} try {attempt}"),
        state => state.label().replace('_', " "),
    }
}

/// The one-line summary under the title: agent, timing, spend, retries, activity.
fn facts(task: &TaskView, activity: &Activity) -> Vec<String> {
    let mut facts = Vec::new();
    if let Some(agent) = task.agent() {
        let parts: Vec<&str> = [
            agent.harness.as_str(),
            agent.model.as_str(),
            agent.effort.as_str(),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect();
        if !parts.is_empty() {
            facts.push(parts.join(" \u{b7} "));
        }
    }
    if let Some(result) = &task.result {
        if result.secs > 0.0 {
            facts.push(duration(result.secs));
        }
        if result.cost_usd > 0.0 {
            facts.push(format!("${:.4}", result.cost_usd));
        }
    } else if let Some(attempt) = task.attempt.filter(|a| a.estimate_usd > 0.0) {
        facts.push(format!("est ${:.2}/try", attempt.estimate_usd));
    }
    let retries = task.retries();
    if retries > 0 {
        facts.push(format!(
            "{retries} {}",
            if retries == 1 { "retry" } else { "retries" }
        ));
    }
    if activity.tools > 0 {
        let failed = if activity.failed_tools > 0 {
            format!(" ({} failed)", activity.failed_tools)
        } else {
            String::new()
        };
        facts.push(format!("{} tool calls{failed}", activity.tools));
    }
    if activity.tokens > 0 {
        facts.push(format!("{} tokens", count(activity.tokens)));
    }
    facts
}

/// What the reader most needs next: what a live task is doing, why a task stopped, or what it
/// produced.
fn headline(task: &TaskView, state: TaskState, activity: &Activity, p: &Paint) -> Option<El> {
    let (line, ink) = match (state, &task.result) {
        (TaskState::Running(_), _) => (
            activity
                .last
                .clone()
                .unwrap_or_else(|| "starting".to_string()),
            p.live,
        ),
        (_, Some(result)) => {
            if let Some(why) = stopped_why(result) {
                (why, status_color(result.status, p))
            } else if !result.note.is_empty() {
                (result.note.clone(), p.c.text_muted)
            } else {
                let output = result.output.as_ref().and_then(output_headline)?;
                (output, p.c.text_base)
            }
        }
        _ if !task.when.is_empty() => (format!("when {}", task.when), p.c.text_muted),
        _ => return None,
    };
    Some(
        text(line)
            .font(mono(11.5, 400.0))
            .color(ink)
            .w_full()
            .max_lines(2)
            .wrap(),
    )
}

fn stopped_why(result: &Outcome) -> Option<String> {
    if let Some(blocked) = &result.blocked {
        let reason = match blocked.reason {
            BlockedReasonKind::RequiredTaskFailed => "a required task failed",
            BlockedReasonKind::BudgetCeiling => "the budget ran out",
            BlockedReasonKind::WallClockCeiling => "the time cap ran out",
            BlockedReasonKind::DependencyDidNotPass => "a dependency did not pass",
            BlockedReasonKind::StagingRefused => "staging was refused",
        };
        return Some(match &blocked.task {
            Some(task) => format!("blocked: {reason} ({task})"),
            None => format!("blocked: {reason}"),
        });
    }
    let transport = result.transport?;
    let cause = match transport {
        TransportCause::Gateway => "the openshell gateway",
        TransportCause::Sandbox => "the sandbox",
        TransportCause::Agent => "the agent process",
        TransportCause::Provider => "the model provider",
        TransportCause::Workspace => "preparing the workspace",
        TransportCause::Command => "spawning the command",
        TransportCause::Orchestrator => "the orchestrator",
        TransportCause::Other => "the transport",
    };
    Some(if result.note.is_empty() {
        format!("lost to {cause}")
    } else {
        format!("lost to {cause}: {}", result.note)
    })
}

/// A route's pick, or a compact rendering of what the task emitted. A fan-out's aggregate is
/// left to its bar.
fn output_headline(output: &Value) -> Option<String> {
    let object = output.as_object()?;
    if object.contains_key("instances") && object.contains_key("passed") {
        return None;
    }
    let pick = |value: &Value| {
        let label = value.get("label")?.as_str()?;
        Some(match value.get("confidence").and_then(Value::as_f64) {
            Some(confidence) => format!("{label} ({confidence:.2})"),
            None => label.to_string(),
        })
    };
    if let Some(found) = pick(output) {
        return Some(found);
    }
    if object.len() == 1
        && let Some(found) = object.values().next().and_then(pick)
    {
        return Some(found);
    }
    Some(
        object
            .iter()
            .map(|(key, value)| format!("{key}={}", brief(value)))
            .collect::<Vec<_>>()
            .join("  "),
    )
}

fn brief(value: &Value) -> String {
    match value {
        Value::Array(items) => format!("[{}]", items.len()),
        Value::Object(map) => format!("{{{}}}", map.len()),
        Value::String(text) if text.chars().count() > 40 => {
            format!("\"{}\u{2026}\"", text.chars().take(39).collect::<String>())
        }
        other => other.to_string(),
    }
}

fn fanout_bar(counts: InstanceCounts, name: &str, p: &Paint) -> El {
    let total = counts.total().max(1) as f32;
    let bar = meter(
        &format!("fanout-{name}"),
        &[
            (counts.passed as f32 / total, p.pass),
            (counts.failed as f32 / total, p.fail),
            (counts.running as f32 / total, p.live),
            (counts.skipped as f32 / total, p.c.text_faint),
        ],
        p,
    );
    let mut parts = vec![format!("{}/{} passed", counts.passed, counts.total())];
    if counts.running > 0 {
        parts.push(format!("{} running", counts.running));
    }
    if counts.failed > 0 {
        parts.push(format!("{} failed", counts.failed));
    }
    if counts.skipped > 0 {
        parts.push(format!("{} not taken", counts.skipped));
    }
    row().w_full().gap(8.0).center_y().child(bar).child(
        text(parts.join(" \u{b7} "))
            .font(p.k.ty.caption())
            .color(p.c.text_muted),
    )
}

fn link_chips(links: &[&LinkView], p: &Paint) -> El {
    let mut chips = row().w_full().flex_wrap().gap(4.0);
    for link in links.iter().take(LINK_CHIPS) {
        chips = chips.child(link_chip(link, p));
    }
    if links.len() > LINK_CHIPS {
        chips = chips.child(
            text(format!("+{} more", links.len() - LINK_CHIPS))
                .font(mono(10.0, 400.0))
                .color(p.c.text_muted),
        );
    }
    chips
}

fn link_chip(link: &LinkView, p: &Paint) -> El {
    let url = link.url.clone();
    row()
        .role("link")
        .label(format!("Open {}", link.url))
        .h(20.0)
        .px(6.0)
        .gap(5.0)
        .center_y()
        .radius(p.k.radius.sm)
        .fill(p.c.layer_02)
        .border(p.hair, p.c.border_base)
        .hover(p.c.hover)
        .focusable(true)
        .on_press_tx(move |_: &mut App, tx| {
            let _ = tx.open_url(&url);
        })
        .child(
            text(provider_mark(link.provider))
                .font(p.k.ty.small_medium)
                .color(p.c.text_faint),
        )
        .child(
            text(link.label.clone())
                .font(mono(11.0, 500.0))
                .color(p.c.text_accent),
        )
}

fn provider_mark(provider: &str) -> String {
    match provider {
        "github" => "GH".to_string(),
        "gitlab" => "GL".to_string(),
        "jira" => "JIRA".to_string(),
        other => other.to_uppercase(),
    }
}

fn expanded(task: &TaskView, p: &Paint) -> El {
    let mut panel = col()
        .key(format!("task-open-{}", task.name))
        .w_full()
        .gap(10.0)
        .pt(8.0);

    let mut grid = col().w_full().gap(3.0);
    for (label, value) in [
        ("After", task.depends_on.join(", ")),
        ("Over", task.over.clone()),
        ("When", task.when.clone()),
        ("Session", task.session.clone()),
        ("Timeout", task.timeout.clone()),
        ("Emits", task.emits.join(", ")),
        ("Files", task.files.join(", ")),
        (
            "Reused",
            task.result
                .as_ref()
                .and_then(|r| r.reused_from.clone())
                .unwrap_or_default(),
        ),
    ] {
        if !value.is_empty() {
            grid = grid.child(fact_row(label, value, p));
        }
    }
    panel = panel.child(grid);

    if !task.instances.is_empty() {
        panel = panel
            .child(section("Instances", p))
            .child(instances(task, p));
    }
    let repairs: Vec<_> = task
        .result
        .iter()
        .chain(task.instances.iter().filter_map(|i| i.result.as_ref()))
        .flat_map(|r| r.repairs.iter())
        .collect();
    if !repairs.is_empty() {
        let mut list = col().w_full().gap(4.0);
        for repair in repairs {
            let mut item = col().w_full().gap(2.0).child(
                text(format!("{} \u{b7} ${:.4}", repair.label, repair.cost_usd))
                    .font(mono(10.5, 500.0))
                    .color(p.c.text_base),
            );
            for note in &repair.notes {
                item = item.child(
                    text(format!("\u{2022} {note}"))
                        .font(mono(10.0, 400.0))
                        .color(p.c.text_muted)
                        .wrap(),
                );
            }
            list = list.child(item);
        }
        panel = panel.child(section("Repairs", p)).child(list);
    }
    let links = task.links();
    if !links.is_empty() {
        let mut list = col().w_full().gap(3.0);
        for link in links {
            list = list.child(
                row()
                    .w_full()
                    .gap(6.0)
                    .center_y()
                    .child(link_chip(link, p))
                    .child(
                        text(link.kind.replace('_', " "))
                            .font(mono(10.0, 400.0))
                            .color(p.c.text_muted),
                    )
                    .child(
                        text(link.url.clone())
                            .font(mono(10.0, 400.0))
                            .color(p.c.text_muted)
                            .grow(1.0)
                            .min_w(0.0)
                            .truncate()
                            .selectable(true)
                            .selection_color(p.k.selection()),
                    ),
            );
        }
        panel = panel.child(section("Links", p)).child(list);
    }
    if let Some(output) = task.result.as_ref().and_then(|r| r.output.as_ref()) {
        let mut pretty = serde_json::to_string_pretty(output).unwrap_or_default();
        if pretty.len() > OUTPUT_CHARS {
            let cut = pretty
                .char_indices()
                .nth(OUTPUT_CHARS)
                .map_or(pretty.len(), |(at, _)| at);
            pretty.truncate(cut);
            pretty.push_str("\n\u{2026}");
        }
        panel = panel.child(section("Output", p)).child(
            col()
                .w_full()
                .max_h(260.0)
                .padding(10.0)
                .radius(p.k.radius.md)
                .fill(p.c.bg_base)
                .border(p.hair, p.c.border_muted)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_xy()
                .child(
                    text(pretty)
                        .font(mono(10.0, 400.0))
                        .color(p.c.text_base)
                        .selectable(true)
                        .selection_color(p.k.selection()),
                ),
        );
    }
    let name = task.name.clone();
    panel.child(
        row()
            .w_full()
            .gap(6.0)
            .child(button("Show in log", p, move |app: &mut App| {
                app.filter_log(Some(&name))
            })),
    )
}

fn instances(task: &TaskView, p: &Paint) -> El {
    let mut list = col().w_full().gap(2.0);
    for instance in task.instances.iter().take(INSTANCE_CHIPS) {
        list = list.child(instance_row(&task.name, instance, p));
    }
    if task.instances.len() > INSTANCE_CHIPS {
        list = list.child(
            text(format!("+{} more", task.instances.len() - INSTANCE_CHIPS))
                .font(mono(10.0, 400.0))
                .color(p.c.text_muted),
        );
    }
    list
}

fn instance_row(task: &str, instance: &Instance, p: &Paint) -> El {
    let state = instance.state();
    let full = format!("{task}[{}]", instance.key);
    let mut detail = Vec::new();
    if let Some(result) = &instance.result {
        if result.attempts > 1 {
            detail.push(format!("{} tries", result.attempts));
        }
        if result.cost_usd > 0.0 {
            detail.push(format!("${:.4}", result.cost_usd));
        }
        if let Some(why) = stopped_why(result) {
            detail.push(why);
        } else if !result.note.is_empty() {
            detail.push(result.note.clone());
        } else if let Some(out) = result.output.as_ref().and_then(output_headline) {
            detail.push(out);
        }
    } else if let Some(attempt) = instance.attempt {
        detail.push(format!("try {}", attempt.number));
    }
    row()
        .role("button")
        .label(format!("Show {full} in the log"))
        .w_full()
        .h(22.0)
        .px(6.0)
        .gap(7.0)
        .center_y()
        .radius(p.k.radius.sm)
        .hover(p.c.hover)
        .focusable(true)
        .on_press(move |app: &mut App| app.filter_log(Some(&full)))
        .child(
            text(instance.key.clone())
                .font(mono(10.5, 500.0))
                .color(p.c.text_base)
                .w(150.0)
                .truncate(),
        )
        .child(
            text(state_label(state))
                .font(mono(10.0, 400.0))
                .color(label_ink(state, p))
                .w(70.0),
        )
        .child(
            text(detail.join(" \u{b7} "))
                .font(mono(10.0, 400.0))
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        )
}

fn fact_row(label: &str, value: String, p: &Paint) -> El {
    row()
        .w_full()
        .gap(8.0)
        .child(
            text(label.to_string())
                .font(p.k.ty.small)
                .color(p.c.text_faint)
                .w(58.0)
                .pt(1.0),
        )
        .child(
            text(value)
                .font(mono(10.5, 400.0))
                .color(p.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        )
}

fn section(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.small_medium)
        .color(p.c.text_muted)
}

pub(crate) fn button(label: &str, p: &Paint, on_press: impl Fn(&mut App) + 'static) -> El {
    Button::new(label)
        .variant(ButtonVariant::Neutral)
        .control_size(ControlSize::Small)
        .on_press(on_press)
        .build(p.k)
}

pub(crate) fn duration(secs: f64) -> String {
    let total = secs.round() as u64;
    match total {
        0 => format!("{secs:.1}s"),
        t if t < 60 => format!("{t}s"),
        t if t < 3_600 => format!("{}m {:02}s", t / 60, t % 60),
        t => format!("{}h {:02}m", t / 3_600, t % 3_600 / 60),
    }
}

pub(crate) fn count(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::tasks::{count, duration, output_headline};
    use serde_json::json;

    #[test]
    fn a_route_pick_reads_as_its_label_and_confidence() {
        assert_eq!(
            output_headline(&json!({"kind": {"label": "go_stdlib", "confidence": 0.734}}))
                .as_deref(),
            Some("go_stdlib (0.73)")
        );
        assert_eq!(
            output_headline(&json!({"label": "yes"})).as_deref(),
            Some("yes")
        );
    }

    #[test]
    fn a_fanout_aggregate_leaves_the_headline_to_its_bar() {
        assert_eq!(
            output_headline(&json!({"instances": 4, "passed": 4, "failed": 0, "outputs": {}})),
            None
        );
    }

    #[test]
    fn other_outputs_list_their_fields_briefly() {
        assert_eq!(
            output_headline(&json!({"kernels": ["a", "b"], "seconds": 474, "flaws": {"x": 1}}))
                .as_deref(),
            Some("flaws={1}  kernels=[2]  seconds=474")
        );
        assert_eq!(output_headline(&json!([1, 2])), None);
    }

    #[test]
    fn numbers_read_short() {
        assert_eq!(count(999), "999");
        assert_eq!(count(31_009), "31.0k");
        assert_eq!(count(2_400_000), "2.4M");
        assert_eq!(duration(0.4), "0.4s");
        assert_eq!(duration(42.0), "42s");
        assert_eq!(duration(245.0), "4m 05s");
        assert_eq!(duration(7_380.0), "2h 03m");
    }
}

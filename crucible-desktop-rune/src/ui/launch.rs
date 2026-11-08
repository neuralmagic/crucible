//! The Launch tab: what a launch was dispatched with, and where its engine went.

use crux::dto::{LaunchRun, PlaybookLaunch};
use rune::{El, ScrollSpec, col, row, text};
use serde_json::Value;

use crate::ui::{Paint, SECTION, Tone, flag, mono};

pub(crate) fn launch_view(launch: Option<&PlaybookLaunch>, p: &Paint) -> El {
    let Some(detail) = launch else {
        return section_card(
            "Launch",
            col().child(
                text("The controller has not described this launch yet.")
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted),
            ),
            p,
        );
    };
    let record = &detail.launch;

    let mut params = col().w_full().gap(4.0);
    match record
        .params
        .as_object()
        .filter(|object| !object.is_empty())
    {
        Some(object) => {
            for (name, value) in object {
                params = params.child(field(name, render(value), p));
            }
        }
        None => {
            params = params.child(
                text("Launched with no params.")
                    .font(p.k.ty.ui)
                    .color(p.c.text_muted),
            );
        }
    }

    let agent = match (
        record.agent_provider.as_deref(),
        record.agent_model.as_deref(),
    ) {
        (Some(provider), Some(model)) => format!("{provider} \u{b7} {model}"),
        (None, Some(model)) => model.to_string(),
        (Some(provider), None) => provider.to_string(),
        (None, None) => "the playbook's default".to_string(),
    };
    let mut how = col()
        .w_full()
        .gap(8.0)
        .child(field("agent", agent, p))
        .child(field("max cost", format!("${:.2}", record.max_cost), p))
        .child(field(
            "max time",
            if record.max_time.is_empty() {
                "none".to_string()
            } else {
                record.max_time.clone()
            },
            p,
        ))
        .child(field(
            "origin",
            if record.origin.is_empty() {
                "launch".to_string()
            } else {
                record.origin.clone()
            },
            p,
        ));
    if let Some(version) = record.draft_version {
        how = how.child(field("draft", format!("version {version}"), p));
    }
    if let Some(schedule) = &record.schedule {
        how = how.child(field("schedule", schedule.clone(), p));
    }
    if record.advance_dedupe {
        how = how.child(field("dedupe", "advanced on success".to_string(), p));
    }
    if !detail.source_exists {
        how = how.child(field(
            "source",
            "deleted since; this launch keeps its snapshot".to_string(),
            p,
        ));
    }

    let mut schema =
        col()
            .w_full()
            .gap(4.0)
            .child(field("launched", short_digest(&record.schema_digest), p));
    if let Some(current) = &record.current_schema_digest {
        schema = schema.child(field("current", short_digest(current), p));
    }
    if record.schema_drifted {
        schema = schema.child(row().child(flag(
            "Params schema changed since",
            Tone::Warning,
            Some(&rune_theme_opencode::icons::WARNING),
            p,
        )));
    }

    let dispatch = &detail.dispatch;
    let mut engine = col()
        .w_full()
        .gap(8.0)
        .child(field("dispatch", dispatch.state.clone(), p));
    if dispatch.failures > 0 {
        engine = engine.child(field(
            "failed tries",
            match &dispatch.failed_at {
                Some(at) => format!("{}, last at {at}", dispatch.failures),
                None => dispatch.failures.to_string(),
            },
            p,
        ));
    }
    if let Some(failure) = &dispatch.failure {
        engine = engine.child(
            text(failure.clone())
                .font(mono(11.5, 400.0))
                .color(p.c.danger.fg)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        );
    }
    if record.transport_losses > 0 {
        engine = engine.child(field(
            "transport losses",
            format!(
                "{} task attempts lost to infrastructure",
                record.transport_losses
            ),
            p,
        ));
    }

    let mut runs = col().w_full().gap(6.0);
    if detail.runs.is_empty() {
        runs = runs.child(
            text("No run was dispatched.")
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
        );
    }
    for run in &detail.runs {
        runs = runs.child(run_row(run, p));
    }

    col()
        .key("launch")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(20.0)
        .pr(4.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(section_card("Params", params, p))
        .child(section_card("Launched with", how, p))
        .child(section_card("Params schema", schema, p))
        .child(section_card("Engine", engine, p))
        .child(section_card("Runs", runs, p))
}

fn run_row(run: &LaunchRun, p: &Paint) -> El {
    let name = run.name.clone().unwrap_or_else(|| run.run_id.clone());
    let place = match (&run.namespace, &run.pod) {
        (Some(namespace), Some(pod)) => format!("{} \u{b7} {namespace} \u{b7} {pod}", run.cluster),
        (Some(namespace), None) => format!("{} \u{b7} {namespace}", run.cluster),
        _ if run.dispatch == "local" => "local subprocess on the controller".to_string(),
        _ => run.cluster.clone(),
    };
    let cost = run
        .cost_usd
        .map(|cost| format!(" \u{b7} ${cost:.4}"))
        .unwrap_or_default();
    col()
        .w_full()
        .gap(2.0)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    text(name)
                        .font(mono(12.0, 500.0))
                        .color(p.c.text_base)
                        .selectable(true)
                        .selection_color(p.k.selection()),
                )
                .child(
                    text(format!("{}{cost}", run.status))
                        .font(p.k.ty.small)
                        .color(p.c.text_muted),
                ),
        )
        .child(
            text(format!("{place}  ({})", run.run_id))
                .font(mono(11.0, 400.0))
                .color(p.c.text_faint)
                .selectable(true)
                .selection_color(p.k.selection())
                .truncate(),
        )
}

fn field(label: &str, value: String, p: &Paint) -> El {
    row()
        .w_full()
        .gap(10.0)
        .child(
            text(label.to_string())
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .w(130.0),
        )
        .child(
            text(value)
                .font(mono(12.0, 400.0))
                .color(p.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        )
}

fn section_card(label: &str, body: El, p: &Paint) -> El {
    col()
        .w_full()
        .gap(8.0)
        .child(text(label.to_string()).font(SECTION).color(p.c.text_base))
        .child(
            col()
                .w_full()
                .px(16.0)
                .py(12.0)
                .radius(p.k.radius.lg)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .child(body),
        )
}

/// A param's value as a person reads it: strings bare, everything else as compact JSON.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn short_digest(digest: &str) -> String {
    match digest.split_once(':') {
        Some((algorithm, hex)) => {
            format!("{algorithm}:{}", hex.chars().take(16).collect::<String>())
        }
        None => digest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use crate::ui::launch::{render, short_digest};
    use serde_json::json;

    #[test]
    fn params_read_bare_when_they_are_strings() {
        assert_eq!(render(&json!("10.0")), "10.0");
        assert_eq!(render(&json!(3)), "3");
        assert_eq!(render(&json!(null)), "null");
        assert_eq!(render(&json!(["a", "b"])), "[\n  \"a\",\n  \"b\"\n]");
    }

    #[test]
    fn a_digest_keeps_its_algorithm_and_a_short_head() {
        assert_eq!(
            short_digest("sha256:8ceea827caf522601f27edab9d3766608e4f87a8"),
            "sha256:8ceea827caf52260"
        );
        assert_eq!(short_digest("plain"), "plain");
    }
}

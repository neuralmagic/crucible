//! The Transcript pane: the selected agent task's turns, step by step, with each tool call's
//! input and result a click away.

use rune::{El, ScrollSpec, col, icon, row, text};
use rune_theme_opencode::icons as oc;

use crate::App;
use crate::inspect::Pick;
use crate::transcript::{Step, Turn, turns, turns_from_log};
use crate::ui::data::{DATA_OPEN, faint, json_box, tree};
use crate::ui::tasks::count;
use crate::ui::{Paint, Pane};

/// Steps one turn draws; its last are kept.
const STEPS: usize = 400;
/// Characters of a tool result shown before it is cut.
const RESULT_CHARS: usize = 8_000;
const INDENT: f32 = 18.0;

pub(crate) fn transcript_view(pane: &Pane, app: &App, p: &Paint) -> El {
    let shell = |body: El| {
        col()
            .key("transcript-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .scroll_edge_fade(8.0)
            .child(body.padding(14.0))
    };
    let Some(task) = app.run.expanded.as_deref() else {
        return shell(col().child(faint("No task selected", p)));
    };
    let mut all = turns(pane.agent_events, task);
    if all.is_empty() {
        all = turns_from_log(pane.agent_lines(), task);
    }
    if all.is_empty() {
        return shell(col().child(faint(&format!("{task} recorded no agent turns"), p)));
    }
    let mut speakers: Vec<&str> = all.iter().map(|t| t.task).collect();
    speakers.dedup();
    let picked = match &app.run.inspect.pick {
        Some(Pick::Instance(key)) => speakers.iter().copied().find(|s| {
            s.strip_prefix(task)
                .is_some_and(|rest| rest == format!("[{key}]"))
        }),
        _ => None,
    }
    .unwrap_or(speakers[0]);
    let mut body = col().w_full().gap(10.0);
    if speakers.len() > 1 {
        body = body.child(instance_chips(task, &speakers, picked, p));
    }
    for turn in all.iter().filter(|t| t.task == picked) {
        body = body.child(turn_view(turn, app, p));
    }
    shell(body)
}

fn instance_chips(task: &str, speakers: &[&str], picked: &str, p: &Paint) -> El {
    let mut chips = row().w_full().gap(4.0).wrap();
    for speaker in speakers {
        let on = *speaker == picked;
        let key = speaker
            .strip_prefix(task)
            .and_then(|rest| rest.strip_prefix('['))
            .and_then(|rest| rest.strip_suffix(']'))
            .map(ToString::to_string);
        let label = key.clone().unwrap_or_else(|| speaker.to_string());
        chips = chips.child(
            row()
                .key(format!("turn-of-{speaker}"))
                .role("button")
                .label(format!("Show {speaker}"))
                .h(20.0)
                .px(7.0)
                .center_y()
                .radius(p.k.radius.sm)
                .fill(if on { p.c.bg_accent } else { p.c.layer_02 })
                .border(p.hair, p.c.border_muted)
                .hover(p.c.hover)
                .focusable(true)
                .on_press(move |app: &mut App| {
                    app.run.inspect.pick = key.clone().map(Pick::Instance);
                })
                .child(text(label).font(p.mono(10.0, 500.0)).color(if on {
                    p.c.text_base
                } else {
                    p.c.text_muted
                })),
        );
    }
    chips
}

pub(crate) fn turn_view(turn: &Turn, app: &App, p: &Paint) -> El {
    let (tools, failed) = turn.tools();
    let mut facts = vec![format!("attempt {}", turn.attempt)];
    if let Some(model) = turn.model {
        facts.push(model.to_string());
    }
    facts.push(format!("{tools} tool calls"));
    if failed > 0 {
        facts.push(format!("{failed} failed"));
    }
    if let Some([_, _, _, total]) = turn.tokens {
        facts.push(format!("{} tokens", count(total)));
    }
    let mut head = row().w_full().gap(8.0).center_y().child(
        text(facts.join(" \u{b7} "))
            .font(p.mono(11.0, 600.0))
            .color(p.c.text_base)
            .grow(1.0)
            .min_w(0.0)
            .truncate(),
    );
    if let Some(ending) = &turn.ending {
        let mut said = format!("{} \u{b7} ${:.4}", ending.subtype, ending.cost_usd);
        if let Some(error) = ending.error.as_ref().filter(|_| ending.failed) {
            said.push_str(&format!(" \u{b7} {error}"));
        }
        head = head.child(
            text(said)
                .font(p.mono(10.5, 500.0))
                .color(if ending.failed { p.fail } else { p.pass }),
        );
    }
    let mut card = col()
        .key(format!("turn-{}-{}", turn.task, turn.attempt))
        .w_full()
        .gap(4.0)
        .padding(10.0)
        .radius(p.k.radius.md)
        .fill(p.c.bg_base)
        .border(p.hair, p.c.border_muted)
        .child(head);
    let skipped = turn.steps.len().saturating_sub(STEPS);
    if skipped > 0 {
        card = card.child(faint(&format!("{skipped} earlier steps not drawn"), p));
    }
    for (i, step) in turn.steps.iter().enumerate().skip(skipped) {
        let path = format!("turn:{}:{}:{i}", turn.task, turn.attempt);
        card = card.child(step_view(step, &path, app, p));
    }
    card
}

fn step_view(step: &Step, path: &str, app: &App, p: &Paint) -> El {
    let open = app.run.inspect.is_open(path, 1, 1);
    let toggle = |head: El, path: &str| {
        let path = path.to_string();
        head.role("button")
            .label(if open { "Fold" } else { "Open" })
            .radius(p.k.radius.sm)
            .hover(p.c.hover)
            .on_press(move |app: &mut App| app.run.inspect.flip(&path))
            .child(
                icon(
                    if open {
                        &oc::CHEVRON_DOWN
                    } else {
                        &oc::CHEVRON_RIGHT
                    },
                    11.0,
                )
                .color(p.c.icon_faint),
            )
    };
    let line = || {
        row()
            .key(path.to_string())
            .w_full()
            .h(22.0)
            .gap(7.0)
            .center_y()
    };
    match step {
        Step::Said(said) => text(said.to_string())
            .font(p.k.ty.ui)
            .color(p.c.text_base)
            .wrap()
            .selectable(true)
            .selection_color(p.k.selection())
            .py(4.0),
        Step::Thought {
            text: thought,
            chars,
        } => {
            let preview = match chars {
                Some(chars) => format!("{chars} chars"),
                None => first_line(thought),
            };
            let head = toggle(line(), path)
                .child(
                    text("thinking".to_string())
                        .font(p.mono(10.5, 500.0))
                        .color(p.c.text_faint),
                )
                .child(
                    text(preview)
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_faint)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                );
            let mut block = col().w_full().child(head);
            if open {
                block = block.child(
                    text(thought.to_string())
                        .font(p.k.ty.small)
                        .color(p.c.text_muted)
                        .wrap()
                        .selectable(true)
                        .pl(INDENT),
                );
            }
            block
        }
        Step::Tool {
            name,
            summary,
            input,
            result,
            failed,
            subagent,
        } => {
            let mut head = toggle(line(), path)
                .child(
                    text(name.to_string())
                        .font(p.mono(10.5, 600.0))
                        .color(if *subagent {
                            p.c.text_accent
                        } else {
                            p.c.text_base
                        }),
                )
                .child(
                    text(first_line(summary))
                        .font(p.mono(10.5, 400.0))
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                );
            if *failed {
                head = head.child(
                    text("failed".to_string())
                        .font(p.mono(10.5, 500.0))
                        .color(p.fail),
                );
            }
            let mut block = col().w_full().child(head);
            if open {
                let mut detail = col().w_full().gap(4.0).pl(INDENT);
                if summary.contains('\n') {
                    detail = detail.child(
                        text(summary.to_string())
                            .font(p.mono(10.5, 400.0))
                            .color(p.c.text_muted)
                            .wrap()
                            .selectable(true),
                    );
                }
                if let Some(input) = input {
                    detail = detail.child(json_box(
                        tree(input, &format!("{path}:in"), DATA_OPEN, &app.run.inspect, p),
                        p,
                    ));
                }
                if let Some(result) = result {
                    detail = detail.child(result_box(result, p));
                }
                if input.is_none() && result.is_none() {
                    detail = detail.child(faint("Summary only", p));
                }
                block = block.child(detail);
            }
            block
        }
        Step::Retry(said) | Step::Error(said) => text(said.clone())
            .font(p.mono(10.5, 400.0))
            .color(p.fail)
            .wrap()
            .py(2.0),
        Step::Note(said) => text(said.clone())
            .font(p.mono(10.0, 400.0))
            .color(p.c.text_faint)
            .truncate()
            .py(1.0),
    }
}

fn result_box(result: &str, p: &Paint) -> El {
    let mut shown: String = result.chars().take(RESULT_CHARS).collect();
    if result.chars().count() > RESULT_CHARS {
        shown.push_str("\n\u{2026}");
    }
    col()
        .w_full()
        .max_h(280.0)
        .padding(8.0)
        .radius(p.k.radius.md)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_xy()
        .child(
            text(shown)
                .font(p.mono(10.5, 400.0))
                .color(p.c.text_base)
                .selectable(true)
                .selection_color(p.k.selection()),
        )
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

//! A send-back loop's rounds on a task card.

use crate::App;
use crate::session::{Outcome, Round, TaskView, sent_back};
use crate::ui::Paint;
use crate::ui::tasks::{
    duration, label_ink, output_headline, state_label, state_mark, stopped_why,
};
use rune::{El, col, icon, row, text};
use rune_theme_opencode::icons as oc;

/// Why a result stopped, else its note, else what it emitted.
pub(in crate::ui::tasks) fn said(result: &Outcome) -> Option<String> {
    stopped_why(result)
        .or_else(|| (!result.note.is_empty()).then(|| result.note.clone()))
        .or_else(|| result.output.as_ref().and_then(output_headline))
}

/// `round 2 of 3` once a round started, `up to 3 rounds` before.
pub(in crate::ui::tasks) fn round_of(task: &TaskView, max_rounds: u32) -> String {
    match task.rounds.last() {
        Some(round) => format!("round {} of {max_rounds}", round.n),
        None => format!("up to {max_rounds} rounds"),
    }
}

/// What the loop's rounds spent so far.
pub(in crate::ui::tasks) fn round_spend(task: &TaskView) -> f64 {
    task.rounds
        .iter()
        .filter_map(|round| round.result.as_ref())
        .map(|result| result.cost_usd.max(0.0))
        .sum()
}

/// The loop's rounds in order, each with its state, then the rounds it may still run.
pub(in crate::ui::tasks) fn round_strip(task: &TaskView, max_rounds: u32, p: &Paint) -> El {
    let mut strip = row()
        .key(format!("rounds-{}", task.name))
        .w_full()
        .gap(4.0)
        .center_y();
    let settled = task.result.is_some();
    for n in 1..=max_rounds.max(task.rounds.last().map_or(0, |r| r.n)) {
        if n > 1 {
            strip = strip.child(icon(&oc::CHEVRON_RIGHT, 10.0).color(p.c.icon_faint));
        }
        let Some(round) = task.round(n) else {
            strip = strip.child(
                text(format!("{n}"))
                    .font(p.mono(10.5, 400.0))
                    .color(p.c.text_faint.alpha(if settled { 0.5 } else { 1.0 }))
                    .px(5.0),
            );
            continue;
        };
        let state = round.state();
        let full = round.name(&task.name);
        strip = strip.child(
            row()
                .key(format!("round-chip-{full}"))
                .role("button")
                .label(format!("Show {full} in the log"))
                .help(format!("round {n}: {}", state_label(state)))
                .h(20.0)
                .px(5.0)
                .gap(4.0)
                .center_y()
                .radius(p.k.radius.sm)
                .fill(p.c.layer_02)
                .border(p.hair, p.c.border_muted)
                .hover(p.c.hover)
                .focusable(true)
                .on_press(move |app: &mut App| app.filter_log(Some(&full)))
                .child(state_mark(state, p).scale(0.7))
                .child(
                    text(format!("{n}"))
                        .font(p.mono(10.5, 500.0))
                        .color(label_ink(state, p)),
                ),
        );
    }
    strip
}

pub(in crate::ui::tasks) fn rounds(task: &TaskView, tasks: &[TaskView], p: &Paint) -> El {
    let mut list = col().w_full().gap(4.0);
    for round in &task.rounds {
        list = list.child(round_row(task, tasks, round, p));
    }
    list
}

fn round_row(task: &TaskView, tasks: &[TaskView], round: &Round, p: &Paint) -> El {
    let state = round.state();
    let full = round.name(&task.name);
    let mut detail = Vec::new();
    if let Some(result) = &round.result {
        if result.attempts > 1 {
            detail.push(format!("{} tries", result.attempts));
        }
        if result.secs > 0.0 {
            detail.push(duration(result.secs));
        }
        detail.push(format!("${:.4}", result.cost_usd));
        detail.extend(said(result));
    } else if let Some(attempt) = round.attempt {
        detail.push(format!("try {}", attempt.number));
    }
    let head = row()
        .key(format!("round-{full}"))
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
        .child(state_mark(state, p).scale(0.8))
        .child(
            text(format!("round {}", round.n))
                .font(p.mono(10.5, 500.0))
                .color(p.c.text_base)
                .w(64.0),
        )
        .child(
            text(state_label(state))
                .font(p.mono(10.0, 400.0))
                .color(label_ink(state, p))
                .w(70.0),
        )
        .child(
            text(detail.join(" \u{b7} "))
                .font(p.mono(10.0, 400.0))
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate(),
        );
    let Some((by, why)) = task.revised.as_ref().zip(sent_back(tasks, task, round.n)) else {
        return head;
    };
    let reason = [
        stopped_why(why).or_else(|| (!why.note.is_empty()).then(|| why.note.clone())),
        why.output.as_ref().and_then(output_headline),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" \u{b7} ");
    col().w_full().gap(1.0).child(head).child(
        row()
            .w_full()
            .gap(6.0)
            .pl(29.0)
            .child(
                text("\u{21a9}")
                    .font(p.mono(10.0, 400.0))
                    .color(p.c.icon_faint),
            )
            .child(
                text(format!("sent back by {}: {reason}", by.by))
                    .font(p.mono(10.0, 400.0))
                    .color(p.fail.mix(p.c.text_muted, 0.4))
                    .grow(1.0)
                    .min_w(0.0)
                    .max_lines(3)
                    .wrap(),
            ),
    )
}

//! The Decisions page: decision requests waiting on the signed-in user, and for the one picked
//! its evidence, its questions and the answer form.

use rune::{El, Tx, col, row, text};

use rune_kit::{Button, ButtonVariant, Markdown, SidebarRow, TextInput};

use crate::admin::{Action, relative};
use crate::ui::desk::{card, frame, heading, landed, list, none, now, reading};
use crate::ui::{Paint, Tone, flag, tag};
use crate::{App, Selection};

const LIST: f32 = 300.0;
/// Characters of an upstream task's output shown before it is cut.
const OUTPUT: usize = 1_200;

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let waiting = app.waiting();
    let note = if waiting > 0 {
        format!("{waiting} waiting")
    } else {
        String::new()
    };
    let page = frame("decisions", "Decisions", note, None, app, p);
    let open = match landed(&app.remote.admin.decisions, "the decision queue", p) {
        Err(standin) => return page.child(*standin),
        Ok(open) => open,
    };
    let now = now();
    let rows: Vec<El> = open
        .iter()
        .map(|decision| {
            let id = decision.id.clone();
            let mut about = vec![decision.questions.join(", ")];
            about.push(format!("expires {}", relative(&decision.expires_at, now)));
            SidebarRow::new(decision.task.clone())
                .subtitle(about.join(" \u{b7} "))
                .height(46.0)
                .selected(app.remote.desk.decision.as_deref() == Some(decision.id.as_str()))
                .on_select(move |app: &mut App, _: &mut Tx| app.remote.desk.pick_decision(&id))
                .build(p.k)
                .key(format!("decision-{}", decision.id))
        })
        .collect();
    let queue = if rows.is_empty() {
        none("decisions-none", "Nothing waiting on you", p)
    } else {
        list("decisions-list", rows, p)
    };
    let picked = match (&app.remote.desk.decision, &app.remote.admin.decision) {
        (Some(id), Some((read, loaded))) if id == read => match landed(loaded, "the request", p) {
            Ok(decision) => detail(decision, app, p),
            Err(standin) => *standin,
        },
        (Some(_), _) => reading("the request", p),
        (None, _) => none("decision-unpicked", "Pick a request", p),
    };
    page.child(
        row()
            .key("decisions-body")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .gap(16.0)
            .child(card("decisions-queue", queue, p).w(LIST).h_full())
            .child(
                col()
                    .key("decision-detail")
                    .grow(1.0)
                    .min_w(0.0)
                    .h_full()
                    .min_h(0.0)
                    .child(picked),
            ),
    )
}

fn detail(decision: &crux::dto::Decision, app: &App, p: &Paint) -> El {
    let mut body = col()
        .key("decision-body")
        .w_full()
        .gap(16.0)
        .child(head(decision, p));
    if let Some(review) = decision.review.as_deref().filter(|r| !r.trim().is_empty()) {
        body = body.child(card(
            "decision-review",
            Markdown::new(review.to_string())
                .selectable(true)
                .build(p.k),
            p,
        ));
    }
    if !decision.gated.is_empty() {
        body = body.child(card("decision-gated", gated(decision, p), p));
    }
    for input in &decision.inputs {
        body = body.child(card(
            &format!("decision-input-{}", input.task),
            upstream(input, p),
            p,
        ));
    }
    body = body.child(card("decision-answer", answer(decision, app, p), p));
    let scroller = col()
        .key("decision-scroll")
        .w_full()
        .h_full()
        .min_h(0.0)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(body);
    p.k.scroll_bar(scroller).w_full().h_full().min_h(0.0)
}

fn head(decision: &crux::dto::Decision, p: &Paint) -> El {
    let now = now();
    let mut facts = vec![format!(
        "${:.2} of ${:.2}",
        decision.spent_usd, decision.max_cost_usd
    )];
    let elapsed = decision.elapsed_secs / 60;
    facts.push(match decision.max_time_secs {
        Some(cap) => format!("{elapsed}m of {}m", cap / 60),
        None => format!("{elapsed}m"),
    });
    facts.push(format!("opened {}", relative(&decision.opened_at, now)));
    if decision.state == "open" {
        facts.push(format!("expires {}", relative(&decision.expires_at, now)));
    }
    let state = match decision.state.as_str() {
        "open" => flag("open", Tone::Warning, None, p),
        "answered" => flag("answered", Tone::Success, None, p),
        other => flag(other, Tone::Danger, None, p),
    };
    let run = decision.launch_key.clone().and_then(|key| {
        let playbook = key.split(':').nth(1)?.to_string();
        Some(
            Button::new("Open run")
                .variant(ButtonVariant::Neutral)
                .on_press(move |app: &mut App| {
                    app.select(Selection::Live {
                        playbook: playbook.clone(),
                        launch: Some(key.clone()),
                    })
                })
                .build(p.k)
                .key("decision-open-run"),
        )
    });
    row()
        .w_full()
        .gap(10.0)
        .center_y()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(
                    row()
                        .gap(8.0)
                        .center_y()
                        .child(
                            text(decision.task.clone())
                                .font(p.k.ty.ui_medium)
                                .color(p.c.text_base),
                        )
                        .child(state),
                )
                .child(
                    text(facts.join(" \u{b7} "))
                        .font(p.k.ty.small)
                        .color(p.c.text_muted),
                ),
        )
        .children(run)
}

/// What each answer lets run.
fn gated(decision: &crux::dto::Decision, p: &Paint) -> El {
    let mut column = col().w_full().gap(6.0).child(heading("Gates", p));
    for gate in &decision.gated {
        let when = if gate.labels.is_empty() {
            gate.question.clone()
        } else {
            format!("{} = {}", gate.question, gate.labels.join(" | "))
        };
        column = column.child(
            row()
                .gap(10.0)
                .center_y()
                .child(
                    text(gate.name.clone())
                        .font(p.mono(12.0, 520.0))
                        .color(p.c.text_base),
                )
                .children((!gate.kind.is_empty()).then(|| tag(&gate.kind, p)))
                .child(
                    text(format!("runs when {when}"))
                        .font(p.k.ty.small)
                        .color(p.c.text_muted),
                ),
        );
    }
    column
}

/// An upstream task's result the request was opened on.
fn upstream(input: &crux::dto::DecisionInput, p: &Paint) -> El {
    let output = input
        .output
        .as_ref()
        .map(|value| serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()))
        .unwrap_or_default();
    let shown = match output.char_indices().nth(OUTPUT) {
        Some((cut, _)) => format!("{}\u{2026}", &output[..cut]),
        None => output,
    };
    col()
        .w_full()
        .gap(6.0)
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(
                    text(input.task.clone())
                        .font(p.k.ty.ui_medium)
                        .color(p.c.text_base),
                )
                .children((!input.status.is_empty()).then(|| tag(&input.status, p))),
        )
        .children((!shown.is_empty()).then(|| {
            text(shown)
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_muted)
                .wrap()
                .selectable(true)
                .selection_color(p.k.selection())
        }))
}

fn answer(decision: &crux::dto::Decision, app: &App, p: &Paint) -> El {
    if let Some(answered) = &decision.answer {
        let labels: Vec<String> = answered
            .labels
            .iter()
            .map(|(q, values)| format!("{q}: {}", values.join(", ")))
            .collect();
        return col()
            .w_full()
            .gap(6.0)
            .child(heading("Answer", p))
            .child(
                text(labels.join(" \u{b7} "))
                    .font(p.k.ty.ui)
                    .color(p.c.text_base),
            )
            .child(
                text(format!(
                    "{} \u{b7} {}",
                    answered.decided_by,
                    relative(&answered.decided_at, now())
                ))
                .font(p.k.ty.small)
                .color(p.c.text_muted),
            )
            .children(
                answered
                    .note
                    .clone()
                    .map(|note| text(note).font(p.k.ty.ui).color(p.c.text_muted).wrap()),
            );
    }
    let open = decision.state == "open" && decision.can_answer;
    let mut column = col().w_full().gap(12.0);
    for question in &decision.questions {
        column = column.child(question_row(question, open, app, p));
    }
    if !open {
        return column.child(none(
            "decision-closed",
            if decision.state == "open" {
                "You may not answer this one"
            } else {
                "Closed"
            },
            p,
        ));
    }
    let missing = app.remote.desk.unanswered(decision);
    let ready = missing.is_empty() && !app.remote.desk.busy();
    let (id, digest) = (decision.id.clone(), decision.evidence_digest.clone());
    let send = move |app: &mut App| {
        let note = app.remote.desk.note.text.trim().to_string();
        let labels = app
            .remote
            .desk
            .picks
            .iter()
            .filter(|(_, picked)| !picked.is_empty())
            .map(|(q, picked)| (q.clone(), picked.clone()))
            .collect();
        app.act(Action::Answer {
            id: id.clone(),
            labels,
            digest: digest.clone(),
            note: (!note.is_empty()).then_some(note),
        });
    };
    column
        .child(
            TextInput::new(&app.remote.desk.note)
                .placeholder("Note")
                .on_edit(|app: &mut App| &mut app.remote.desk.note)
                .build(p.k)
                .key("decision-note"),
        )
        .child(
            row()
                .w_full()
                .gap(10.0)
                .center_y()
                .child(
                    text(if missing.is_empty() {
                        String::new()
                    } else {
                        format!("No answer for {}", missing.join(", "))
                    })
                    .font(p.k.ty.small)
                    .color(p.c.text_muted)
                    .grow(1.0),
                )
                .child(
                    Button::new("Answer")
                        .variant(ButtonVariant::Submit)
                        .disabled(!ready)
                        .on_press(send)
                        .build(p.k)
                        .key("decision-send"),
                ),
        )
}

fn question_row(question: &crux::dto::DecisionQuestion, open: bool, app: &App, p: &Paint) -> El {
    let picked = app.remote.desk.picks.get(&question.id);
    let mut options = row().gap(6.0).flex_wrap();
    for label in &question.options {
        let on = picked.is_some_and(|picked| picked.contains(label));
        let (q, l, multiple) = (question.id.clone(), label.clone(), question.multiple);
        options = options.child(
            Button::new(label.clone())
                .variant(if on {
                    ButtonVariant::Submit
                } else {
                    ButtonVariant::Outline
                })
                .disabled(!open)
                .on_press(move |app: &mut App| app.remote.desk.toggle_pick(&q, &l, multiple))
                .build(p.k)
                .key(format!("pick-{}-{label}", question.id)),
        );
    }
    col()
        .key(format!("question-{}", question.id))
        .w_full()
        .gap(6.0)
        .child(
            row()
                .gap(8.0)
                .center_y()
                .child(
                    text(question.id.clone())
                        .font(p.mono(12.0, 560.0))
                        .color(p.c.text_base),
                )
                .children(question.multiple.then(|| tag("any number", p))),
        )
        .children(question.instructions.clone().map(|instructions| {
            text(instructions)
                .font(p.k.ty.ui)
                .color(p.c.text_muted)
                .wrap()
        }))
        .child(options)
}

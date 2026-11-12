//! The drafting agent's pane: each prompt, the turn it started drawn as the
//! Transcript pane draws one, what the session did beside it, and a prompt to send.

use rune::{El, ScrollSpec, TextStyle, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Spinner, TextInput};

use crate::App;
use crate::agent::{Entry, SETUP_GUIDE, SPEAKER, State};
use crate::settings::LogFont;
use crate::transcript::turns;
use crate::ui::Paint;
use crate::ui::transcript::turn_view;

/// Turns drawn, newest last.
const SHOWN: usize = 12;

fn status(app: &App, p: &Paint) -> El {
    let desk = &app.remote.agent;
    let (label, ink) = match &desk.state {
        None => ("No session".to_string(), p.c.text_faint),
        Some(State::Starting(what)) => (what.clone(), p.live),
        Some(State::Running) => ("Working".to_string(), p.live),
        Some(State::Idle) => ("Ready".to_string(), p.pass),
        Some(State::NotSetUp(why) | State::Failed(why)) => (why.clone(), p.fail),
    };
    let mut bar = row().w_full().h(24.0).gap(8.0).center_y();
    if desk.busy() {
        bar = bar.child(Spinner::new().size(12.0).build(p.k));
    }
    bar = bar.child(
        text(label.clone())
            .font(p.k.ty.small)
            .color(ink)
            .grow(1.0)
            .min_w(0.0)
            .truncate()
            .help_when_truncated(label)
            .key("agent-state"),
    );
    if matches!(desk.state, Some(State::Running)) {
        bar = bar.child(
            Button::new("Stop")
                .variant(ButtonVariant::Neutral)
                .on_press(|app: &mut App| app.stop_agent())
                .build(p.k)
                .key("agent-stop"),
        );
    }
    if matches!(desk.state, Some(State::NotSetUp(_))) {
        bar = bar.child(
            Button::new("Setup guide")
                .variant(ButtonVariant::Neutral)
                .on_press(|app: &mut App| {
                    app.editor_error = crate::editor::open_url(SETUP_GUIDE).err();
                })
                .build(p.k)
                .key("agent-setup-guide"),
        );
    }
    if desk.state.is_some() {
        bar = bar.child(
            Button::new("Reset")
                .variant(ButtonVariant::Neutral)
                .on_press(|app: &mut App| app.reset_agent())
                .build(p.k)
                .help("New sandbox")
                .key("agent-reset"),
        );
        bar = bar.child(
            Button::new("End")
                .variant(ButtonVariant::Neutral)
                .on_press(|app: &mut App| app.end_agent())
                .build(p.k)
                .help("Delete the sandbox")
                .key("agent-end"),
        );
    }
    bar
}

fn entry_row(entry: &Entry, style: TextStyle, p: &Paint) -> El {
    let (tag, body, ink) = match entry {
        Entry::Asked { text, .. } => ("YOU", text, p.accent),
        Entry::Note { text, .. } => ("SESSION", text, p.c.text_muted),
    };
    row()
        .w_full()
        .gap(6.0)
        .py(1.0)
        .child(
            text(tag)
                .font(TextStyle::new(9.5, 600.0).lh(12.0).tracking(0.4))
                .color(ink)
                .w(52.0),
        )
        .child(
            text(body.clone())
                .font(style)
                .color(ink)
                .grow(1.0)
                .min_w(0.0)
                .selectable(true)
                .selection_color(p.k.selection())
                .wrap(),
        )
}

/// The prompt waiting for the turn to end, with the ways out of waiting.
fn queued(prompt: &str, style: TextStyle, p: &Paint) -> El {
    row()
        .key("agent-queued")
        .w_full()
        .gap(8.0)
        .center_y()
        .child(
            text("QUEUED")
                .font(TextStyle::new(9.5, 600.0).lh(12.0).tracking(0.4))
                .color(p.live)
                .w(52.0),
        )
        .child(
            text(prompt.to_string())
                .font(style)
                .color(p.c.text_base)
                .grow(1.0)
                .min_w(0.0)
                .truncate()
                .help_when_truncated(prompt.to_string()),
        )
        .child(
            Button::new("Send now")
                .variant(ButtonVariant::Neutral)
                .on_press(|app: &mut App| app.send_agent_now())
                .build(p.k)
                .help("Stop the turn and run this")
                .key("agent-send-now"),
        )
        .child(
            Button::new("Drop")
                .variant(ButtonVariant::Neutral)
                .on_press(|app: &mut App| app.drop_queued_prompt())
                .build(p.k)
                .key("agent-drop-queued"),
        )
}

pub(crate) fn panel(font: LogFont, app: &App, p: &Paint) -> El {
    let desk = &app.remote.agent;
    let style = p.sized(TextStyle::new(11.5, 400.0).lh(18.0).family(font.family()));
    let agent_turns = turns(&desk.steps, SPEAKER);
    let last = desk
        .entries
        .iter()
        .map(Entry::turn)
        .chain(agent_turns.iter().map(|turn| turn.attempt))
        .max()
        .unwrap_or(0);
    let first = last.saturating_sub(SHOWN as u32 - 1);
    let mut entries = col().w_full().gap(6.0).py(8.0);
    for n in std::iter::once(0).chain(first.max(1)..=last) {
        let beside = |asked: bool| {
            desk.entries
                .iter()
                .filter(move |entry| {
                    entry.turn() == n && matches!(entry, Entry::Asked { .. }) == asked
                })
                .map(|entry| entry_row(entry, style, p))
        };
        entries = entries.children(beside(true));
        entries = entries.children(
            agent_turns
                .iter()
                .filter(|turn| turn.attempt == n)
                .map(|turn| turn_view(turn, app, p)),
        );
        entries = entries.children(beside(false));
    }
    let blocked = app.agent_blocked();
    let send = Button::new("Send")
        .variant(ButtonVariant::Submit)
        .disabled(blocked.is_some())
        .on_press(|app: &mut App| app.send_agent())
        .build(p.k)
        .key("agent-send");
    let send = match blocked {
        Some(why) => send.help(why),
        None => send,
    };
    col()
        .key("agent")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(6.0)
        .child(status(app, p))
        .child(
            col()
                .key("agent-log")
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .px(12.0)
                .radius(p.k.radius.lg)
                .fill(p.c.layer_01)
                .border(p.hair, p.c.border_muted)
                .scroll(ScrollSpec { stick_bottom: true })
                .scroll_edge_fade(12.0)
                .child(entries),
        )
        .children(
            desk.queued
                .as_deref()
                .map(|prompt| queued(prompt, style, p)),
        )
        .child(
            row()
                .w_full()
                .gap(8.0)
                .center_y()
                .child(
                    el().grow(1.0).min_w(0.0).child(
                        TextInput::new(&desk.prompt)
                            .placeholder("Change this draft")
                            .width(None)
                            .on_edit(|app: &mut App| &mut app.remote.agent.prompt)
                            .on_submit(|app: &mut App, _: &mut Tx| app.send_agent())
                            .build(p.k)
                            .key("agent-prompt"),
                    ),
                )
                .child(send),
        )
}

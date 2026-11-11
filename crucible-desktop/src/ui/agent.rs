//! The drafting agent's panel in the Pack tab: what it did, and a prompt to send it.

use rune::{El, ScrollSpec, TextStyle, Tx, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Spinner, TextInput};

use crate::App;
use crate::agent::{Entry, State};
use crate::settings::LogFont;
use crate::ui::Paint;
use crate::ui::log::line_row;

/// Entries drawn, newest last.
const SHOWN: usize = 600;
const HEIGHT: f32 = 300.0;

fn status(app: &App, p: &Paint) -> El {
    let desk = &app.remote.agent;
    let (label, ink) = match &desk.state {
        None => ("No session".to_string(), p.c.text_faint),
        Some(State::Starting(what)) => (what.clone(), p.live),
        Some(State::Running) => ("Working".to_string(), p.live),
        Some(State::Idle) => ("Ready".to_string(), p.pass),
        Some(State::Failed(why)) => (why.clone(), p.fail),
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
    if desk.state.is_some() {
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
    let said = |tag: &'static str, body: &str, ink| {
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
                text(body.to_string())
                    .font(style)
                    .color(ink)
                    .grow(1.0)
                    .min_w(0.0)
                    .selectable(true)
                    .selection_color(p.k.selection())
                    .wrap(),
            )
    };
    match entry {
        Entry::Asked(prompt) => said("YOU", prompt, p.accent),
        Entry::Note(note) => said("SESSION", note, p.c.text_muted),
        Entry::Agent(line) => line_row(line, style, true, p),
    }
}

pub(crate) fn panel(font: LogFont, app: &App, p: &Paint) -> El {
    let desk = &app.remote.agent;
    let style = p.sized(TextStyle::new(11.5, 400.0).lh(18.0).family(font.family()));
    let mut entries = col().gap(0.0).py(8.0);
    for entry in desk
        .entries
        .iter()
        .skip(desk.entries.len().saturating_sub(SHOWN))
    {
        entries = entries.child(entry_row(entry, style, p));
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
        .h(HEIGHT)
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

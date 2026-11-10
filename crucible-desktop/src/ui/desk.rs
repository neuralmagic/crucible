//! What the management pages share: their frame, read states, and the last action's outcome.

use rune::{El, ScrollSpec, col, el, row, text};
use rune_kit::{Button, ButtonVariant, Spinner};

use crate::App;
use crate::admin::Loaded;
use crate::ui::{Paint, TITLE, banner};

/// A management page: its title and a note beside it, then `body`.
pub(crate) fn frame(
    key: &str,
    title: &str,
    note: String,
    action: Option<El>,
    app: &App,
    p: &Paint,
) -> El {
    col()
        .key("detail")
        .grow(1.0)
        .h_full()
        .min_h(0.0)
        .min_w(0.0)
        .px(28.0)
        .pb(20.0)
        .gap(16.0)
        .child(
            row()
                .key(format!("{key}-head"))
                .w_full()
                .h(52.0)
                .gap(10.0)
                .center_y()
                .window_drag()
                .child(
                    text(title.to_string())
                        .font(p.sized(TITLE))
                        .color(p.c.text_base),
                )
                .child(
                    text(note)
                        .font(p.k.ty.ui)
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .truncate(),
                )
                .children(action),
        )
        .children(outcome(app, p))
}

/// The newest action's outcome, or a spinner while one is on its way.
fn outcome(app: &App, p: &Paint) -> Option<El> {
    if app.remote.desk.busy() {
        return Some(
            row()
                .key("outcome")
                .gap(8.0)
                .center_y()
                .child(Spinner::new().size(12.0).color(p.c.text_muted).build(p.k))
                .child(
                    text("Saving\u{2026}")
                        .font(p.k.ty.small)
                        .color(p.c.text_muted),
                ),
        );
    }
    match app.remote.desk.outcome.as_ref()? {
        Ok(what) => Some(
            text(what.clone())
                .key("outcome")
                .font(p.k.ty.small)
                .color(p.pass),
        ),
        Err(why) => Some(banner(why, p).key("outcome")),
    }
}

/// What a read has to show: its value, or what stands in for it.
pub(crate) fn landed<'a, T>(
    loaded: &'a Loaded<T>,
    what: &str,
    p: &Paint,
) -> Result<&'a T, Box<El>> {
    match loaded {
        Some(Ok(value)) => Ok(value),
        Some(Err(why)) => Err(Box::new(banner(why, p))),
        None => Err(Box::new(reading(what, p))),
    }
}

/// A spinner over what is being read.
pub(crate) fn reading(what: &str, p: &Paint) -> El {
    row()
        .key(format!("{}-loading", what.replace(' ', "-")))
        .gap(8.0)
        .center_y()
        .child(Spinner::new().size(14.0).color(p.c.text_muted).build(p.k))
        .child(
            text(format!("Reading {what}\u{2026}"))
                .font(p.k.ty.ui)
                .color(p.c.text_muted),
        )
}

/// A muted line standing in for an empty list.
pub(crate) fn none(key: &str, label: &str, p: &Paint) -> El {
    text(label.to_string())
        .key(key.to_string())
        .font(p.k.ty.ui)
        .color(p.c.text_muted)
}

/// A bordered card holding `body`.
pub(crate) fn card(key: &str, body: El, p: &Paint) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .min_h(0.0)
        .padding(14.0)
        .gap(10.0)
        .radius(p.k.radius.lg)
        .fill(p.c.layer_01)
        .border(p.hair, p.c.border_muted)
        .child(body)
}

/// `rows` in a scrolling column, divided by hairlines.
pub(crate) fn list(key: &str, rows: Vec<El>, p: &Paint) -> El {
    let mut column = col()
        .key(key.to_string())
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0);
    for (i, r) in rows.into_iter().enumerate() {
        if i > 0 {
            column = column.child(el().h(p.hair).w_full().fill(p.c.border_muted));
        }
        column = column.child(r);
    }
    p.k.scroll_bar(column).w_full().grow(1.0).min_h(0.0)
}

/// `rows` divided by hairlines, as tall as they are.
pub(crate) fn stacked(key: &str, rows: Vec<El>, p: &Paint) -> El {
    let mut column = col().key(key.to_string()).w_full();
    for (i, r) in rows.into_iter().enumerate() {
        if i > 0 {
            column = column.child(el().h(p.hair).w_full().fill(p.c.border_muted));
        }
        column = column.child(r);
    }
    column
}

/// `body` in a scroll area filling the rest of the page, so a short card keeps its height.
pub(crate) fn scrolled(key: &str, body: El, p: &Paint) -> El {
    let area = col()
        .key(key.to_string())
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .scroll_edge_fade(8.0)
        .child(body);
    p.k.scroll_bar(area).w_full().grow(1.0).min_h(0.0)
}

/// One choice of several as a wrapping row of buttons, the picked one filled.
pub(crate) fn pills(
    key: &str,
    options: &[(String, String)],
    picked: Option<&str>,
    pick: impl Fn(&mut App, &str) + Clone + 'static,
    p: &Paint,
) -> El {
    let mut row_ = row().key(key.to_string()).gap(6.0).flex_wrap();
    for (value, label) in options {
        let on = picked == Some(value.as_str());
        let (pick, value) = (pick.clone(), value.clone());
        row_ = row_.child(
            Button::new(label.clone())
                .variant(if on {
                    ButtonVariant::Submit
                } else {
                    ButtonVariant::Outline
                })
                .on_press(move |app: &mut App| pick(app, &value))
                .build(p.k)
                .key(format!(
                    "{key}-{}",
                    label.to_ascii_lowercase().replace(' ', "-")
                )),
        );
    }
    row_
}

/// A labelled field in a form.
pub(crate) fn field(label: &str, control: El, p: &Paint) -> El {
    col()
        .w_full()
        .gap(6.0)
        .child(
            text(label.to_string())
                .font(p.k.ty.small_medium)
                .color(p.c.text_muted),
        )
        .child(control)
}

/// What the app knows that completes `typed`, as buttons that fill the field. Nothing when there
/// is nothing to offer.
pub(crate) fn suggestions(
    key: &str,
    typed: &str,
    want: crate::principals::Want,
    fill: fn(&mut App, &str),
    app: &App,
    p: &Paint,
) -> Option<El> {
    let offered = app.known_principals().suggest(typed, want, 6);
    if offered.is_empty() {
        return None;
    }
    let mut chips = row().key(key.to_string()).gap(6.0).flex_wrap();
    for value in offered {
        let chip = format!("{key}-{value}");
        let label = value.clone();
        chips = chips.child(
            Button::new(label)
                .variant(ButtonVariant::Outline)
                .control_size(rune_kit::ControlSize::Small)
                .on_press(move |app: &mut App| fill(app, &value))
                .build(p.k)
                .key(chip),
        );
    }
    Some(chips)
}

/// A list beside what is picked from it, each as tall as the page.
pub(crate) fn split(key: &str, rows: Vec<El>, width: f32, picked: El, p: &Paint) -> El {
    row()
        .key(format!("{key}-body"))
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(16.0)
        .child(
            card(
                &format!("{key}-list-card"),
                list(&format!("{key}-list"), rows, p),
                p,
            )
            .w(width)
            .h_full(),
        )
        .child(
            col()
                .key(format!("{key}-detail"))
                .grow(1.0)
                .min_w(0.0)
                .h_full()
                .min_h(0.0)
                .child(scrolled(&format!("{key}-scroll"), picked, p)),
        )
}

/// A button that does `action` on its second press: the first arms it under `armed`, and it then
/// reads `sure`.
pub(crate) fn confirm(
    key: impl Into<String>,
    armed: &str,
    (label, sure): (&'static str, &'static str),
    enabled: bool,
    action: crate::admin::Action,
    app: &App,
    p: &Paint,
) -> El {
    let on = app.remote.desk.armed.as_deref() == Some(armed);
    let armed = armed.to_string();
    Button::new(if on { sure } else { label })
        .variant(if on {
            ButtonVariant::Danger
        } else {
            ButtonVariant::Ghost
        })
        .disabled(!enabled || app.remote.desk.busy())
        .on_press(move |app: &mut App| {
            if app.remote.desk.armed.as_deref() == Some(armed.as_str()) {
                app.act(action.clone());
            } else {
                app.remote.desk.armed = Some(armed.clone());
            }
        })
        .build(p.k)
        .key(key.into())
}

/// A value in mono that can be selected and copied.
pub(crate) fn mono(key: impl Into<String>, value: impl Into<String>, p: &Paint) -> El {
    text(value.into())
        .key(key.into())
        .font(p.mono(12.0, 480.0))
        .color(p.c.text_base)
        .selectable(true)
        .wrap()
}

/// A heading over a [`mono`] value.
pub(crate) fn fact(label: &str, key: impl Into<String>, value: impl Into<String>, p: &Paint) -> El {
    col()
        .w_full()
        .gap(4.0)
        .child(heading(label, p))
        .child(mono(key, value, p))
}

/// A small muted heading over a group of rows.
pub(crate) fn heading(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.small_medium)
        .color(p.c.text_faint)
}

pub(crate) fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

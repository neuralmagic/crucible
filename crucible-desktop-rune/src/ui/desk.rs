//! What the management pages share: their frame, read states, and the last action's outcome.

use rune::{El, ScrollSpec, col, el, row, text};
use rune_kit::Spinner;

use crate::App;
use crate::admin::Loaded;
use crate::ui::{Paint, TITLE, banner};

/// A management page: its title and a note beside it, then `body`.
pub(crate) fn frame(key: &str, title: &str, note: String, app: &App, p: &Paint) -> El {
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
                .child(text(title.to_string()).font(TITLE).color(p.c.text_base))
                .child(
                    text(note)
                        .font(p.k.ty.ui)
                        .color(p.c.text_muted)
                        .grow(1.0)
                        .truncate(),
                ),
        )
        .children(outcome(app, p))
}

/// The newest action's outcome, or a spinner while one is on its way.
fn outcome(app: &App, p: &Paint) -> Option<El> {
    if app.desk.busy() {
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
    match app.desk.outcome.as_ref()? {
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

/// A small muted heading over a group of rows.
pub(crate) fn heading(label: &str, p: &Paint) -> El {
    text(label.to_string())
        .font(p.k.ty.small_medium)
        .color(p.c.text_faint)
}

pub(crate) fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

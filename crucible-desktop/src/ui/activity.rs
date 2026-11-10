//! The Activity page: the status transitions the controller recorded, newest first, by day.

use crux::dto::Event;
use jiff::tz::TimeZone;
use rune::{El, col, row, text};

use crate::activity::{by_day, playbook_of};
use crate::ui::desk::{card, frame, heading, landed, none, scrolled, stacked};
use crate::ui::{Paint, tag};
use crate::{App, link::Link};

pub(crate) fn page(app: &App, p: &Paint) -> El {
    let note = match &app.remote.admin.events {
        Some(Ok(events)) => format!("newest {}", events.len()),
        _ => String::new(),
    };
    let page = frame("activity", "Activity", note, None, app, p);
    let events = match landed(&app.remote.admin.events, "the activity", p) {
        Err(standin) => return page.child(*standin),
        Ok(events) => events,
    };
    if events.is_empty() {
        return page.child(none("activity-none", "Nothing yet", p));
    }
    let days = by_day(events, &TimeZone::system())
        .into_iter()
        .enumerate()
        .fold(
            col().key("activity-days").w_full().gap(16.0),
            |column, (at, (day, rows))| {
                let lines = rows
                    .into_iter()
                    .enumerate()
                    .map(|(n, (event, time))| line(at, n, event, time, p))
                    .collect();
                column.child(card(
                    &format!("activity-day-{at}"),
                    col()
                        .w_full()
                        .gap(6.0)
                        .child(heading(&day, p))
                        .child(stacked(&format!("activity-rows-{at}"), lines, p)),
                    p,
                ))
            },
        );
    page.child(scrolled("activity-scroll", days, p))
}

fn line(day: usize, n: usize, event: &Event, time: String, p: &Paint) -> El {
    let key = text(event.key.clone())
        .font(p.mono(12.0, 480.0))
        .color(p.c.text_base)
        .w(360.0)
        .truncate();
    let key = match playbook_of(&event.key) {
        Some(playbook) => {
            let link = Link::Launch {
                playbook: playbook.to_string(),
                key: event.key.clone(),
                task: None,
            };
            key.role("link")
                .focusable(true)
                .hover(p.c.hover)
                .on_press(move |app: &mut App| app.follow(&link))
        }
        None => key,
    };
    let moved = if event.from == event.to {
        event.to.clone()
    } else {
        format!("{} \u{2192} {}", event.from, event.to)
    };
    let reason = event
        .reason
        .as_ref()
        .map(|reason| reason.text.clone())
        .unwrap_or_default();
    let mut line = row()
        .key(format!("activity-{day}-{n}"))
        .w_full()
        .py(6.0)
        .gap(12.0)
        .center_y()
        .child(
            text(time)
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_muted)
                .w(44.0),
        )
        .child(key)
        .child(tag(&moved, p))
        .child(
            text(reason.clone())
                .font(p.k.ty.small)
                .color(p.c.text_muted)
                .grow(1.0)
                .min_w(0.0)
                .truncate()
                .help(reason),
        );
    if let Some(actor) = &event.actor {
        line = line.child(
            text(actor.clone())
                .font(p.mono(12.0, 420.0))
                .color(p.c.text_faint),
        );
    }
    line
}

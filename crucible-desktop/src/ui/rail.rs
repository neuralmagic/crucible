//! The list beside a pane's content, and the handle between them that drags its width.

use rune::{Cursor, Drag, El, Phase, Tx, el, row};

use crate::App;
use crate::ui::Paint;
use crate::workspace::{PaneKind, RailDrag, Rails};

const HANDLE: f32 = 10.0;
/// What a drag leaves the content beside the list.
const CONTENT_MIN: f32 = 220.0;
/// A pane's padding on both sides.
const PANE_PADDING: f32 = 20.0;

pub(crate) fn width(kind: PaneKind, app: &App) -> f32 {
    match app.rail_drag {
        Some(drag) if drag.kind == kind => drag.now,
        _ => app.settings.rails.width(kind).min(widest(kind, app)),
    }
}

/// The widest `kind`'s list goes while its group is as wide as it was last laid out.
fn widest(kind: PaneKind, app: &App) -> f32 {
    app.workspace()
        .locate(kind)
        .and_then(|at| app.geometry.groups.get(&at))
        .map_or(f32::MAX, |rect| {
            (rect[2] - PANE_PADDING - HANDLE - CONTENT_MIN).max(Rails::MIN)
        })
}

pub(crate) fn handle(kind: PaneKind, app: &App, p: &Paint) -> El {
    let held = app.rail_drag.is_some_and(|drag| drag.kind == kind);
    let grip = el().key("grip").size(3.0, 32.0).radius(1.5).fill(if held {
        p.c.bg_accent
    } else {
        p.c.border_strong
    });
    row()
        .key(format!("rail-{}", kind.label().to_ascii_lowercase()))
        .role("separator")
        .label("Resize")
        .w(HANDLE)
        .h_full()
        .center()
        .hover_group()
        .cursor(Cursor::ResizeColumn)
        .on_drag(move |app: &mut App, d: &mut Drag, tx: &mut Tx| drag(app, kind, d, tx))
        .on_double_press(move |app: &mut App| {
            let mut rails = app.settings.rails;
            rails.set(kind, None);
            app.settings.set_rails(rails);
        })
        .child(if held {
            grip
        } else {
            grip.group_opacity(0.0, 1.0)
        })
}

fn drag(app: &mut App, kind: PaneKind, d: &mut Drag, tx: &mut Tx) {
    match d.phase {
        Phase::Down => {
            let from = app.settings.rails.width(kind);
            app.rail_drag = Some(RailDrag {
                kind,
                from,
                now: from,
            });
        }
        Phase::Move => {
            let widest = widest(kind, app);
            if let Some(drag) = app.rail_drag.as_mut() {
                tx.instant();
                drag.now = (drag.from + d.dx).clamp(Rails::MIN, widest);
            }
        }
        Phase::Up => {
            let Some(drag) = app.rail_drag.take() else {
                return;
            };
            if !d.cancelled() {
                let mut rails = app.settings.rails;
                rails.set(kind, Some(drag.now));
                app.settings.set_rails(rails);
            }
        }
    }
}

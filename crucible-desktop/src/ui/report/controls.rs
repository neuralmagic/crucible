//! The row of controls a view declares above its body: selects, switches and ranges.

use crucible_view::{Choice, Control, Param};
use rune::{Drag, El, Phase, Tx, col, el, row, text};
use rune_kit::{Select, Switch};

use crate::App;
use crate::ui::Paint;

/// How wide a range's track is drawn.
const TRACK: f32 = 160.0;
/// How wide a select's field is drawn.
const FIELD: f32 = 180.0;

/// The shown report's controls, or `None` when its last view declared none.
pub(crate) fn controls(app: &App, p: &Paint, layer: &mut Vec<El>) -> Option<El> {
    let declared = app.report_shown.controls();
    if declared.is_empty() {
        return None;
    }
    let mut bar = row()
        .key("report-controls")
        .w_full()
        .gap(18.0)
        .flex_wrap()
        .center_y();
    for control in declared {
        let id = control.id();
        let widget = match (control, app.report_shown.value(control)) {
            (Control::Select { options, .. }, Param::Select(value)) => {
                select(id, control.label(), options, &value, app, p, layer)
            }
            (Control::Toggle { .. }, Param::Toggle(on)) => toggle(id, control.label(), on, p),
            (Control::Range { min, max, step, .. }, Param::Range(value)) => {
                range(id, control.label(), [*min, *max], *step, value, p)
            }
            _ => continue,
        };
        bar = bar.child(
            row()
                .key(format!("report-control-{id}"))
                .gap(8.0)
                .center_y()
                .child(
                    text(control.label().to_string())
                        .font(p.k.ty.small_medium)
                        .color(p.c.text_muted),
                )
                .child(widget),
        );
    }
    if app.report_pending() {
        bar = bar.child(
            text("Updating\u{2026}")
                .key("report-pending")
                .font(p.k.ty.small)
                .color(p.c.text_faint),
        );
    }
    Some(bar)
}

fn select(
    id: &str,
    label: &str,
    options: &[Choice],
    value: &str,
    app: &App,
    p: &Paint,
    layer: &mut Vec<El>,
) -> El {
    let labels: Vec<&str> = options.iter().map(|choice| choice.label.as_str()).collect();
    let current = options
        .iter()
        .find(|choice| choice.value == value)
        .map(|choice| choice.label.as_str());
    let (opened, closed, chosen) = (id.to_string(), id.to_string(), id.to_string());
    let choices = options.to_vec();
    Select::new(format!("report-select-{id}"), &labels)
        .label(label.to_string())
        .current(current)
        .width(FIELD)
        .open(app.report_shown.choosing.as_deref() == Some(id))
        .on_toggle(move |app: &mut App| {
            let shown = &mut app.report_shown;
            shown.choosing = match shown.choosing.take() {
                Some(open) if open == opened => None,
                _ => Some(opened.clone()),
            };
        })
        .on_close(move |app: &mut App| {
            if app.report_shown.choosing.as_deref() == Some(closed.as_str()) {
                app.report_shown.choosing = None;
            }
        })
        .on_choose(move |app: &mut App, picked: &str, _: &mut Tx| {
            if let Some(choice) = choices.iter().find(|choice| choice.label == picked) {
                app.set_report_control(&chosen, Param::Select(choice.value.clone()));
            }
        })
        .build(p.k, layer)
}

fn toggle(id: &str, label: &str, on: bool, p: &Paint) -> El {
    let set = id.to_string();
    Switch::new(on)
        .label(label.to_string())
        .hide_label()
        .on_toggle(move |app: &mut App| app.set_report_control(&set, Param::Toggle(!on)))
        .build(p.k)
        .key(format!("report-switch-{id}"))
}

/// A track the reader drags along; the plugin runs again when the drag ends.
fn range(id: &str, label: &str, [min, max]: [f64; 2], step: f64, value: f64, p: &Paint) -> El {
    let span = max - min;
    let frac = ((value - min) / span).clamp(0.0, 1.0) as f32;
    let set = id.to_string();
    let track = el()
        .key(format!("report-range-{id}"))
        .role("slider")
        .label(label.to_string())
        .w(TRACK)
        .h(20.0)
        .mx(6.0)
        .on_drag(move |app: &mut App, drag: &mut Drag, tx: &mut Tx| {
            tx.instant();
            let width = drag.w.max(1.0);
            let along = |x: f32| f64::from((x / width).clamp(0.0, 1.0));
            match drag.phase {
                Phase::Down => {
                    drag.data = drag.x;
                    app.report_shown.slide(&set, min + along(drag.x) * span);
                }
                Phase::Move => {
                    app.report_shown
                        .slide(&set, min + along(drag.data + drag.dx) * span);
                }
                Phase::Up if drag.cancelled() => app.report_shown.cancel_slide(),
                Phase::Up => app.release_report_range(&set),
            }
        })
        .child(
            col()
                .absolute(0.0, 8.0)
                .w(TRACK)
                .h(4.0)
                .radius(2.0)
                .fill(p.c.layer_02)
                .child(
                    el().w(TRACK * frac)
                        .h(4.0)
                        .radius(2.0)
                        .fill(p.c.text_accent),
                ),
        )
        .child(
            el().absolute(TRACK * frac - 6.0, 4.0)
                .size(12.0, 12.0)
                .radius(6.0)
                .fill(p.c.bg_base)
                .border(p.hair.max(1.0), p.c.text_accent),
        );
    row().gap(8.0).center_y().child(track).child(
        text(figure(value, step))
            .key(format!("report-range-value-{id}"))
            .font(p.mono(11.0, 440.0))
            .color(p.c.text_base),
    )
}

/// `value` with as many decimals as `step` has, up to six.
fn figure(value: f64, step: f64) -> String {
    let mut places = 0;
    let mut scaled = step;
    while places < 6 && (scaled - scaled.round()).abs() > 1e-9 * scaled.abs() {
        scaled *= 10.0;
        places += 1;
    }
    format!("{value:.places$}")
}

#[cfg(test)]
mod tests {
    use crate::ui::report::controls::figure;

    #[test]
    fn a_range_s_value_shows_the_decimals_its_step_has() {
        assert_eq!(figure(7.0, 1.0), "7");
        assert_eq!(figure(7.5, 0.5), "7.5");
        assert_eq!(figure(0.3, 0.1), "0.3");
        assert_eq!(figure(0.25, 0.05), "0.25");
        assert_eq!(figure(1e15, 1e14), "1000000000000000");
        assert_eq!(figure(-2.0, 2.0), "-2");
        assert_eq!(figure(1.0, 1e-12), "1.000000");
        assert_eq!(figure(3.0, f64::MIN_POSITIVE), "3.000000");
    }
}

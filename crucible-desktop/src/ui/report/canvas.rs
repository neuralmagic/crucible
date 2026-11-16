//! Draws a view's canvas: its commands, read as the HTML canvas's 2D context reads them, become
//! one vector shape with the text laid over it.

use std::f32::consts::{PI, TAU};

use crucible_view::{Draw, LineCap, Style, TextAlign, Tone};
use rune::{Color, El, Path, Shape, canvas, el, row, text};

use crate::ui::Paint;

/// The widest box a line of canvas text is laid out in.
const TEXT_BOX: f32 = 400.0;

#[derive(Clone)]
struct State {
    fill: Color,
    stroke: Color,
    alpha: f32,
    line_width: f32,
    cap: LineCap,
    font: Font,
    align: TextAlign,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Font {
    size: f32,
    bold: bool,
    mono: bool,
}

impl Font {
    /// What a CSS font shorthand says of size, weight and face; the rest is not read.
    fn parse(font: &str) -> Font {
        let size = font
            .split_whitespace()
            .find_map(|word| word.strip_suffix("px")?.parse::<f32>().ok())
            .filter(|size| size.is_finite())
            .map_or(10.0, |size| size.clamp(6.0, 48.0));
        Font {
            size,
            bold: font.split_whitespace().any(|word| word == "bold"),
            mono: font.contains("monospace"),
        }
    }
}

fn colour(style: Style, p: &Paint) -> Color {
    match style {
        Style::Tone(Tone::Neutral) => p.c.text_base,
        Style::Tone(Tone::Muted) => p.c.text_muted,
        Style::Tone(Tone::Good) => p.c.success.icon,
        Style::Tone(Tone::Warn) => p.c.warning.icon,
        Style::Tone(Tone::Bad) => p.c.danger.icon,
        Style::Tone(Tone::Info) => p.c.info.icon,
        Style::Rgba(_) => rune::color::hex(&style.to_string()).unwrap_or(p.c.text_base),
    }
}

/// The current path: its subpaths, and where the last one started and stands.
#[derive(Default)]
struct Pen {
    subpaths: Vec<Path>,
    start: Option<[f32; 2]>,
    at: Option<[f32; 2]>,
}

impl Pen {
    fn move_to(&mut self, to: [f32; 2]) {
        self.subpaths.push(Path::new().move_to(to));
        self.start = Some(to);
        self.at = Some(to);
    }

    /// Extends the last subpath with `draw`, which ends at `to`; starts one at `to` when there
    /// is none, as the 2D context does.
    fn extend(&mut self, to: [f32; 2], draw: impl FnOnce(Path) -> Path) {
        match (self.at, self.subpaths.pop()) {
            (Some(_), Some(last)) => {
                self.subpaths.push(draw(last));
                self.at = Some(to);
            }
            (_, last) => {
                self.subpaths.extend(last);
                self.move_to(to);
            }
        }
    }

    fn close(&mut self) {
        if let Some(last) = self.subpaths.pop() {
            self.subpaths.push(last.close());
            self.at = self.start;
        }
    }

    fn rect(&mut self, [x, y, w, h]: [f32; 4]) {
        self.move_to([x, y]);
        for corner in [[x + w, y], [x + w, y + h], [x, y + h]] {
            self.extend(corner, |path| path.line_to(corner));
        }
        self.close();
        self.move_to([x, y]);
    }

    fn arc(&mut self, centre: [f32; 2], radius: f32, from: f32, to: f32, counterclockwise: bool) {
        for point in arc_points(centre, radius, from, to, counterclockwise) {
            self.extend(point, |path| path.line_to(point));
        }
    }
}

/// The points of an arc, a line segment at most 7.5 degrees apart.
fn arc_points(
    [cx, cy]: [f32; 2],
    radius: f32,
    from: f32,
    to: f32,
    counterclockwise: bool,
) -> Vec<[f32; 2]> {
    let radius = radius.max(0.0);
    let mut sweep = if counterclockwise {
        from - to
    } else {
        to - from
    };
    if sweep < 0.0 {
        sweep = sweep.rem_euclid(TAU);
    }
    let sweep = sweep.min(TAU);
    let steps = ((sweep / (PI / 24.0)).ceil() as usize).max(1);
    (0..=steps)
        .map(|step| {
            let turned = sweep * step as f32 / steps as f32;
            let angle = if counterclockwise {
                from - turned
            } else {
                from + turned
            };
            [cx + radius * angle.cos(), cy + radius * angle.sin()]
        })
        .collect()
}

fn rune_cap(cap: LineCap) -> rune::LineCap {
    match cap {
        LineCap::Butt => rune::LineCap::Butt,
        LineCap::Round => rune::LineCap::Round,
    }
}

fn label(body: &str, [x, y]: [f32; 2], state: &State, p: &Paint) -> El {
    let Font { size, bold, mono } = state.font;
    let weight = if bold { 620.0 } else { 420.0 };
    let style = if mono {
        p.mono(size, weight)
    } else {
        p.sized(rune::TextStyle::new(size, weight))
    };
    let ink = text(body.to_string())
        .font(style)
        .color(state.fill.alpha(state.fill.a * state.alpha));
    let top = y - size * 0.82;
    let line = row().w(TEXT_BOX).h(size * 1.3);
    match state.align {
        TextAlign::Start | TextAlign::Left => line.absolute(x, top).child(ink),
        TextAlign::Center => line
            .absolute(x - TEXT_BOX / 2.0, top)
            .justify_center()
            .child(ink),
        TextAlign::End | TextAlign::Right => {
            line.absolute(x - TEXT_BOX, top).justify_end().child(ink)
        }
    }
}

pub(crate) fn canvas_view(width: f32, height: f32, commands: &[Draw], p: &Paint) -> El {
    let mut state = State {
        fill: p.c.text_base,
        stroke: p.c.text_base,
        alpha: 1.0,
        line_width: 1.0,
        cap: LineCap::Butt,
        font: Font::parse("10px sans-serif"),
        align: TextAlign::Start,
    };
    let mut saved: Vec<State> = Vec::new();
    let mut pen = Pen::default();
    let mut shape = Shape::new();
    let mut labels: Vec<El> = Vec::new();
    for command in commands {
        let fill = state.fill.alpha(state.fill.a * state.alpha);
        let stroke = state.stroke.alpha(state.stroke.a * state.alpha);
        match command {
            Draw::FillStyle(style) => state.fill = colour(*style, p),
            Draw::StrokeStyle(style) => state.stroke = colour(*style, p),
            Draw::GlobalAlpha(alpha) => state.alpha = alpha.clamp(0.0, 1.0),
            Draw::LineWidth(width) if *width > 0.0 => state.line_width = *width,
            Draw::LineWidth(_) => {}
            Draw::LineCap(cap) => state.cap = *cap,
            Draw::Font(font) => state.font = Font::parse(font),
            Draw::TextAlign(align) => state.align = *align,
            Draw::Save => saved.push(state.clone()),
            Draw::Restore => {
                if let Some(earlier) = saved.pop() {
                    state = earlier;
                }
            }
            Draw::BeginPath => pen = Pen::default(),
            Draw::MoveTo(to) => pen.move_to(*to),
            Draw::LineTo(to) => pen.extend(*to, |path| path.line_to(*to)),
            Draw::QuadraticCurveTo([cx, cy, x, y]) => {
                pen.extend([*x, *y], |path| path.quad_to([*cx, *cy], [*x, *y]));
            }
            Draw::BezierCurveTo([ax, ay, bx, by, x, y]) => {
                pen.extend([*x, *y], |path| {
                    path.cubic_to([*ax, *ay], [*bx, *by], [*x, *y])
                });
            }
            Draw::Arc {
                x,
                y,
                radius,
                start_angle,
                end_angle,
                counterclockwise,
            } => pen.arc(
                [*x, *y],
                *radius,
                *start_angle,
                *end_angle,
                *counterclockwise,
            ),
            Draw::Rect(rect) => pen.rect(*rect),
            Draw::ClosePath => pen.close(),
            Draw::Fill => {
                for path in &pen.subpaths {
                    shape = shape.fill_path(path, fill);
                }
            }
            Draw::Stroke => {
                for path in &pen.subpaths {
                    shape = shape.stroke_path(path, state.line_width, rune_cap(state.cap), stroke);
                }
            }
            Draw::FillRect([x, y, w, h]) => {
                shape = shape.fill(
                    vec![[*x, *y], [x + w, *y], [x + w, y + h], [*x, y + h]],
                    fill,
                );
            }
            Draw::StrokeRect([x, y, w, h]) => {
                let outline = Path::new()
                    .move_to([*x, *y])
                    .line_to([x + w, *y])
                    .line_to([x + w, y + h])
                    .line_to([*x, y + h])
                    .close();
                shape = shape.stroke_path(&outline, state.line_width, rune_cap(state.cap), stroke);
            }
            Draw::FillText { text: body, x, y } => labels.push(label(body, [*x, *y], &state, p)),
        }
    }
    el().key("report-canvas")
        .size(width, height)
        .child(canvas(shape).size(width, height).absolute(0.0, 0.0))
        .children(labels)
}

#[cfg(test)]
mod tests {
    use std::f32::consts::{PI, TAU};

    use crate::ui::report::canvas::{Font, Pen, arc_points};

    fn near(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
    }

    #[test]
    fn a_font_is_read_for_its_size_weight_and_face() {
        assert_eq!(
            Font::parse("bold 14px monospace"),
            Font {
                size: 14.0,
                bold: true,
                mono: true
            }
        );
        assert_eq!(
            Font::parse("italic 11.5px Inter, sans-serif"),
            Font {
                size: 11.5,
                bold: false,
                mono: false
            }
        );
        assert_eq!(Font::parse("900px serif").size, 48.0);
        assert_eq!(Font::parse("1px serif").size, 6.0);
        assert_eq!(Font::parse("large").size, 10.0);
        assert_eq!(Font::parse("NaNpx x").size, 10.0);
    }

    #[test]
    fn an_arc_runs_clockwise_on_a_y_down_canvas_unless_told_otherwise() {
        let quarter = arc_points([10.0, 10.0], 5.0, 0.0, PI / 2.0, false);
        assert!(near(quarter[0], [15.0, 10.0]), "{quarter:?}");
        assert!(
            near(quarter[quarter.len() - 1], [10.0, 15.0]),
            "{quarter:?}"
        );
        assert_eq!(quarter.len(), 13);

        let other_way = arc_points([10.0, 10.0], 5.0, 0.0, PI / 2.0, true);
        assert!(near(other_way[other_way.len() - 1], [10.0, 15.0]));
        assert!(
            near(other_way[other_way.len() / 2], [6.464, 6.464]),
            "{other_way:?}"
        );
        assert_eq!(other_way.len(), 37, "three quarters the long way round");

        let wrapped = arc_points([0.0, 0.0], 1.0, PI, 0.0, false);
        assert!(near(wrapped[wrapped.len() / 2], [0.0, -1.0]), "{wrapped:?}");

        let full = arc_points([0.0, 0.0], 2.0, 0.0, 10.0 * TAU, false);
        assert_eq!(full.len(), 49, "never more than one turn");
        assert!(near(full[48], [2.0, 0.0]));
        assert!(near(
            arc_points([1.0, 1.0], -3.0, 0.0, 1.0, false)[0],
            [1.0, 1.0]
        ));
    }

    #[test]
    fn a_pen_starts_a_subpath_when_a_line_has_none_and_closes_back_to_its_start() {
        let mut pen = Pen::default();
        pen.extend([3.0, 4.0], |path| path.line_to([3.0, 4.0]));
        assert_eq!((pen.subpaths.len(), pen.at), (1, Some([3.0, 4.0])));
        pen.extend([9.0, 4.0], |path| path.line_to([9.0, 4.0]));
        assert_eq!((pen.subpaths.len(), pen.at), (1, Some([9.0, 4.0])));
        pen.close();
        assert_eq!(pen.at, Some([3.0, 4.0]));
        pen.rect([0.0, 0.0, 2.0, 2.0]);
        assert_eq!((pen.subpaths.len(), pen.at), (3, Some([0.0, 0.0])));
        pen.close();
        Pen::default().close();
    }
}

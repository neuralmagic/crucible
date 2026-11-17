//! Draws a view's canvas: its commands, read as the HTML canvas's 2D context reads them, become
//! vector shapes and lines of text painted in command order, shrunk to fit the pane.

use std::f32::consts::TAU;

use crucible_view::{
    ARC_STEPS, CANVAS_REACH, CURVE_STEPS, Draw, LineCap, Style, TextAlign, TextBaseline, Tone,
};
use rune::{Color, El, Shape, canvas, col, el, row, text};

use crate::ui::Paint;

/// The widest box a line of canvas text with no max width is laid out in.
const TEXT_BOX: f32 = 400.0;
/// The furthest from the canvas's origin a transformed point is placed.
const FAR: f32 = 1e9;
/// The widest a line is stroked or a box of text laid out once transformed.
const WIDEST: f32 = 10_000.0;
/// The least and most a line of text is sized once transformed.
const TEXT_SIZE: [f32; 2] = [1.0, 96.0];
/// The least a canvas is shrunk to fit its pane.
const LEAST_FIT: f32 = 0.1;

#[derive(Clone)]
struct State {
    fill: Color,
    stroke: Color,
    alpha: f32,
    line_width: f32,
    cap: LineCap,
    font: Font,
    align: TextAlign,
    baseline: TextBaseline,
    matrix: Matrix,
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

/// `n` within `reach` of zero, and zero for NaN.
fn bound(n: f32, reach: f32) -> f32 {
    if n.is_nan() {
        0.0
    } else {
        n.clamp(-reach, reach)
    }
}

/// An affine transform `[a, b, c, d, e, f]`, taking `[x, y]` to `[a x + c y + e, b x + d y + f]`,
/// each entry within [`CANVAS_REACH`] of zero.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Matrix([f32; 6]);

impl Matrix {
    fn new(entries: [f32; 6]) -> Matrix {
        Matrix(entries.map(|n| bound(n, CANVAS_REACH)))
    }

    /// This transform applied after `inner`, as the 2D context's `transform` composes them.
    fn then(self, inner: [f32; 6]) -> Matrix {
        let [a, b, c, d, e, f] = self.0;
        let [ia, ib, ic, id, ie, if_] = Matrix::new(inner).0;
        Matrix::new([
            a * ia + c * ib,
            b * ia + d * ib,
            a * ic + c * id,
            b * ic + d * id,
            a * ie + c * if_ + e,
            b * ie + d * if_ + f,
        ])
    }

    fn apply(self, [x, y]: [f32; 2]) -> [f32; 2] {
        let [a, b, c, d, e, f] = self.0;
        [a * x + c * y + e, b * x + d * y + f].map(|n| bound(n, FAR))
    }

    /// How much the transform scales a length: the square root of how much it scales an area.
    fn reach(self) -> f32 {
        let [a, b, c, d, ..] = self.0;
        (a * d - b * c).abs().sqrt()
    }
}

/// How much a canvas `width` points wide is shrunk to fit `room`: at most 1, in hundredths.
fn fit(room: f32, width: f32) -> f32 {
    if room.is_nan() || width.is_nan() || room >= width || width <= 0.0 {
        return 1.0;
    }
    ((room / width * 100.0 + 1e-3).floor() / 100.0).clamp(LEAST_FIT, 1.0)
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

/// A stretch of the current path: its points, and whether it joins back to its first.
#[derive(Clone, Debug, Default, PartialEq)]
struct Subpath {
    points: Vec<[f32; 2]>,
    closed: bool,
}

/// The current path, flattened to points as its commands arrive.
#[derive(Default)]
struct Pen {
    subpaths: Vec<Subpath>,
}

impl Pen {
    fn at(&self) -> Option<[f32; 2]> {
        self.subpaths.last()?.points.last().copied()
    }

    fn move_to(&mut self, to: [f32; 2]) {
        self.subpaths.push(Subpath {
            points: vec![to],
            closed: false,
        });
    }

    /// A line to `to`; a move there when the path is empty, as the 2D context does.
    fn line_to(&mut self, to: [f32; 2]) {
        match self.subpaths.last_mut() {
            Some(last) => last.points.push(to),
            None => self.move_to(to),
        }
    }

    /// A curve from where the pen stands, or from `control` when the path is empty, flattened
    /// to at most [`CURVE_STEPS`] lines. `length` is that of its control polygon and `along`
    /// the curve's point at each fraction of the way.
    fn curve(
        &mut self,
        control: [f32; 2],
        length: impl Fn([f32; 2]) -> f32,
        along: impl Fn([f32; 2], f32) -> [f32; 2],
    ) {
        let from = match self.at() {
            Some(from) => from,
            None => {
                self.move_to(control);
                control
            }
        };
        let steps = ((length(from) / 0.1).sqrt() * 0.5)
            .ceil()
            .clamp(1.0, CURVE_STEPS as f32) as usize;
        for step in 1..=steps {
            self.line_to(along(from, step as f32 / steps as f32));
        }
    }

    /// Joins the last stretch back to its start and starts the next one there.
    fn close(&mut self) {
        let Some(last) = self.subpaths.last_mut() else {
            return;
        };
        let Some(start) = last.points.first().copied() else {
            return;
        };
        last.closed = true;
        self.move_to(start);
    }

    /// Takes a path command, its points put through `m`; false for any other.
    fn take(&mut self, command: &Draw, m: Matrix) -> bool {
        match command {
            Draw::BeginPath => self.subpaths.clear(),
            Draw::MoveTo(to) => self.move_to(m.apply(*to)),
            Draw::LineTo(to) => self.line_to(m.apply(*to)),
            Draw::QuadraticCurveTo([cx, cy, x, y]) => {
                let (c, p) = (m.apply([*cx, *cy]), m.apply([*x, *y]));
                self.curve(
                    c,
                    |a| dist(a, c) + dist(c, p),
                    |a, t| {
                        let u = 1.0 - t;
                        [0, 1].map(|i| u * u * a[i] + 2.0 * u * t * c[i] + t * t * p[i])
                    },
                );
            }
            Draw::BezierCurveTo([ax, ay, bx, by, x, y]) => {
                let (c1, c2, p) = (m.apply([*ax, *ay]), m.apply([*bx, *by]), m.apply([*x, *y]));
                self.curve(
                    c1,
                    |a| dist(a, c1) + dist(c1, c2) + dist(c2, p),
                    |a, t| {
                        let u = 1.0 - t;
                        let (b0, b1, b2, b3) =
                            (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                        [0, 1].map(|i| b0 * a[i] + b1 * c1[i] + b2 * c2[i] + b3 * p[i])
                    },
                );
            }
            Draw::Arc {
                x,
                y,
                radius,
                start_angle,
                end_angle,
                counterclockwise,
            } => {
                for point in arc_points(
                    [*x, *y],
                    *radius,
                    *start_angle,
                    *end_angle,
                    *counterclockwise,
                ) {
                    self.line_to(m.apply(point));
                }
            }
            Draw::Rect(rect) => {
                let [first, rest @ ..] = corners(*rect, m);
                self.move_to(first);
                for corner in rest {
                    self.line_to(corner);
                }
                self.close();
            }
            Draw::ClosePath => self.close(),
            _ => return false,
        }
        true
    }
}

/// The corners of the rectangle `[x, y, width, height]` put through `m`, clockwise from `[x, y]`.
fn corners([x, y, w, h]: [f32; 4], m: Matrix) -> [[f32; 2]; 4] {
    [[x, y], [x + w, y], [x + w, y + h], [x, y + h]].map(|corner| m.apply(corner))
}

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt()
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
    let steps = ((sweep / (TAU / ARC_STEPS as f32)).ceil() as usize).clamp(1, ARC_STEPS);
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

/// A line of canvas text placed in the canvas's points.
#[derive(Clone, Debug, PartialEq)]
struct Label {
    body: String,
    /// The top left of the box the line is laid out in.
    at: [f32; 2],
    /// The box's width; the text ends in an ellipsis past it when it has a max width.
    room: f32,
    cut: bool,
    font: Font,
    align: TextAlign,
    ink: Color,
}

impl Label {
    /// `body` at `anchor` as the state draws it, or none where the 2D context draws nothing.
    fn new(body: &str, anchor: [f32; 2], max_width: Option<f32>, state: &State) -> Option<Label> {
        let reach = state.matrix.reach();
        if reach.is_nan() || reach <= 0.0 {
            return None;
        }
        let size = (state.font.size * reach).clamp(TEXT_SIZE[0], TEXT_SIZE[1]);
        let room = match max_width {
            None => None,
            Some(width) if width > 0.0 => Some((width * reach).min(WIDEST)),
            Some(_) => return None,
        };
        let [x, y] = state.matrix.apply(anchor);
        let drop = match state.baseline {
            TextBaseline::Top => 0.78,
            TextBaseline::Middle => 0.36,
            TextBaseline::Alphabetic => 0.0,
            TextBaseline::Bottom => -0.22,
        };
        let room_ = room.unwrap_or(TEXT_BOX);
        let left = match state.align {
            TextAlign::Start | TextAlign::Left => x,
            TextAlign::Center => x - room_ / 2.0,
            TextAlign::End | TextAlign::Right => x - room_,
        };
        Some(Label {
            body: body.to_string(),
            at: [left, y + size * (drop - 0.82)],
            room: room_,
            cut: room.is_some(),
            font: Font { size, ..state.font },
            align: state.align,
            ink: state.fill.alpha(state.fill.a * state.alpha),
        })
    }

    fn el(&self, p: &Paint) -> El {
        let Font { size, bold, mono } = self.font;
        let weight = if bold { 620.0 } else { 420.0 };
        let style = if mono {
            p.mono(size, weight)
        } else {
            p.sized(rune::TextStyle::new(size, weight))
        };
        let mut ink = text(self.body.clone()).font(style).color(self.ink);
        if self.cut {
            ink = ink.truncate();
        }
        let line = row()
            .w(self.room)
            .h(size * 1.3)
            .absolute(self.at[0], self.at[1])
            .child(ink);
        match self.align {
            TextAlign::Start | TextAlign::Left => line,
            TextAlign::Center => line.justify_center(),
            TextAlign::End | TextAlign::Right => line.justify_end(),
        }
    }
}

/// What a canvas paints, bottom first: shapes and lines of text in command order.
#[derive(Clone, Debug, PartialEq)]
enum Piece {
    Shape(Shape),
    Label(Label),
}

/// A canvas's pieces and its hints, `[x, y, width, height]` and a text, in the canvas's points.
#[derive(Debug, Default, PartialEq)]
struct Drawing {
    pieces: Vec<Piece>,
    hints: Vec<([f32; 4], String)>,
}

/// `commands` read as the 2D context reads them on a canvas `size` points wide and high, scaled
/// by `fit`, with `ink` giving a style's colour.
fn drawing(commands: &[Draw], size: [f32; 2], fit: f32, ink: &dyn Fn(Style) -> Color) -> Drawing {
    let base = Matrix::new([fit, 0.0, 0.0, fit, 0.0, 0.0]);
    let neutral = ink(Style::Tone(Tone::Neutral));
    let mut state = State {
        fill: neutral,
        stroke: neutral,
        alpha: 1.0,
        line_width: 1.0,
        cap: LineCap::Butt,
        font: Font::parse("10px sans-serif"),
        align: TextAlign::Start,
        baseline: TextBaseline::Alphabetic,
        matrix: base,
    };
    let mut saved: Vec<State> = Vec::new();
    let mut pen = Pen::default();
    let mut shape = Shape::new();
    let mut drawn = Drawing::default();
    for command in commands {
        let fill = state.fill.alpha(state.fill.a * state.alpha);
        let stroke = state.stroke.alpha(state.stroke.a * state.alpha);
        let line_width = (state.line_width * state.matrix.reach()).min(WIDEST);
        let m = state.matrix;
        match command {
            Draw::FillStyle(style) => state.fill = ink(*style),
            Draw::StrokeStyle(style) => state.stroke = ink(*style),
            Draw::GlobalAlpha(alpha) => state.alpha = alpha.clamp(0.0, 1.0),
            Draw::LineWidth(width) if *width > 0.0 => state.line_width = *width,
            Draw::LineWidth(_) => {}
            Draw::LineCap(cap) => state.cap = *cap,
            Draw::Font(font) => state.font = Font::parse(font),
            Draw::TextAlign(align) => state.align = *align,
            Draw::TextBaseline(baseline) => state.baseline = *baseline,
            Draw::Save => saved.push(state.clone()),
            Draw::Restore => {
                if let Some(earlier) = saved.pop() {
                    state = earlier;
                }
            }
            Draw::Translate([x, y]) => state.matrix = m.then([1.0, 0.0, 0.0, 1.0, *x, *y]),
            Draw::Scale([x, y]) => state.matrix = m.then([*x, 0.0, 0.0, *y, 0.0, 0.0]),
            Draw::Rotate(angle) => {
                let (sin, cos) = angle.sin_cos();
                state.matrix = m.then([cos, sin, -sin, cos, 0.0, 0.0]);
            }
            Draw::Transform(entries) => state.matrix = m.then(*entries),
            Draw::SetTransform(entries) => state.matrix = base.then(*entries),
            Draw::ResetTransform => state.matrix = base,
            Draw::Fill => {
                for sub in &pen.subpaths {
                    shape = shape.fill(sub.points.clone(), fill);
                }
            }
            Draw::Stroke => {
                for sub in &pen.subpaths {
                    shape = shape.stroke_with(
                        sub.points.clone(),
                        sub.closed,
                        line_width,
                        rune_cap(state.cap),
                        stroke,
                    );
                }
            }
            Draw::FillRect(rect) => shape = shape.fill(corners(*rect, m).to_vec(), fill),
            Draw::StrokeRect(rect) => {
                shape = shape.stroke_with(
                    corners(*rect, m).to_vec(),
                    true,
                    line_width,
                    rune_cap(state.cap),
                    stroke,
                );
            }
            Draw::FillText {
                text: body,
                x,
                y,
                max_width,
            } => {
                if let Some(label) = Label::new(body, [*x, *y], *max_width, &state) {
                    if !shape.is_empty() {
                        drawn.pieces.push(Piece::Shape(std::mem::take(&mut shape)));
                    }
                    drawn.pieces.push(Piece::Label(label));
                }
            }
            Draw::Hint {
                x,
                y,
                width,
                height,
                text: body,
            } => {
                if let Some(area) = hinted(corners([*x, *y, *width, *height], m), size, fit) {
                    drawn.hints.push((area, body.clone()));
                }
            }
            path => {
                pen.take(path, m);
            }
        }
    }
    if !shape.is_empty() {
        drawn.pieces.push(Piece::Shape(shape));
    }
    drawn
}

/// The box around `corners` inside a canvas `size` points by `fit`, if any of it is.
fn hinted(corners: [[f32; 2]; 4], [w, h]: [f32; 2], fit: f32) -> Option<[f32; 4]> {
    let low = |i: usize| corners.iter().map(|c| c[i]).fold(f32::INFINITY, f32::min);
    let high = |i: usize| {
        corners
            .iter()
            .map(|c| c[i])
            .fold(f32::NEG_INFINITY, f32::max)
    };
    let (left, top) = (low(0).max(0.0), low(1).max(0.0));
    let (right, bottom) = (high(0).min(w * fit), high(1).min(h * fit));
    (right > left && bottom > top).then_some([left, top, right - left, bottom - top])
}

/// A canvas `width` by `height` points, scaled by `fit`.
fn canvas_view(width: f32, height: f32, commands: &[Draw], fit: f32, p: &Paint) -> El {
    let size = [width * fit, height * fit];
    let drawn = drawing(commands, [width, height], fit, &|style| colour(style, p));
    el().key("report-canvas")
        .size(size[0], size[1])
        .children(drawn.pieces.into_iter().map(|piece| match piece {
            Piece::Shape(shape) => canvas(shape).size(size[0], size[1]).absolute(0.0, 0.0),
            Piece::Label(label) => label.el(p),
        }))
        .children(
            drawn
                .hints
                .into_iter()
                .map(|([x, y, w, h], body)| el().absolute(x, y).size(w, h).help(body)),
        )
}

/// A canvas in a box as wide as its parent, shrunk to that width when it is wider. `key` names
/// the box; no other canvas may share it.
pub(crate) fn fitted(key: String, width: f32, height: f32, commands: &[Draw], p: &Paint) -> El {
    let scale = rune::memo::layout_with(&key, move |rect| fit(rect[2], width)).unwrap_or(1.0);
    col().w_full().min_w(0.0).child(
        col()
            .key(key)
            .w_full()
            .min_w(0.0)
            .child(canvas_view(width, height, commands, scale, p)),
    )
}

#[cfg(test)]
mod tests {
    use std::f32::consts::{PI, TAU};

    use crucible_view::{CANVAS_REACH, Draw, Style, TextBaseline};
    use rune::Color;

    use crate::ui::report::canvas::{
        Drawing, FAR, Font, LEAST_FIT, Label, Matrix, Pen, Piece, Subpath, TEXT_BOX, TEXT_SIZE,
        WIDEST, arc_points, drawing, fit,
    };

    const IDENTITY: Matrix = Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

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

    fn points(pen: &Pen) -> usize {
        pen.subpaths.iter().map(|sub| sub.points.len()).sum()
    }

    fn arc(end_angle: f32) -> Draw {
        Draw::Arc {
            x: 0.0,
            y: 0.0,
            radius: 5.0,
            start_angle: 0.0,
            end_angle,
            counterclockwise: false,
        }
    }

    #[test]
    fn a_line_with_no_path_starts_one_and_a_close_starts_the_next_at_its_start() {
        let mut pen = Pen::default();
        pen.take(&Draw::LineTo([3.0, 4.0]), IDENTITY);
        pen.take(&Draw::LineTo([9.0, 4.0]), IDENTITY);
        pen.take(&Draw::LineTo([9.0, 8.0]), IDENTITY);
        assert_eq!((pen.subpaths.len(), pen.at()), (1, Some([9.0, 8.0])));
        pen.take(&Draw::ClosePath, IDENTITY);
        assert!(pen.subpaths[0].closed);
        assert_eq!(pen.subpaths[0].points.len(), 3);
        assert_eq!((pen.subpaths.len(), pen.at()), (2, Some([3.0, 4.0])));
        pen.take(&Draw::LineTo([0.0, 0.0]), IDENTITY);
        assert_eq!(pen.subpaths[1].points, [[3.0, 4.0], [0.0, 0.0]]);

        pen.take(&Draw::Rect([1.0, 1.0, 2.0, 2.0]), IDENTITY);
        assert_eq!(
            pen.subpaths[2],
            Subpath {
                points: vec![[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0]],
                closed: true,
            }
        );
        assert_eq!(pen.at(), Some([1.0, 1.0]));
        pen.take(&Draw::BeginPath, IDENTITY);
        assert_eq!((points(&pen), pen.at()), (0, None));
        pen.take(&Draw::ClosePath, IDENTITY);
        assert_eq!(points(&pen), 0);
        assert!(!pen.take(&Draw::Fill, IDENTITY));
    }

    #[test]
    fn a_curve_ends_on_its_point_and_starts_at_its_control_when_the_path_is_empty() {
        let mut pen = Pen::default();
        pen.take(&Draw::QuadraticCurveTo([10.0, 0.0, 10.0, 10.0]), IDENTITY);
        assert_eq!(pen.subpaths[0].points[0], [10.0, 0.0]);
        assert!(near(pen.at().unwrap(), [10.0, 10.0]));

        let mut pen = Pen::default();
        pen.take(&Draw::MoveTo([0.0, 0.0]), IDENTITY);
        pen.take(
            &Draw::BezierCurveTo([0.0, 10.0, 10.0, 10.0, 10.0, 0.0]),
            IDENTITY,
        );
        let points = &pen.subpaths[0].points;
        assert!(near(points[points.len() - 1], [10.0, 0.0]));
        let peak = points.iter().map(|point| point[1]).fold(0.0, f32::max);
        assert!((7.3..=7.5).contains(&peak), "{points:?}");
        assert!(points.len() > 6 && points.len() <= 65, "{}", points.len());
    }

    /// Transforms from none to the furthest the catalog lets a plugin compose.
    fn transforms() -> Vec<Matrix> {
        let far = 1_000_000.0;
        let mut composed = IDENTITY;
        for _ in 0..40 {
            composed = composed.then([far, -far, far, far, far, -far]);
        }
        vec![
            IDENTITY,
            Matrix::new([2.0, 0.0, 0.0, 0.5, 10.0, -10.0]),
            Matrix::new([0.0, 1.0, -1.0, 0.0, 0.0, 0.0]),
            Matrix::new([0.0; 6]),
            Matrix::new([far, far, -far, far, far, far]),
            Matrix::new([
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                1.0,
                f32::MAX,
                0.0,
            ]),
            composed,
        ]
    }

    #[test]
    fn no_command_adds_more_points_than_the_catalog_counts_for_it() {
        let far = 1_000_000.0;
        let commands = [
            Draw::LineTo([1.0, 1.0]),
            Draw::MoveTo([0.0, 0.0]),
            Draw::LineTo([5.0, 5.0]),
            Draw::QuadraticCurveTo([far, -far, -far, far]),
            Draw::BezierCurveTo([far, far, -far, -far, far, -far]),
            Draw::QuadraticCurveTo([0.0, 0.0, 0.0, 0.0]),
            arc(TAU),
            arc(1_000.0 * TAU),
            arc(0.01),
            Draw::Rect([0.0, 0.0, far, far]),
            Draw::ClosePath,
            Draw::ClosePath,
            Draw::BeginPath,
            Draw::BezierCurveTo([far, far, -far, -far, far, -far]),
            Draw::BeginPath,
            arc(-0.1),
            Draw::BeginPath,
            Draw::ClosePath,
            Draw::Rect([-far, -far, far, far]),
        ];
        for m in transforms() {
            let mut pen = Pen::default();
            for command in &commands {
                let before = if *command == Draw::BeginPath {
                    0
                } else {
                    points(&pen)
                };
                assert!(pen.take(command, m));
                let added = points(&pen) - before;
                assert!(
                    added <= command.path_points(),
                    "{command:?} under {m:?} added {added} points"
                );
            }
            assert!(
                pen.subpaths
                    .iter()
                    .flat_map(|sub| &sub.points)
                    .flatten()
                    .all(|n| n.is_finite() && n.abs() <= FAR),
                "{m:?}"
            );
        }
        for command in [
            Draw::Translate([1.0, 2.0]),
            Draw::Scale([2.0, 2.0]),
            Draw::Rotate(1.0),
            Draw::Transform([1.0; 6]),
            Draw::SetTransform([1.0; 6]),
            Draw::ResetTransform,
            Draw::Hint {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
                text: "a".into(),
            },
        ] {
            let mut pen = Pen::default();
            assert!(!pen.take(&command, IDENTITY), "{command:?}");
            assert_eq!(command.path_points(), 0);
        }
    }

    #[test]
    fn transforms_compose_as_the_2d_context_s_do_and_stay_bounded() {
        let moved = IDENTITY.then([1.0, 0.0, 0.0, 1.0, 10.0, 0.0]);
        let (sin, cos) = (PI / 2.0).sin_cos();
        let turned = moved.then([cos, sin, -sin, cos, 0.0, 0.0]);
        assert!(
            near(turned.apply([1.0, 0.0]), [10.0, 1.0]),
            "translate, then rotate"
        );
        let scaled = turned.then([2.0, 0.0, 0.0, 3.0, 0.0, 0.0]);
        assert!(near(scaled.apply([1.0, 1.0]), [7.0, 2.0]), "{scaled:?}");
        assert!((scaled.reach() - 6.0_f32.sqrt()).abs() < 1e-5);
        assert_eq!(Matrix::new([0.0; 6]).reach(), 0.0);
        assert_eq!(Matrix::new([0.0; 6]).apply([5.0, 5.0]), [0.0, 0.0]);
        for m in transforms() {
            assert!(m.0.iter().all(|n| n.abs() <= CANVAS_REACH), "{m:?}");
            assert!(m.reach().is_finite(), "{m:?}");
            for point in [[0.0, 0.0], [1e6, -1e6], [-1e6, -1e6]] {
                assert!(
                    m.apply(point)
                        .iter()
                        .all(|n| n.is_finite() && n.abs() <= FAR),
                    "{m:?}"
                );
            }
        }
    }

    #[test]
    fn a_canvas_shrinks_to_its_room_in_hundredths_and_no_further_than_a_tenth() {
        assert_eq!(fit(600.0, 600.0), 1.0);
        assert_eq!(fit(2_000.0, 600.0), 1.0);
        assert_eq!(fit(300.0, 600.0), 0.5);
        assert_eq!(fit(599.0, 600.0), 0.99);
        assert_eq!(fit(1.0, 600.0), LEAST_FIT);
        assert_eq!(fit(0.0, 600.0), LEAST_FIT);
        assert_eq!(fit(-5.0, 600.0), LEAST_FIT);
        assert_eq!(fit(f32::NAN, 600.0), 1.0);
        assert_eq!(fit(f32::INFINITY, 600.0), 1.0);
        assert_eq!(fit(300.0, 0.0), 1.0);
        assert_eq!(fit(300.0, f32::NAN), 1.0);
        for width in [600.0, 624.0, 4_000.0, 333.0] {
            for hundredths in 10..100 {
                let scale = hundredths as f32 / 100.0;
                assert_eq!(
                    fit(width * scale, width),
                    scale,
                    "a canvas drawn at its scale keeps it on {width}"
                );
            }
        }
    }

    fn white(_: Style) -> Color {
        Color::WHITE
    }

    fn label(drawn: &Drawing, at: usize) -> &Label {
        match &drawn.pieces[at] {
            Piece::Label(label) => label,
            Piece::Shape(_) => panic!("piece {at} is a shape: {drawn:?}"),
        }
    }

    fn fill_text(text: &str, x: f32, y: f32, max_width: Option<f32>) -> Draw {
        Draw::FillText {
            text: text.into(),
            x,
            y,
            max_width,
        }
    }

    #[test]
    fn text_and_shapes_are_painted_in_command_order() {
        let rect = Draw::FillRect([0.0, 0.0, 10.0, 10.0]);
        let kinds = |commands: &[Draw]| {
            drawing(commands, [100.0, 100.0], 1.0, &white)
                .pieces
                .iter()
                .map(|piece| match piece {
                    Piece::Shape(_) => 's',
                    Piece::Label(_) => 't',
                })
                .collect::<String>()
        };
        assert_eq!(kinds(&[]), "");
        assert_eq!(kinds(&[rect.clone(), rect.clone()]), "s");
        assert_eq!(
            kinds(&[
                rect.clone(),
                fill_text("a", 0.0, 0.0, None),
                rect.clone(),
                rect.clone(),
                fill_text("b", 0.0, 0.0, None),
                fill_text("c", 0.0, 0.0, None),
                rect.clone(),
            ]),
            "ststts"
        );
        assert_eq!(
            kinds(&[fill_text("a", 0.0, 0.0, None), rect.clone(), rect]),
            "ts"
        );
        let many: Vec<Draw> = (0..2_000)
            .map(|i| {
                if i % 2 == 0 {
                    Draw::FillRect([0.0, 0.0, 1.0, 1.0])
                } else {
                    fill_text("x", 0.0, 0.0, None)
                }
            })
            .collect();
        assert_eq!(
            drawing(&many, [10.0, 10.0], 1.0, &white).pieces.len(),
            2_000,
            "a piece per command at most"
        );
    }

    #[test]
    fn text_is_placed_upright_at_its_transformed_anchor_and_sized_with_the_transform() {
        let drawn = drawing(
            &[
                Draw::Font("10px sans-serif".into()),
                fill_text("plain", 20.0, 30.0, None),
                Draw::Save,
                Draw::Translate([100.0, 0.0]),
                Draw::Scale([2.0, 2.0]),
                Draw::Rotate(1.0),
                fill_text("moved", 0.0, 0.0, None),
                Draw::Restore,
                fill_text("back", 20.0, 30.0, None),
                Draw::Scale([0.0, 1.0]),
                fill_text("flat", 0.0, 0.0, None),
            ],
            [400.0, 100.0],
            1.0,
            &white,
        );
        assert_eq!(
            drawn.pieces.len(),
            3,
            "nothing drawn under a flat transform"
        );
        let plain = label(&drawn, 0);
        assert_eq!(plain.font.size, 10.0);
        assert!(near(plain.at, [20.0, 30.0 - 8.2]), "{plain:?}");
        assert_eq!((plain.room, plain.cut), (TEXT_BOX, false));
        let moved = label(&drawn, 1);
        assert!((moved.font.size - 20.0).abs() < 1e-4, "{moved:?}");
        assert!(near(moved.at, [100.0, -16.4]), "{moved:?}");
        assert_eq!(
            label(&drawn, 2),
            &Label {
                body: "back".into(),
                ..plain.clone()
            },
            "restore brings the transform back"
        );
    }

    #[test]
    fn a_fitted_canvas_scales_every_place_size_and_width() {
        let commands = [
            Draw::Font("20px sans-serif".into()),
            Draw::TextAlign(crucible_view::TextAlign::Center),
            fill_text("a", 200.0, 100.0, Some(100.0)),
            Draw::SetTransform([1.0, 0.0, 0.0, 1.0, 40.0, 0.0]),
            fill_text("b", 0.0, 100.0, None),
            Draw::ResetTransform,
            fill_text("c", 0.0, 100.0, None),
            Draw::Hint {
                x: 100.0,
                y: 50.0,
                width: 100.0,
                height: 20.0,
                text: "h".into(),
            },
        ];
        let whole = drawing(&commands, [400.0, 200.0], 1.0, &white);
        let half = drawing(&commands, [400.0, 200.0], 0.5, &white);
        let (a, small) = (label(&whole, 0), label(&half, 0));
        assert_eq!((a.room, a.cut, a.font.size), (100.0, true, 20.0));
        assert_eq!((small.room, small.cut, small.font.size), (50.0, true, 10.0));
        assert!(near(small.at, a.at.map(|n| n / 2.0)), "{small:?} {a:?}");
        assert!(near(
            label(&half, 1).at,
            [20.0 - TEXT_BOX / 2.0, 50.0 - 8.2]
        ));
        assert!(near(label(&half, 2).at, [-TEXT_BOX / 2.0, 50.0 - 8.2]));
        assert_eq!(whole.hints, [([100.0, 50.0, 100.0, 20.0], "h".to_string())]);
        assert_eq!(half.hints, [([50.0, 25.0, 50.0, 10.0], "h".to_string())]);
    }

    #[test]
    fn a_max_width_cuts_text_and_one_not_above_zero_draws_none() {
        for (max_width, drawn) in [
            (Some(80.0), true),
            (Some(1e6), true),
            (Some(0.0), false),
            (Some(-1.0), false),
            (Some(f32::NAN), false),
            (None, true),
        ] {
            let pieces = drawing(
                &[fill_text("long text", 0.0, 10.0, max_width)],
                [100.0, 100.0],
                1.0,
                &white,
            )
            .pieces;
            assert_eq!(pieces.len(), usize::from(drawn), "{max_width:?}");
            if let Some(Piece::Label(label)) = pieces.first() {
                assert!(label.room <= WIDEST && label.room > 0.0, "{label:?}");
            }
        }
        let huge = drawing(
            &[
                Draw::Scale([1e6, 1e6]),
                Draw::Font("48px sans-serif".into()),
                fill_text("x", 1e6, 1e6, Some(1e6)),
            ],
            [100.0, 100.0],
            1.0,
            &white,
        );
        let x = label(&huge, 0);
        assert_eq!((x.room, x.font.size), (WIDEST, TEXT_SIZE[1]));
        assert!(x.at.iter().all(|n| n.is_finite()), "{x:?}");
    }

    #[test]
    fn a_baseline_moves_the_line_by_its_size() {
        let top = |baseline: TextBaseline| {
            let drawn = drawing(
                &[
                    Draw::Font("20px sans-serif".into()),
                    Draw::TextBaseline(baseline),
                    fill_text("a", 0.0, 100.0, None),
                ],
                [100.0, 200.0],
                1.0,
                &white,
            );
            label(&drawn, 0).at[1]
        };
        let [t, m, a, b] = [
            TextBaseline::Top,
            TextBaseline::Middle,
            TextBaseline::Alphabetic,
            TextBaseline::Bottom,
        ]
        .map(top);
        assert!(near([a, 0.0], [100.0 - 16.4, 0.0]), "{a}");
        assert!(t > m && m > a && a > b, "{t} {m} {a} {b}");
        assert!((t - a - 15.6).abs() < 1e-3, "{t}");
    }

    #[test]
    fn a_hint_covers_its_transformed_rectangle_inside_the_canvas() {
        let hints = |commands: &[Draw]| drawing(commands, [100.0, 50.0], 1.0, &white).hints;
        let hint = |x: f32, y: f32, width: f32, height: f32| Draw::Hint {
            x,
            y,
            width,
            height,
            text: "h".into(),
        };
        assert_eq!(
            hints(&[hint(10.0, 10.0, 20.0, 5.0)])[0].0,
            [10.0, 10.0, 20.0, 5.0]
        );
        assert_eq!(
            hints(&[hint(30.0, 15.0, -20.0, -5.0)])[0].0,
            [10.0, 10.0, 20.0, 5.0],
            "a negative size runs back from x and y"
        );
        let turned = hints(&[
            Draw::Translate([50.0, 25.0]),
            Draw::Rotate(PI / 2.0),
            hint(0.0, 0.0, 10.0, 4.0),
        ]);
        let [x, y, w, h] = turned[0].0;
        assert!(
            near([x, y], [46.0, 25.0]) && near([w, h], [4.0, 10.0]),
            "{turned:?}"
        );
        assert_eq!(
            hints(&[hint(-50.0, -50.0, 1e6, 1e6)])[0].0,
            [0.0, 0.0, 100.0, 50.0]
        );
        for off in [
            hint(200.0, 0.0, 10.0, 10.0),
            hint(-20.0, 0.0, 10.0, 10.0),
            hint(10.0, 10.0, 0.0, 10.0),
            hint(10.0, 10.0, 10.0, 0.0),
        ] {
            assert!(hints(std::slice::from_ref(&off)).is_empty(), "{off:?}");
        }
        assert!(hints(&[Draw::Scale([0.0, 0.0]), hint(1.0, 1.0, 5.0, 5.0)]).is_empty());
        assert_eq!(hints(&vec![hint(0.0, 0.0, 5.0, 5.0); 3]).len(), 3);
    }
}

//! Draws a view's canvas: its commands, read as the HTML canvas's 2D context reads them, become
//! one vector shape with the text laid over it.

use std::f32::consts::TAU;

use crucible_view::{ARC_STEPS, CURVE_STEPS, Draw, LineCap, Style, TextAlign, Tone};
use rune::{Color, El, Shape, canvas, el, row, text};

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

    /// Takes a path command; false for any other.
    fn take(&mut self, command: &Draw) -> bool {
        match command {
            Draw::BeginPath => self.subpaths.clear(),
            Draw::MoveTo(to) => self.move_to(*to),
            Draw::LineTo(to) => self.line_to(*to),
            Draw::QuadraticCurveTo([cx, cy, x, y]) => {
                let (c, p) = ([*cx, *cy], [*x, *y]);
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
                let (c1, c2, p) = ([*ax, *ay], [*bx, *by], [*x, *y]);
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
                    self.line_to(point);
                }
            }
            Draw::Rect([x, y, w, h]) => {
                self.move_to([*x, *y]);
                for corner in [[x + w, *y], [x + w, y + h], [*x, y + h]] {
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
                        state.line_width,
                        rune_cap(state.cap),
                        stroke,
                    );
                }
            }
            Draw::FillRect([x, y, w, h]) => {
                shape = shape.fill(
                    vec![[*x, *y], [x + w, *y], [x + w, y + h], [*x, y + h]],
                    fill,
                );
            }
            Draw::StrokeRect([x, y, w, h]) => {
                shape = shape.stroke_with(
                    vec![[*x, *y], [x + w, *y], [x + w, y + h], [*x, y + h]],
                    true,
                    state.line_width,
                    rune_cap(state.cap),
                    stroke,
                );
            }
            Draw::FillText { text: body, x, y } => labels.push(label(body, [*x, *y], &state, p)),
            path => {
                pen.take(path);
            }
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

    use crucible_view::Draw;

    use crate::ui::report::canvas::{Font, Pen, Subpath, arc_points};

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
        pen.take(&Draw::LineTo([3.0, 4.0]));
        pen.take(&Draw::LineTo([9.0, 4.0]));
        pen.take(&Draw::LineTo([9.0, 8.0]));
        assert_eq!((pen.subpaths.len(), pen.at()), (1, Some([9.0, 8.0])));
        pen.take(&Draw::ClosePath);
        assert!(pen.subpaths[0].closed);
        assert_eq!(pen.subpaths[0].points.len(), 3);
        assert_eq!((pen.subpaths.len(), pen.at()), (2, Some([3.0, 4.0])));
        pen.take(&Draw::LineTo([0.0, 0.0]));
        assert_eq!(pen.subpaths[1].points, [[3.0, 4.0], [0.0, 0.0]]);

        pen.take(&Draw::Rect([1.0, 1.0, 2.0, 2.0]));
        assert_eq!(
            pen.subpaths[2],
            Subpath {
                points: vec![[1.0, 1.0], [3.0, 1.0], [3.0, 3.0], [1.0, 3.0]],
                closed: true,
            }
        );
        assert_eq!(pen.at(), Some([1.0, 1.0]));
        pen.take(&Draw::BeginPath);
        assert_eq!((points(&pen), pen.at()), (0, None));
        pen.take(&Draw::ClosePath);
        assert_eq!(points(&pen), 0);
        assert!(!pen.take(&Draw::Fill));
    }

    #[test]
    fn a_curve_ends_on_its_point_and_starts_at_its_control_when_the_path_is_empty() {
        let mut pen = Pen::default();
        pen.take(&Draw::QuadraticCurveTo([10.0, 0.0, 10.0, 10.0]));
        assert_eq!(pen.subpaths[0].points[0], [10.0, 0.0]);
        assert!(near(pen.at().unwrap(), [10.0, 10.0]));

        let mut pen = Pen::default();
        pen.take(&Draw::MoveTo([0.0, 0.0]));
        pen.take(&Draw::BezierCurveTo([0.0, 10.0, 10.0, 10.0, 10.0, 0.0]));
        let points = &pen.subpaths[0].points;
        assert!(near(points[points.len() - 1], [10.0, 0.0]));
        let peak = points.iter().map(|point| point[1]).fold(0.0, f32::max);
        assert!((7.3..=7.5).contains(&peak), "{points:?}");
        assert!(points.len() > 6 && points.len() <= 65, "{}", points.len());
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
        ];
        let mut pen = Pen::default();
        for command in &commands {
            let before = if *command == Draw::BeginPath {
                0
            } else {
                points(&pen)
            };
            assert!(pen.take(command));
            let added = points(&pen) - before;
            assert!(
                added <= command.path_points(),
                "{command:?} added {added} points"
            );
        }
        assert!(
            pen.subpaths
                .iter()
                .flat_map(|sub| &sub.points)
                .flatten()
                .all(|n| n.is_finite())
        );
    }
}

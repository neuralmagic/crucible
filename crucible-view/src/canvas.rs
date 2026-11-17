//! Free drawing for a view, with the commands of the HTML canvas's 2D context under the same
//! names in snake case. Coordinates are points from the top left, y down, angles in radians.
//!
//! ```
//! use crucible_view::{Context2d, Tone};
//!
//! let mut ctx = Context2d::new(200.0, 80.0);
//! ctx.set_fill_style(Tone::Good);
//! ctx.fill_rect(10.0, 10.0, 60.0, 60.0);
//! ctx.set_stroke_style("#3b82f6".parse::<crucible_view::Style>().unwrap());
//! ctx.begin_path();
//! ctx.arc(140.0, 40.0, 30.0, 0.0, std::f32::consts::TAU);
//! ctx.stroke();
//! let node = ctx.finish();
//! ```
//!
//! What differs from a browser's canvas: styles are a theme tone or a hex colour, with no
//! gradients or patterns; there are no clips or images; text is drawn upright at its
//! transformed anchor; and a font is read only for its `px` size and the words `bold` and
//! `monospace`. [`Draw::Hint`] and [`Draw::Hit`] are not the 2D context's: a text shown on hover
//! over a rectangle, and a rectangle the reader clicks.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{Node, Tone};

/// A fill or stroke colour: one of the host theme's tones, or `#rgb`, `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Style {
    Tone(Tone),
    /// Red, green, blue and alpha.
    Rgba([u8; 4]),
}

impl From<Tone> for Style {
    fn from(tone: Tone) -> Style {
        Style::Tone(tone)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a tone or a hex colour")]
pub struct NotAStyle(String);

impl FromStr for Style {
    type Err = NotAStyle;

    fn from_str(text: &str) -> Result<Style, NotAStyle> {
        let refuse = || NotAStyle(text.to_string());
        let Some(hex) = text.strip_prefix('#') else {
            return serde_json::from_value(serde_json::Value::String(text.to_string()))
                .map(Style::Tone)
                .map_err(|_| refuse());
        };
        if !hex.is_ascii() {
            return Err(refuse());
        }
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<_>>()
            .ok_or_else(refuse)?;
        match digits.as_slice() {
            [r, g, b] => Ok(Style::Rgba([r * 17, g * 17, b * 17, 255])),
            [r1, r0, g1, g0, b1, b0] => {
                Ok(Style::Rgba([r1 * 16 + r0, g1 * 16 + g0, b1 * 16 + b0, 255]))
            }
            [r1, r0, g1, g0, b1, b0, a1, a0] => Ok(Style::Rgba([
                r1 * 16 + r0,
                g1 * 16 + g0,
                b1 * 16 + b0,
                a1 * 16 + a0,
            ])),
            _ => Err(refuse()),
        }
    }
}

impl TryFrom<String> for Style {
    type Error = NotAStyle;

    fn try_from(text: String) -> Result<Style, NotAStyle> {
        text.parse()
    }
}

impl fmt::Display for Style {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Style::Tone(tone) => match serde_json::to_value(tone) {
                Ok(serde_json::Value::String(name)) => f.write_str(&name),
                _ => Err(fmt::Error),
            },
            Style::Rgba([r, g, b, a]) => write!(f, "#{r:02x}{g:02x}{b:02x}{a:02x}"),
        }
    }
}

impl From<Style> for String {
    fn from(style: Style) -> String {
        style.to_string()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineCap {
    #[default]
    Butt,
    Round,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextAlign {
    #[default]
    Start,
    Left,
    Center,
    Right,
    End,
}

/// Where `y` falls on a line of text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextBaseline {
    Top,
    Middle,
    #[default]
    Alphabetic,
    Bottom,
}

/// One canvas command. A setter holds the new value; a path or drawing command its arguments in
/// the order the 2D context takes them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Draw {
    FillStyle(Style),
    StrokeStyle(Style),
    GlobalAlpha(f32),
    LineWidth(f32),
    LineCap(LineCap),
    Font(String),
    TextAlign(TextAlign),
    TextBaseline(TextBaseline),
    Save,
    Restore,
    Translate([f32; 2]),
    Scale([f32; 2]),
    Rotate(f32),
    /// Multiplies the current transform by `[a, b, c, d, e, f]`.
    Transform([f32; 6]),
    SetTransform([f32; 6]),
    ResetTransform,
    BeginPath,
    MoveTo([f32; 2]),
    LineTo([f32; 2]),
    QuadraticCurveTo([f32; 4]),
    BezierCurveTo([f32; 6]),
    Arc {
        x: f32,
        y: f32,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        #[serde(default)]
        counterclockwise: bool,
    },
    Rect([f32; 4]),
    ClosePath,
    Fill,
    Stroke,
    FillRect([f32; 4]),
    StrokeRect([f32; 4]),
    FillText {
        text: String,
        x: f32,
        y: f32,
        /// The widest the text is drawn; longer text ends in an ellipsis.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_width: Option<f32>,
    },
    /// A text shown on hover over a rectangle.
    Hint {
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        text: String,
    },
    /// A rectangle the reader clicks, under the current transform, which runs the plugin again
    /// with [`crate::Event::Hit`] and this id.
    Hit {
        id: String,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
    },
}

/// The furthest from zero a canvas number may be.
pub const CANVAS_REACH: f32 = 1_000_000.0;
/// The most line segments a curve is drawn as.
pub const CURVE_STEPS: usize = 64;
/// The most line segments an arc is drawn as, one each 7.5 degrees of a full turn.
pub const ARC_STEPS: usize = 48;

impl Draw {
    /// Whether every number the command holds is within [`CANVAS_REACH`] of zero.
    pub(crate) fn in_reach(&self) -> bool {
        let near = |n: &f32| n.abs() <= CANVAS_REACH;
        match self {
            Draw::GlobalAlpha(n) | Draw::LineWidth(n) | Draw::Rotate(n) => near(n),
            Draw::MoveTo(n) | Draw::LineTo(n) | Draw::Translate(n) | Draw::Scale(n) => {
                n.iter().all(near)
            }
            Draw::QuadraticCurveTo(n) | Draw::Rect(n) | Draw::FillRect(n) | Draw::StrokeRect(n) => {
                n.iter().all(near)
            }
            Draw::BezierCurveTo(n) | Draw::Transform(n) | Draw::SetTransform(n) => {
                n.iter().all(near)
            }
            Draw::Arc {
                x,
                y,
                radius,
                start_angle,
                end_angle,
                ..
            } => [x, y, radius, start_angle, end_angle].into_iter().all(near),
            Draw::FillText {
                x, y, max_width, ..
            } => near(x) && near(y) && max_width.as_ref().is_none_or(near),
            Draw::Hint {
                x,
                y,
                width,
                height,
                ..
            }
            | Draw::Hit {
                x,
                y,
                width,
                height,
                ..
            } => [x, y, width, height].into_iter().all(near),
            Draw::FillStyle(_)
            | Draw::StrokeStyle(_)
            | Draw::LineCap(_)
            | Draw::Font(_)
            | Draw::TextAlign(_)
            | Draw::TextBaseline(_)
            | Draw::Save
            | Draw::Restore
            | Draw::ResetTransform
            | Draw::BeginPath
            | Draw::ClosePath
            | Draw::Fill
            | Draw::Stroke => true,
        }
    }

    /// The most points the command adds to the current path.
    pub fn path_points(&self) -> usize {
        match self {
            Draw::MoveTo(_) | Draw::LineTo(_) | Draw::ClosePath => 1,
            Draw::QuadraticCurveTo(_) | Draw::BezierCurveTo(_) => CURVE_STEPS + 1,
            Draw::Arc { .. } => ARC_STEPS + 1,
            Draw::Rect(_) => 5,
            _ => 0,
        }
    }
}

/// Records canvas commands under the 2D context's method names, as `web-sys` spells them.
#[derive(Clone, Debug, PartialEq)]
pub struct Context2d {
    width: f32,
    height: f32,
    commands: Vec<Draw>,
}

impl Context2d {
    pub fn new(width: f32, height: f32) -> Context2d {
        Context2d {
            width,
            height,
            commands: Vec::new(),
        }
    }

    /// The canvas as a view component.
    pub fn finish(self) -> Node {
        Node::Canvas {
            width: self.width,
            height: self.height,
            commands: self.commands,
        }
    }

    pub fn set_fill_style(&mut self, style: impl Into<Style>) {
        self.commands.push(Draw::FillStyle(style.into()));
    }

    pub fn set_stroke_style(&mut self, style: impl Into<Style>) {
        self.commands.push(Draw::StrokeStyle(style.into()));
    }

    pub fn set_global_alpha(&mut self, alpha: f32) {
        self.commands.push(Draw::GlobalAlpha(alpha));
    }

    pub fn set_line_width(&mut self, width: f32) {
        self.commands.push(Draw::LineWidth(width));
    }

    pub fn set_line_cap(&mut self, cap: LineCap) {
        self.commands.push(Draw::LineCap(cap));
    }

    pub fn set_font(&mut self, font: &str) {
        self.commands.push(Draw::Font(font.to_string()));
    }

    pub fn set_text_align(&mut self, align: TextAlign) {
        self.commands.push(Draw::TextAlign(align));
    }

    pub fn set_text_baseline(&mut self, baseline: TextBaseline) {
        self.commands.push(Draw::TextBaseline(baseline));
    }

    pub fn save(&mut self) {
        self.commands.push(Draw::Save);
    }

    pub fn restore(&mut self) {
        self.commands.push(Draw::Restore);
    }

    pub fn translate(&mut self, x: f32, y: f32) {
        self.commands.push(Draw::Translate([x, y]));
    }

    pub fn scale(&mut self, x: f32, y: f32) {
        self.commands.push(Draw::Scale([x, y]));
    }

    /// Clockwise by `angle` radians.
    pub fn rotate(&mut self, angle: f32) {
        self.commands.push(Draw::Rotate(angle));
    }

    pub fn transform(&mut self, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) {
        self.commands.push(Draw::Transform([a, b, c, d, e, f]));
    }

    pub fn set_transform(&mut self, a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) {
        self.commands.push(Draw::SetTransform([a, b, c, d, e, f]));
    }

    pub fn reset_transform(&mut self) {
        self.commands.push(Draw::ResetTransform);
    }

    pub fn begin_path(&mut self) {
        self.commands.push(Draw::BeginPath);
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.commands.push(Draw::MoveTo([x, y]));
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        self.commands.push(Draw::LineTo([x, y]));
    }

    pub fn quadratic_curve_to(&mut self, cpx: f32, cpy: f32, x: f32, y: f32) {
        self.commands.push(Draw::QuadraticCurveTo([cpx, cpy, x, y]));
    }

    pub fn bezier_curve_to(&mut self, cp1x: f32, cp1y: f32, cp2x: f32, cp2y: f32, x: f32, y: f32) {
        self.commands
            .push(Draw::BezierCurveTo([cp1x, cp1y, cp2x, cp2y, x, y]));
    }

    /// Clockwise from `start_angle` to `end_angle`.
    pub fn arc(&mut self, x: f32, y: f32, radius: f32, start_angle: f32, end_angle: f32) {
        self.arc_with_anticlockwise(x, y, radius, start_angle, end_angle, false);
    }

    /// From `start_angle` to `end_angle`, anticlockwise when `anticlockwise`.
    pub fn arc_with_anticlockwise(
        &mut self,
        x: f32,
        y: f32,
        radius: f32,
        start_angle: f32,
        end_angle: f32,
        anticlockwise: bool,
    ) {
        self.commands.push(Draw::Arc {
            x,
            y,
            radius,
            start_angle,
            end_angle,
            counterclockwise: anticlockwise,
        });
    }

    pub fn rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.commands.push(Draw::Rect([x, y, width, height]));
    }

    pub fn close_path(&mut self) {
        self.commands.push(Draw::ClosePath);
    }

    pub fn fill(&mut self) {
        self.commands.push(Draw::Fill);
    }

    pub fn stroke(&mut self) {
        self.commands.push(Draw::Stroke);
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.commands.push(Draw::FillRect([x, y, width, height]));
    }

    pub fn stroke_rect(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.commands.push(Draw::StrokeRect([x, y, width, height]));
    }

    pub fn fill_text(&mut self, text: &str, x: f32, y: f32) {
        self.commands.push(Draw::FillText {
            text: text.to_string(),
            x,
            y,
            max_width: None,
        });
    }

    pub fn fill_text_with_max_width(&mut self, text: &str, x: f32, y: f32, max_width: f32) {
        self.commands.push(Draw::FillText {
            text: text.to_string(),
            x,
            y,
            max_width: Some(max_width),
        });
    }

    /// Shows `text` on hover over the rectangle, under the current transform.
    pub fn hint(&mut self, x: f32, y: f32, width: f32, height: f32, text: &str) {
        self.commands.push(Draw::Hint {
            x,
            y,
            width,
            height,
            text: text.to_string(),
        });
    }

    /// Marks the rectangle, under the current transform, as one the reader clicks; see
    /// [`Draw::Hit`].
    pub fn hit(&mut self, x: f32, y: f32, width: f32, height: f32, id: &str) {
        self.commands.push(Draw::Hit {
            id: id.to_string(),
            x,
            y,
            width,
            height,
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Context2d, Draw, Limits, Node, Problem, Style, TextAlign, TextBaseline, Tone, View,
        ViewKind,
    };

    fn view(body: Node) -> View {
        View {
            title: "T".into(),
            body,
            controls: vec![],
            live: Default::default(),
        }
    }

    #[test]
    fn a_style_is_a_tone_or_a_hex_colour() {
        assert_eq!("bad".parse(), Ok(Style::Tone(Tone::Bad)));
        assert_eq!("#fff".parse(), Ok(Style::Rgba([255, 255, 255, 255])));
        assert_eq!("#3b82f6".parse(), Ok(Style::Rgba([0x3b, 0x82, 0xf6, 255])));
        assert_eq!(
            "#3B82F680".parse(),
            Ok(Style::Rgba([0x3b, 0x82, 0xf6, 0x80]))
        );
        for off in ["red", "#12", "#12345", "#gggggg", "rgb(1,2,3)", "", "#é12"] {
            assert!(off.parse::<Style>().is_err(), "{off}");
        }
        assert_eq!(Style::Tone(Tone::Warn).to_string(), "warn");
        assert_eq!(Style::Rgba([1, 2, 3, 4]).to_string(), "#01020304");
        assert_eq!(
            serde_json::to_string(&Draw::FillStyle(Tone::Good.into())).unwrap(),
            r#"{"fill_style":"good"}"#
        );
        assert!(serde_json::from_str::<Draw>(r#"{"fill_style":"url(x)"}"#).is_err());
    }

    #[test]
    fn the_context_records_commands_under_the_2d_context_s_names() {
        let mut ctx = Context2d::new(200.0, 80.0);
        ctx.save();
        ctx.set_fill_style(Tone::Info);
        ctx.set_global_alpha(0.5);
        ctx.fill_rect(1.0, 2.0, 3.0, 4.0);
        ctx.restore();
        ctx.begin_path();
        ctx.move_to(0.0, 0.0);
        ctx.line_to(10.0, 0.0);
        ctx.quadratic_curve_to(1.0, 2.0, 3.0, 4.0);
        ctx.bezier_curve_to(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
        ctx.arc(50.0, 40.0, 20.0, 0.0, 3.0);
        ctx.arc_with_anticlockwise(50.0, 40.0, 10.0, 3.0, 0.0, true);
        ctx.arc_with_anticlockwise(50.0, 40.0, 5.0, 0.0, 1.0, false);
        ctx.close_path();
        ctx.stroke();
        ctx.set_text_align(TextAlign::Center);
        ctx.set_font("bold 12px monospace");
        ctx.fill_text("70%", 50.0, 44.0);
        let node = ctx.finish();
        let json = serde_json::to_string(&node).unwrap();
        for part in [
            r#""save""#,
            r#"{"global_alpha":0.5}"#,
            r#"{"fill_rect":[1.0,2.0,3.0,4.0]}"#,
            r#""begin_path""#,
            r#"{"move_to":[0.0,0.0]}"#,
            r#"{"bezier_curve_to":[1.0,2.0,3.0,4.0,5.0,6.0]}"#,
            r#"{"arc":{"x":50.0,"y":40.0,"radius":20.0,"start_angle":0.0,"end_angle":3.0,"counterclockwise":false}}"#,
            r#"{"arc":{"x":50.0,"y":40.0,"radius":10.0,"start_angle":3.0,"end_angle":0.0,"counterclockwise":true}}"#,
            r#"{"arc":{"x":50.0,"y":40.0,"radius":5.0,"start_angle":0.0,"end_angle":1.0,"counterclockwise":false}}"#,
            r#"{"text_align":"center"}"#,
            r#"{"fill_text":{"text":"70%","x":50.0,"y":44.0}}"#,
        ] {
            assert!(json.contains(part), "{part} in {json}");
        }
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), node);
        assert_eq!(view(node).check(ViewKind::Run, &Limits::DEFAULT), Ok(()));
        let unknown = r#"{"canvas":{"width":1,"height":1,"commands":[{"draw_image":"x.png"}]}}"#;
        assert!(serde_json::from_str::<Node>(unknown).is_err());
    }

    fn canvas(width: f32, commands: Vec<Draw>) -> View {
        view(Node::Canvas {
            width,
            height: 100.0,
            commands,
        })
    }

    #[test]
    fn a_canvas_is_bounded_and_its_numbers_within_reach() {
        assert_eq!(
            canvas(100.0, vec![]).check(ViewKind::Run, &Limits::DEFAULT),
            Ok(())
        );
        for width in [0.0, -5.0, 4_001.0, f32::NAN] {
            assert_eq!(
                canvas(width, vec![]).check(ViewKind::Run, &Limits::DEFAULT),
                Err(Problem::CanvasSize(4_000))
            );
        }
        assert_eq!(
            canvas(100.0, vec![Draw::LineTo([1.0, -1e6])]).check(ViewKind::Run, &Limits::DEFAULT),
            Ok(())
        );
        for off in [f32::INFINITY, f32::NAN, 1.1e6, f32::MIN] {
            for command in [
                Draw::LineTo([1.0, off]),
                Draw::FillRect([0.0, 0.0, off, 1.0]),
                Draw::BezierCurveTo([0.0, 0.0, 0.0, 0.0, 0.0, off]),
                Draw::Arc {
                    x: 0.0,
                    y: 0.0,
                    radius: off,
                    start_angle: 0.0,
                    end_angle: 1.0,
                    counterclockwise: false,
                },
                Draw::LineWidth(off),
            ] {
                assert_eq!(
                    canvas(100.0, vec![Draw::Save, command]).check(ViewKind::Run, &Limits::DEFAULT),
                    Err(Problem::CanvasNumber(1))
                );
            }
        }
        assert_eq!(
            canvas(100.0, vec![Draw::Save; 20_001]).check(ViewKind::Run, &Limits::DEFAULT),
            Err(Problem::Commands(20_000))
        );
        let two = Node::Stack {
            children: vec![
                Node::Canvas {
                    width: 1.0,
                    height: 1.0,
                    commands: vec![Draw::Save; 10_001],
                };
                2
            ],
        };
        assert_eq!(
            view(two).check(ViewKind::Run, &Limits::DEFAULT),
            Err(Problem::Commands(20_000))
        );
        assert_eq!(
            canvas(100.0, vec![Draw::Font("x".repeat(20_001))])
                .check(ViewKind::Run, &Limits::DEFAULT),
            Err(Problem::Text(20_000))
        );
    }

    #[test]
    fn every_fill_and_stroke_counts_the_whole_path_it_draws() {
        let limits = Limits {
            points: 100,
            path_points: 20,
            ..Limits::DEFAULT
        };
        let mut drawn_once = vec![Draw::LineTo([0.0, 0.0]); 20];
        drawn_once.push(Draw::Stroke);
        assert_eq!(
            canvas(100.0, drawn_once.clone()).check(ViewKind::Run, &limits),
            Ok(())
        );

        let mut drawn_often = drawn_once.clone();
        drawn_often.extend(vec![Draw::Fill; 4]);
        assert_eq!(
            canvas(100.0, drawn_often.clone()).check(ViewKind::Run, &limits),
            Ok(())
        );
        drawn_often.push(Draw::Stroke);
        assert_eq!(
            canvas(100.0, drawn_often).check(ViewKind::Run, &limits),
            Err(Problem::Points(100))
        );

        let mut fresh = Vec::new();
        for _ in 0..5 {
            fresh.push(Draw::BeginPath);
            fresh.extend(drawn_once.clone());
        }
        assert_eq!(canvas(100.0, fresh).check(ViewKind::Run, &limits), Ok(()));

        let rects = |count: usize| {
            let mut commands = vec![Draw::FillRect([0.0, 0.0, 1.0, 1.0]); count];
            commands.insert(0, Draw::BeginPath);
            canvas(100.0, commands)
        };
        assert_eq!(rects(25).check(ViewKind::Run, &limits), Ok(()));
        assert_eq!(
            rects(26).check(ViewKind::Run, &limits),
            Err(Problem::Points(100))
        );
    }

    #[test]
    fn a_stretch_of_path_is_bounded_and_a_move_or_close_starts_another() {
        let limits = Limits {
            path_points: 20,
            ..Limits::DEFAULT
        };
        let line = Draw::LineTo([0.0, 0.0]);
        assert_eq!(
            canvas(100.0, vec![line.clone(); 21]).check(ViewKind::Run, &limits),
            Err(Problem::PathPoints(20))
        );
        for restart in [
            Draw::MoveTo([0.0, 0.0]),
            Draw::ClosePath,
            Draw::Rect([0.0; 4]),
        ] {
            let mut commands = vec![line.clone(); 19];
            commands.push(restart);
            commands.extend(vec![line.clone(); 19]);
            assert_eq!(
                canvas(100.0, commands).check(ViewKind::Run, &limits),
                Ok(())
            );
        }
        let curve = Draw::QuadraticCurveTo([0.0; 4]);
        assert_eq!(curve.path_points(), 65);
        assert_eq!(
            canvas(100.0, vec![curve]).check(ViewKind::Run, &limits),
            Err(Problem::PathPoints(20))
        );
        let arc = Draw::Arc {
            x: 0.0,
            y: 0.0,
            radius: 1.0,
            start_angle: 0.0,
            end_angle: 1.0,
            counterclockwise: false,
        };
        assert_eq!(arc.path_points(), 49);
        assert_eq!(Draw::Rect([0.0; 4]).path_points(), 5);
        assert_eq!(Draw::Fill.path_points(), 0);
    }

    #[test]
    fn transforms_baselines_widths_and_hints_go_under_the_2d_context_s_names() {
        let mut ctx = Context2d::new(200.0, 80.0);
        ctx.save();
        ctx.translate(10.0, 20.0);
        ctx.scale(2.0, 0.5);
        ctx.rotate(1.5);
        ctx.transform(1.0, 0.0, 0.0, 1.0, 3.0, 4.0);
        ctx.set_transform(1.0, 0.5, -0.5, 1.0, 0.0, 0.0);
        ctx.reset_transform();
        ctx.set_text_baseline(TextBaseline::Middle);
        ctx.fill_text("plain", 1.0, 2.0);
        ctx.fill_text_with_max_width("long", 1.0, 2.0, 40.0);
        ctx.hint(0.0, 0.0, 20.0, 10.0, "CVE-1");
        ctx.restore();
        let node = ctx.finish();
        let json = serde_json::to_string(&node).unwrap();
        for part in [
            r#"{"translate":[10.0,20.0]}"#,
            r#"{"scale":[2.0,0.5]}"#,
            r#"{"rotate":1.5}"#,
            r#"{"transform":[1.0,0.0,0.0,1.0,3.0,4.0]}"#,
            r#"{"set_transform":[1.0,0.5,-0.5,1.0,0.0,0.0]}"#,
            r#""reset_transform""#,
            r#"{"text_baseline":"middle"}"#,
            r#"{"fill_text":{"text":"plain","x":1.0,"y":2.0}}"#,
            r#"{"fill_text":{"text":"long","x":1.0,"y":2.0,"max_width":40.0}}"#,
            r#"{"hint":{"x":0.0,"y":0.0,"width":20.0,"height":10.0,"text":"CVE-1"}}"#,
        ] {
            assert!(json.contains(part), "{part} in {json}");
        }
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), node);
        assert_eq!(view(node).check(ViewKind::Run, &Limits::DEFAULT), Ok(()));
        for baseline in ["top", "middle", "alphabetic", "bottom"] {
            let json = format!(r#"{{"text_baseline":"{baseline}"}}"#);
            assert!(serde_json::from_str::<Draw>(&json).is_ok(), "{json}");
        }
        for off in [
            r#"{"text_baseline":"hanging"}"#,
            r#"{"rotate":[1.0]}"#,
            r#"{"translate":[1.0]}"#,
            r#"{"transform":[1.0,0.0,0.0,1.0,0.0]}"#,
            r#"{"hint":{"x":0,"y":0,"width":1,"height":1}}"#,
            r#"{"fill_text":{"text":"a","x":0,"y":0,"max_width":"wide"}}"#,
        ] {
            assert!(serde_json::from_str::<Draw>(off).is_err(), "{off}");
        }
    }

    #[test]
    fn every_transform_entry_text_width_and_hint_is_within_reach() {
        assert_eq!(
            canvas(
                100.0,
                vec![
                    Draw::Transform([1e6, -1e6, 1e6, -1e6, 1e6, -1e6]),
                    Draw::Rotate(-1e6),
                    Draw::FillText {
                        text: "a".into(),
                        x: 0.0,
                        y: 0.0,
                        max_width: Some(0.0),
                    },
                ]
            )
            .check(ViewKind::Run, &Limits::DEFAULT),
            Ok(())
        );
        for off in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1.1e6, f32::MAX] {
            for command in [
                Draw::Translate([0.0, off]),
                Draw::Scale([off, 1.0]),
                Draw::Rotate(off),
                Draw::Transform([1.0, 0.0, 0.0, 1.0, 0.0, off]),
                Draw::SetTransform([off, 0.0, 0.0, 1.0, 0.0, 0.0]),
                Draw::FillText {
                    text: "a".into(),
                    x: 0.0,
                    y: 0.0,
                    max_width: Some(off),
                },
                Draw::Hint {
                    x: 0.0,
                    y: 0.0,
                    width: off,
                    height: 1.0,
                    text: "a".into(),
                },
            ] {
                assert_eq!(
                    canvas(100.0, vec![Draw::ResetTransform, command.clone()])
                        .check(ViewKind::Run, &Limits::DEFAULT),
                    Err(Problem::CanvasNumber(1)),
                    "{command:?}"
                );
            }
        }
        for command in [
            Draw::Translate([0.0; 2]),
            Draw::Scale([0.0; 2]),
            Draw::Rotate(0.0),
            Draw::Transform([0.0; 6]),
            Draw::SetTransform([0.0; 6]),
            Draw::ResetTransform,
            Draw::TextBaseline(TextBaseline::Top),
            Draw::Hint {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
                text: String::new(),
            },
        ] {
            assert_eq!(command.path_points(), 0, "{command:?}");
        }
    }

    #[test]
    fn hints_are_items_across_the_view_and_their_text_is_bounded() {
        let limits = Limits {
            items: 3,
            text: 8,
            ..Limits::DEFAULT
        };
        let hint = |text: &str| Draw::Hint {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            text: text.into(),
        };
        assert_eq!(canvas(100.0, vec![]).check(ViewKind::Run, &limits), Ok(()));
        assert_eq!(
            canvas(100.0, vec![hint(""); 3]).check(ViewKind::Run, &limits),
            Ok(())
        );
        assert_eq!(
            canvas(100.0, vec![hint("same"); 4]).check(ViewKind::Run, &limits),
            Err(Problem::Items(3)),
            "duplicates count"
        );
        let two = Node::Stack {
            children: vec![
                Node::Canvas {
                    width: 1.0,
                    height: 1.0,
                    commands: vec![hint("a"); 2],
                };
                2
            ],
        };
        assert_eq!(
            view(two).check(ViewKind::Run, &limits),
            Err(Problem::Items(3))
        );
        assert_eq!(
            canvas(100.0, vec![hint("12345678")]).check(ViewKind::Run, &limits),
            Ok(())
        );
        assert_eq!(
            canvas(100.0, vec![hint("123456789")]).check(ViewKind::Run, &limits),
            Err(Problem::Text(8))
        );
        let long_text = Draw::FillText {
            text: "123456789".into(),
            x: 0.0,
            y: 0.0,
            max_width: Some(5.0),
        };
        assert_eq!(
            canvas(100.0, vec![long_text]).check(ViewKind::Run, &limits),
            Err(Problem::Text(8))
        );
    }

    fn hit(id: &str, x: f32, width: f32) -> Draw {
        Draw::Hit {
            id: id.into(),
            x,
            y: 0.0,
            width,
            height: 4.0,
        }
    }

    #[test]
    fn a_hit_goes_under_its_own_name_and_reads_back() {
        let mut ctx = Context2d::new(10.0, 10.0);
        ctx.translate(2.0, 3.0);
        ctx.hit(0.0, 0.0, 4.0, 4.0, "left");
        let node = ctx.finish();
        let json = serde_json::to_string(&node).unwrap();
        assert_eq!(
            json,
            r#"{"canvas":{"width":10.0,"height":10.0,"commands":[{"translate":[2.0,3.0]},{"hit":{"id":"left","x":0.0,"y":0.0,"width":4.0,"height":4.0}}]}}"#
        );
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), node);
        assert_eq!(view(node).check(ViewKind::Run, &Limits::DEFAULT), Ok(()));
        assert_eq!(hit("a", 0.0, 1.0).path_points(), 0);
        for off in [
            r#"{"hit":{"x":0,"y":0,"width":1,"height":1}}"#,
            r#"{"hit":{"id":"a","x":0,"y":0,"width":1}}"#,
            r#"{"hit":{"id":1,"x":0,"y":0,"width":1,"height":1}}"#,
            r#"{"hit":{"id":"a","x":0,"y":0,"width":1,"height":1,"tone":"bad"}}"#,
        ] {
            assert!(serde_json::from_str::<Draw>(off).is_err(), "{off}");
        }
    }

    #[test]
    fn a_hit_s_numbers_are_within_reach() {
        for fine in [0.0, -1e6, 1e6] {
            assert_eq!(
                canvas(100.0, vec![hit("a", fine, fine)]).check(ViewKind::Run, &Limits::DEFAULT),
                Ok(()),
                "{fine}"
            );
        }
        for off in [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            1.000_001e6,
            f32::MAX,
        ] {
            for command in [hit("a", off, 1.0), hit("a", 0.0, off)] {
                assert_eq!(
                    canvas(100.0, vec![Draw::Save, command.clone()])
                        .check(ViewKind::Run, &Limits::DEFAULT),
                    Err(Problem::CanvasNumber(1)),
                    "{command:?}"
                );
            }
        }
    }

    #[test]
    fn hits_are_items_across_the_view_and_their_ids_are_bounded() {
        let limits = Limits {
            items: 3,
            text: 8,
            ..Limits::DEFAULT
        };
        assert_eq!(
            canvas(100.0, vec![hit("a", 0.0, 1.0); 3]).check(ViewKind::Run, &limits),
            Ok(())
        );
        assert_eq!(
            canvas(100.0, vec![hit("a", 0.0, 1.0); 4]).check(ViewKind::Run, &limits),
            Err(Problem::Items(3)),
            "the same id counts again"
        );
        let hinted = Draw::Hint {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            text: "h".into(),
        };
        assert_eq!(
            canvas(
                100.0,
                vec![
                    hinted.clone(),
                    hinted,
                    hit("a", 0.0, 1.0),
                    hit("b", 0.0, 1.0)
                ]
            )
            .check(ViewKind::Run, &limits),
            Err(Problem::Items(3)),
            "hints and hits count together"
        );
        let two = Node::Stack {
            children: vec![
                Node::Canvas {
                    width: 1.0,
                    height: 1.0,
                    commands: vec![hit("a", 0.0, 1.0); 2],
                };
                2
            ],
        };
        assert_eq!(
            view(two).check(ViewKind::Runs, &limits),
            Err(Problem::Items(3))
        );
        assert_eq!(
            canvas(100.0, vec![hit("12345678", 0.0, 1.0)]).check(ViewKind::Run, &limits),
            Ok(())
        );
        assert_eq!(
            canvas(100.0, vec![hit("123456789", 0.0, 1.0)]).check(ViewKind::Run, &limits),
            Err(Problem::Text(8))
        );
        assert_eq!(
            canvas(100.0, vec![hit("", 0.0, 1.0)]).check(ViewKind::Run, &limits),
            Ok(())
        );
    }
}

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
//! gradients or patterns; there are no transforms, clips or images; text is drawn over every
//! shape; and a font is read only for its `px` size and the words `bold` and `monospace`.

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
    Save,
    Restore,
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
            Draw::GlobalAlpha(n) | Draw::LineWidth(n) => near(n),
            Draw::MoveTo(n) | Draw::LineTo(n) => n.iter().all(near),
            Draw::QuadraticCurveTo(n) | Draw::Rect(n) | Draw::FillRect(n) | Draw::StrokeRect(n) => {
                n.iter().all(near)
            }
            Draw::BezierCurveTo(n) => n.iter().all(near),
            Draw::Arc {
                x,
                y,
                radius,
                start_angle,
                end_angle,
                ..
            } => [x, y, radius, start_angle, end_angle].into_iter().all(near),
            Draw::FillText { x, y, .. } => near(x) && near(y),
            Draw::FillStyle(_)
            | Draw::StrokeStyle(_)
            | Draw::LineCap(_)
            | Draw::Font(_)
            | Draw::TextAlign(_)
            | Draw::Save
            | Draw::Restore
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

    pub fn save(&mut self) {
        self.commands.push(Draw::Save);
    }

    pub fn restore(&mut self) {
        self.commands.push(Draw::Restore);
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
        self.commands.push(Draw::Arc {
            x,
            y,
            radius,
            start_angle,
            end_angle,
            counterclockwise: false,
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
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::{Context2d, Draw, Limits, Node, Problem, Style, TextAlign, Tone, View};

    fn view(body: Node) -> View {
        View {
            title: "T".into(),
            body,
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
            r#"{"text_align":"center"}"#,
            r#"{"fill_text":{"text":"70%","x":50.0,"y":44.0}}"#,
        ] {
            assert!(json.contains(part), "{part} in {json}");
        }
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), node);
        assert_eq!(view(node).check(&Limits::DEFAULT), Ok(()));
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
        assert_eq!(canvas(100.0, vec![]).check(&Limits::DEFAULT), Ok(()));
        for width in [0.0, -5.0, 4_001.0, f32::NAN] {
            assert_eq!(
                canvas(width, vec![]).check(&Limits::DEFAULT),
                Err(Problem::CanvasSize(4_000))
            );
        }
        assert_eq!(
            canvas(100.0, vec![Draw::LineTo([1.0, -1e6])]).check(&Limits::DEFAULT),
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
                    canvas(100.0, vec![Draw::Save, command]).check(&Limits::DEFAULT),
                    Err(Problem::CanvasNumber(1))
                );
            }
        }
        assert_eq!(
            canvas(100.0, vec![Draw::Save; 20_001]).check(&Limits::DEFAULT),
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
            view(two).check(&Limits::DEFAULT),
            Err(Problem::Commands(20_000))
        );
        assert_eq!(
            canvas(100.0, vec![Draw::Font("x".repeat(20_001))]).check(&Limits::DEFAULT),
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
        assert_eq!(canvas(100.0, drawn_once.clone()).check(&limits), Ok(()));

        let mut drawn_often = drawn_once.clone();
        drawn_often.extend(vec![Draw::Fill; 4]);
        assert_eq!(canvas(100.0, drawn_often.clone()).check(&limits), Ok(()));
        drawn_often.push(Draw::Stroke);
        assert_eq!(
            canvas(100.0, drawn_often).check(&limits),
            Err(Problem::Points(100))
        );

        let mut fresh = Vec::new();
        for _ in 0..5 {
            fresh.push(Draw::BeginPath);
            fresh.extend(drawn_once.clone());
        }
        assert_eq!(canvas(100.0, fresh).check(&limits), Ok(()));

        let rects = |count: usize| {
            let mut commands = vec![Draw::FillRect([0.0, 0.0, 1.0, 1.0]); count];
            commands.insert(0, Draw::BeginPath);
            canvas(100.0, commands)
        };
        assert_eq!(rects(25).check(&limits), Ok(()));
        assert_eq!(rects(26).check(&limits), Err(Problem::Points(100)));
    }

    #[test]
    fn a_stretch_of_path_is_bounded_and_a_move_or_close_starts_another() {
        let limits = Limits {
            path_points: 20,
            ..Limits::DEFAULT
        };
        let line = Draw::LineTo([0.0, 0.0]);
        assert_eq!(
            canvas(100.0, vec![line.clone(); 21]).check(&limits),
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
            assert_eq!(canvas(100.0, commands).check(&limits), Ok(()));
        }
        let curve = Draw::QuadraticCurveTo([0.0; 4]);
        assert_eq!(curve.path_points(), 65);
        assert_eq!(
            canvas(100.0, vec![curve]).check(&limits),
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
}

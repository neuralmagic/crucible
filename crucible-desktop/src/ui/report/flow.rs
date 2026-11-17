//! Lays a view's flow out as canvas commands: a bar per stage in its column, as tall as what
//! flows through it, and a ribbon per link.
//!
//! ```text
//!        column 0          column 1         column 2
//!   Go 34 ▐█▌━━━━━━━━━━━━━▐█▌ reach 40 ━━━━▐█▌ fix 8
//!         ▐█▌━━━━┓  ┏━━━━━▐█▌━━━━━━━━━━━━━━▐█▌ close 56
//!  Py 30  ▐█▌━━━━╋━━┛     ▐█▌ scope 24 ━━━━▐█▌
//!         ▐█▌━━━━┻━━━━━━━━▐█▌
//! ```

use std::collections::HashMap;

use crucible_view::{Draw, FlowLink, FlowNode, Style, TextAlign, Tone, flow_columns};

use crate::ui::report::hue::{Hue, Hues};

/// The drawing's width, and the height its tallest column fills.
pub(crate) const WIDTH: f32 = 600.0;
const HEIGHT: f32 = 240.0;
/// The room beside the first and last columns for their labels.
const GUTTER: f32 = 130.0;
const BAR: f32 = 10.0;
/// Between two stages of one column.
const GAP: f32 = 10.0;
const MARGIN: f32 = 8.0;

/// Where a stage's bar is drawn, and how much flows through the stage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placed {
    pub x: f32,
    pub top: f32,
    pub height: f32,
    pub through: f64,
}

/// Each stage's bar, in the nodes' order, and how many points one unit of value is tall.
pub(crate) fn place(nodes: &[FlowNode], links: &[FlowLink]) -> Option<(Vec<Placed>, f64)> {
    let columns = flow_columns(nodes, links).ok()?;
    let count = columns.iter().max()? + 1;
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let (mut flows_in, mut flows_out) = (vec![0.0_f64; nodes.len()], vec![0.0_f64; nodes.len()]);
    for link in links {
        flows_out[*index.get(link.from.as_str())?] += link.value;
        flows_in[*index.get(link.to.as_str())?] += link.value;
    }
    let through: Vec<f64> = flows_in
        .iter()
        .zip(&flows_out)
        .map(|(a, b)| a.max(*b))
        .collect();
    let members: Vec<Vec<usize>> = (0..count)
        .map(|column| (0..nodes.len()).filter(|i| columns[*i] == column).collect())
        .collect();
    let gaps = |column: &[usize]| GAP * column.len().saturating_sub(1) as f32;
    let scale = members
        .iter()
        .filter_map(|column| {
            let total: f64 = column.iter().map(|i| through[*i]).sum();
            (total > 0.0).then(|| f64::from((HEIGHT - gaps(column)).max(1.0)) / total)
        })
        .fold(f64::INFINITY, f64::min);
    let scale = if scale.is_finite() { scale } else { 0.0 };
    let tall = |i: usize| (through[i] * scale) as f32;
    let step = if count > 1 {
        (WIDTH - 2.0 * GUTTER - BAR) / (count - 1) as f32
    } else {
        0.0
    };
    let mut placed = vec![
        Placed {
            x: 0.0,
            top: 0.0,
            height: 0.0,
            through: 0.0,
        };
        nodes.len()
    ];
    for (column, inside) in members.iter().enumerate() {
        let used: f32 = inside.iter().map(|i| tall(*i)).sum::<f32>() + gaps(inside);
        let mut top = MARGIN + (HEIGHT - used) / 2.0;
        for &i in inside {
            let height = tall(i);
            placed[i] = Placed {
                x: GUTTER + step * column as f32,
                top,
                height,
                through: through[i],
            };
            top += height + GAP;
        }
    }
    Some((placed, scale))
}

fn short(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

/// The flow as a canvas of [`WIDTH`] by the returned height, a neutral stage in the next of
/// `palette`; `None` for one the catalog's check would have refused.
pub(crate) fn drawing(
    nodes: &[FlowNode],
    links: &[FlowLink],
    palette: &[Style],
) -> Option<(f32, Vec<Draw>)> {
    let (placed, scale) = place(nodes, links)?;
    let index: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut ordered: Vec<(usize, usize, &FlowLink)> = links
        .iter()
        .filter_map(|link| {
            Some((
                *index.get(link.from.as_str())?,
                *index.get(link.to.as_str())?,
                link,
            ))
        })
        .collect();
    ordered.sort_by(|a, b| {
        let key = |(from, to, _): &(usize, usize, &FlowLink)| {
            (placed[*from].x, placed[*from].top, placed[*to].top)
        };
        key(a)
            .partial_cmp(&key(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut hues = Hues::default();
    let styles: Vec<Style> = nodes
        .iter()
        .map(|node| match hues.pick(&node.id, node.tone) {
            Hue::Tone(tone) => Style::Tone(tone),
            Hue::Palette(i) => palette
                .get(i % palette.len().max(1))
                .copied()
                .unwrap_or(Style::Tone(Tone::Muted)),
        })
        .collect();
    let mut commands = vec![Draw::GlobalAlpha(0.35)];
    let (mut left, mut entered) = (vec![0.0_f32; nodes.len()], vec![0.0_f32; nodes.len()]);
    for (from, to, link) in ordered {
        let thick = (link.value * scale) as f32;
        let (start, end) = (placed[from], placed[to]);
        let (x0, x1) = (start.x + BAR, end.x);
        let (y0, y1) = (start.top + left[from], end.top + entered[to]);
        left[from] += thick;
        entered[to] += thick;
        let middle = (x0 + x1) / 2.0;
        commands.extend([
            Draw::FillStyle(match link.tone {
                Some(Tone::Neutral) | None => styles[to],
                Some(tone) => Style::Tone(tone),
            }),
            Draw::BeginPath,
            Draw::MoveTo([x0, y0]),
            Draw::BezierCurveTo([middle, y0, middle, y1, x1, y1]),
            Draw::LineTo([x1, y1 + thick]),
            Draw::BezierCurveTo([middle, y1 + thick, middle, y0 + thick, x0, y0 + thick]),
            Draw::ClosePath,
            Draw::Fill,
        ]);
    }

    commands.push(Draw::GlobalAlpha(1.0));
    for (style, at) in styles.iter().zip(&placed) {
        commands.extend([
            Draw::FillStyle(*style),
            Draw::FillRect([at.x, at.top, BAR, at.height.max(1.0)]),
        ]);
    }

    commands.push(Draw::Font("11px sans-serif".into()));
    let first = placed.iter().map(|at| at.x).fold(f32::INFINITY, f32::min);
    for (node, at) in nodes.iter().zip(&placed) {
        let leading = at.x <= first;
        commands.extend([
            Draw::FillStyle(Style::Tone(match node.tone {
                Tone::Muted => Tone::Neutral,
                tone => tone,
            })),
            Draw::TextAlign(if leading {
                TextAlign::Right
            } else {
                TextAlign::Left
            }),
            Draw::FillText {
                text: format!("{}  {}", node.label, short(at.through)),
                x: if leading {
                    at.x - 8.0
                } else {
                    at.x + BAR + 8.0
                },
                y: at.top + at.height / 2.0 + 4.0,
            },
        ]);
    }
    Some((HEIGHT + 2.0 * MARGIN, commands))
}

#[cfg(test)]
mod tests {
    use crucible_view::{Draw, FlowLink, FlowNode, Style, TextAlign, Tone};

    use crate::ui::report::flow::{WIDTH, drawing, place};

    const PALETTE: [Style; 2] = [Style::Rgba([1, 2, 3, 255]), Style::Rgba([4, 5, 6, 255])];

    fn node(id: &str, tone: Tone) -> FlowNode {
        FlowNode {
            id: id.into(),
            label: id.into(),
            tone,
        }
    }

    fn link(from: &str, to: &str, value: f64) -> FlowLink {
        FlowLink {
            from: from.into(),
            to: to.into(),
            value,
            tone: None,
        }
    }

    fn triage() -> (Vec<FlowNode>, Vec<FlowLink>) {
        (
            vec![
                node("go", Tone::Muted),
                node("python", Tone::Muted),
                node("reach", Tone::Info),
                node("scope", Tone::Info),
                node("fix", Tone::Bad),
                node("close", Tone::Good),
            ],
            vec![
                link("go", "reach", 30.0),
                link("go", "scope", 4.0),
                link("python", "reach", 10.0),
                link("python", "scope", 20.0),
                link("reach", "fix", 8.0),
                link("reach", "close", 32.0),
                link("scope", "close", 24.0),
            ],
        )
    }

    #[test]
    fn bars_are_as_tall_as_what_flows_through_them_and_fit_the_drawing() {
        let (nodes, links) = triage();
        let (placed, scale) = place(&nodes, &links).unwrap();
        let tall = |i: usize| (f64::from(placed[i].height) / scale).round();
        assert_eq!(
            [tall(0), tall(1), tall(2), tall(3), tall(4), tall(5)],
            [34.0, 30.0, 40.0, 24.0, 8.0, 56.0]
        );
        let xs: Vec<f32> = placed.iter().map(|at| at.x).collect();
        assert_eq!(xs[0], xs[1]);
        assert!(xs[0] < xs[2] && xs[2] < xs[4], "{xs:?}");
        assert_eq!(xs[2] - xs[0], xs[4] - xs[2], "columns are evenly spaced");
        assert!(xs[4] + 10.0 < WIDTH - 100.0, "room for the last labels");
        for (a, b) in [(0, 1), (2, 3), (4, 5)] {
            assert!(
                placed[a].top + placed[a].height < placed[b].top,
                "no overlap"
            );
        }
        assert!(
            placed
                .iter()
                .all(|at| at.top >= 0.0 && at.top + at.height <= 256.0),
            "{placed:?}"
        );
    }

    #[test]
    fn ribbons_fill_their_bars_edge_to_edge_and_take_the_tone_they_reach() {
        let (nodes, links) = triage();
        let (placed, scale) = place(&nodes, &links).unwrap();
        let (height, commands) = drawing(&nodes, &links, &PALETTE).unwrap();
        assert_eq!(height, 256.0);
        let starts: Vec<[f32; 2]> = commands
            .iter()
            .filter_map(|command| match command {
                Draw::MoveTo(at) => Some(*at),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 7, "a ribbon per link");
        assert_eq!(starts[0], [placed[0].x + 10.0, placed[0].top]);
        assert_eq!(
            starts[1][1],
            placed[0].top + (30.0 * scale) as f32,
            "stacked down the bar"
        );

        let fills: Vec<Style> = commands
            .iter()
            .filter_map(|command| match command {
                Draw::FillStyle(style) => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(
            fills[..7],
            [
                Style::Tone(Tone::Info),
                Style::Tone(Tone::Info),
                Style::Tone(Tone::Info),
                Style::Tone(Tone::Info),
                Style::Tone(Tone::Bad),
                Style::Tone(Tone::Good),
                Style::Tone(Tone::Good),
            ]
        );
        assert!(matches!(commands[0], Draw::GlobalAlpha(alpha) if alpha < 1.0));

        let labels: Vec<(&str, f32)> = commands
            .iter()
            .filter_map(|command| match command {
                Draw::FillText { text, x, .. } => Some((text.as_str(), *x)),
                _ => None,
            })
            .collect();
        assert_eq!(labels[0], ("go  34", placed[0].x - 8.0));
        assert_eq!(labels[5], ("close  56", placed[5].x + 18.0));
        let aligned: Vec<TextAlign> = commands
            .iter()
            .filter_map(|command| match command {
                Draw::TextAlign(align) => Some(*align),
                _ => None,
            })
            .collect();
        assert_eq!(aligned[0], TextAlign::Right);
        assert_eq!(aligned[2], TextAlign::Left);
    }

    #[test]
    fn a_ribbon_s_own_tone_wins_and_odd_flows_still_draw() {
        let nodes = vec![node("a", Tone::Neutral), node("b", Tone::Good)];
        let toned = vec![FlowLink {
            tone: Some(Tone::Warn),
            ..link("a", "b", 2.5)
        }];
        let (_, commands) = drawing(&nodes, &toned, &PALETTE).unwrap();
        assert!(commands.contains(&Draw::FillStyle(Style::Tone(Tone::Warn))));
        let bars: Vec<Style> = commands
            .iter()
            .zip(commands.iter().skip(1))
            .filter_map(|(style, next)| match (style, next) {
                (Draw::FillStyle(style), Draw::FillRect(_)) => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(
            bars,
            [PALETTE[0], Style::Tone(Tone::Good)],
            "a neutral stage takes the palette's first colour"
        );
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, Draw::FillText { text, .. } if text == "a  2.5"))
        );

        let (placed, scale) = place(&nodes, &[]).unwrap();
        assert_eq!(scale, 0.0);
        assert!(placed.iter().all(|at| at.height == 0.0));
        assert!(drawing(&nodes, &[], &PALETTE).is_some());

        let neutral: Vec<FlowNode> = ["a", "b", "c", "d"]
            .map(|id| node(id, Tone::Neutral))
            .into_iter()
            .chain([node("e", Tone::Muted)])
            .collect();
        let fed = vec![
            link("a", "b", 1.0),
            link("c", "d", 1.0),
            link("a", "e", 1.0),
        ];
        let stages = |palette: &[Style]| -> Vec<Style> {
            let (_, commands) = drawing(&neutral, &fed, palette).unwrap();
            commands
                .iter()
                .zip(commands.iter().skip(1))
                .filter_map(|(style, next)| match (style, next) {
                    (Draw::FillStyle(style), Draw::FillRect(_)) => Some(*style),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            stages(&PALETTE),
            [
                PALETTE[0],
                PALETTE[1],
                PALETTE[0],
                PALETTE[1],
                Style::Tone(Tone::Muted)
            ],
            "the palette wraps and a muted stage stays muted"
        );
        assert_eq!(stages(&[])[0], Style::Tone(Tone::Muted), "no palette");
        let (_, commands) = drawing(&neutral, &fed, &PALETTE).unwrap();
        let ribbons: Vec<Style> = commands
            .iter()
            .zip(commands.iter().skip(1))
            .filter_map(|(style, next)| match (style, next) {
                (Draw::FillStyle(style), Draw::BeginPath) => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(
            ribbons,
            [PALETTE[1], Style::Tone(Tone::Muted), PALETTE[1]],
            "a ribbon takes the colour of the stage it reaches"
        );
        let plain: Vec<FlowLink> = fed
            .iter()
            .map(|one| FlowLink {
                tone: Some(Tone::Neutral),
                ..one.clone()
            })
            .collect();
        let (_, commands) = drawing(&neutral, &plain, &PALETTE).unwrap();
        let plain_ribbons: Vec<Style> = commands
            .iter()
            .zip(commands.iter().skip(1))
            .filter_map(|(style, next)| match (style, next) {
                (Draw::FillStyle(style), Draw::BeginPath) => Some(*style),
                _ => None,
            })
            .collect();
        assert_eq!(
            plain_ribbons, ribbons,
            "a ribbon left neutral takes its stage's colour"
        );
        assert!(drawing(&[], &[], &PALETTE).is_none());
        assert!(drawing(&nodes, &[link("a", "nowhere", 1.0)], &PALETTE).is_none());
    }

    #[test]
    fn values_at_either_end_of_what_the_catalog_allows_stay_on_the_drawing() {
        let nodes = vec![
            node("a", Tone::Neutral),
            node("b", Tone::Good),
            node("c", Tone::Bad),
        ];
        for links in [
            vec![link("a", "b", 1e-300), link("a", "c", 1e-300)],
            vec![link("a", "b", 1e15), link("a", "c", 1e15)],
            vec![link("a", "b", 1e-300), link("a", "c", 1e15)],
            vec![link("a", "b", 5e-324)],
        ] {
            let (placed, scale) = place(&nodes, &links).unwrap();
            assert!(scale.is_finite());
            assert!(
                placed.iter().all(|at| at.top.is_finite()
                    && (0.0..=240.0).contains(&at.height)
                    && at.top >= 0.0
                    && at.top + at.height <= 256.0),
                "{placed:?}"
            );
            let (_, commands) = drawing(&nodes, &links, &PALETTE).unwrap();
            let view = crucible_view::View {
                title: "T".into(),
                body: crucible_view::Node::Canvas {
                    width: WIDTH,
                    height: 256.0,
                    commands,
                },
            };
            assert_eq!(view.check(&crucible_view::Limits::DEFAULT), Ok(()));
        }
    }
}

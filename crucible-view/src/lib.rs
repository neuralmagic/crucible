//! The components a pack's view may use, and the documents a view plugin exchanges with its host.
//!
//! A plugin is a WebAssembly module with no imports. The host calls `crucible_inputs` for the
//! captured file paths it reads, then `crucible_view` with a [`Request`] holding those files, and
//! gets a [`Reply`]. The host draws the [`View`]; the plugin draws nothing.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

mod canvas;
mod flow;
#[cfg(target_arch = "wasm32")]
pub mod guest;

pub use crate::canvas::{Context2d, Draw, LineCap, NotAStyle, Style, TextAlign};
pub use crate::flow::{FlowLink, FlowNode, flow_columns};

/// The export that reserves `len` bytes in the plugin's memory and returns their address.
pub const ALLOC: &str = "crucible_alloc";
/// The export that returns the captured file paths the plugin reads, as a JSON list.
pub const INPUTS: &str = "crucible_inputs";
/// The export that takes a [`Request`] and returns a [`Reply`].
pub const VIEW: &str = "crucible_view";

/// A captured file handed to a plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    /// The path the task declared.
    pub path: String,
    /// The task that captured it, with its instance key when it has one.
    pub task: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub files: Vec<File>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    View(View),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub title: String,
    pub body: Node,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// Children top to bottom.
    Stack { children: Vec<Node> },
    /// One body shown at a time.
    Tabs { id: String, tabs: Vec<Tab> },
    /// A row of headline numbers.
    Stats { items: Vec<Stat> },
    /// Horizontal bars on one scale, each split into segments.
    Bars { bars: Vec<Bar> },
    /// Points on shared axes, joined into lines or left as marks.
    Plot {
        kind: PlotKind,
        /// What the horizontal axis measures.
        x: String,
        /// What the vertical axis measures.
        y: String,
        series: Vec<Series>,
    },
    /// Quantities flowing between stages, drawn as ribbons: a Sankey diagram.
    Flow {
        nodes: Vec<FlowNode>,
        links: Vec<FlowLink>,
    },
    /// Free drawing, built with [`Context2d`].
    Canvas {
        width: f32,
        height: f32,
        commands: Vec<Draw>,
    },
    /// A small table drawn whole, such as a pivot.
    Grid {
        columns: Vec<String>,
        rows: Vec<Vec<Cell>>,
    },
    /// A sortable table of any length.
    Table {
        id: String,
        columns: Vec<Column>,
        rows: Vec<Row>,
    },
    Text {
        text: String,
        #[serde(default)]
        tone: Tone,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tab {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    pub body: Node,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stat {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub tone: Tone,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bar {
    pub label: String,
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub label: String,
    /// Finite and not negative.
    pub value: f64,
    #[serde(default)]
    pub tone: Tone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlotKind {
    Line,
    Scatter,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Series {
    pub label: String,
    #[serde(default)]
    pub tone: Tone,
    /// Each point's x then y, both finite.
    pub points: Vec<[f64; 2]>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub kind: ColumnKind,
    #[serde(default)]
    pub width: Width,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnKind {
    #[default]
    Text,
    Mono,
    /// A label coloured by the cell's tone.
    Badge,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Width {
    Narrow,
    #[default]
    Medium,
    Wide,
    /// Shares the width the other columns leave.
    Fill,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// Unique within its table.
    pub id: String,
    pub cells: Vec<Cell>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub text: String,
    /// An http(s) address the cell opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub href: Option<String>,
    #[serde(default)]
    pub tone: Tone,
}

impl Cell {
    pub fn text(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            ..Cell::default()
        }
    }

    pub fn link(mut self, href: impl Into<String>) -> Cell {
        self.href = Some(href.into());
        self
    }

    pub fn tone(mut self, tone: Tone) -> Cell {
        self.tone = tone;
        self
    }
}

/// A colour role from the host's theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Neutral,
    Muted,
    Good,
    Warn,
    Bad,
    Info,
}

/// How much a view may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub nodes: usize,
    pub depth: usize,
    pub cells: usize,
    pub grid_cells: usize,
    /// Segments across one set of bars.
    pub segments: usize,
    /// Points across one plot.
    pub points: usize,
    /// Stages and ribbons in one flow.
    pub flow_nodes: usize,
    pub flow_links: usize,
    /// Commands in one canvas.
    pub commands: usize,
    /// The longest side of a canvas, in points.
    pub canvas_side: u16,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        nodes: 2_000,
        depth: 8,
        cells: 500_000,
        grid_cells: 600,
        segments: 1_000,
        points: 20_000,
        flow_nodes: 60,
        flow_links: 400,
        commands: 20_000,
        canvas_side: 4_000,
    };
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("the view has more than {0} nodes")]
    Nodes(usize),
    #[error("the view nests deeper than {0}")]
    Depth(usize),
    #[error("the view's tables hold more than {0} cells")]
    Cells(usize),
    #[error("a grid holds more than {0} cells")]
    GridCells(usize),
    #[error("a set of bars holds more than {0} segments")]
    Segments(usize),
    #[error("the bar segment {0} is negative or not a number")]
    Value(String),
    #[error("a flow holds more than {0} stages")]
    FlowNodes(usize),
    #[error("a flow holds more than {0} ribbons")]
    FlowLinks(usize),
    #[error("a flow has two stages named {0}")]
    FlowDuplicate(String),
    #[error("a flow's ribbon names the stage {0}, which it does not have")]
    FlowUnknown(String),
    #[error("the ribbon from {0} to {1} has a value that is not greater than zero")]
    FlowValue(String, String),
    #[error("a flow's ribbons lead back to a stage they came from")]
    FlowCycle,
    #[error("a canvas is not between 1 and {0} points on each side")]
    CanvasSize(u16),
    #[error("a canvas holds more than {0} commands")]
    Commands(usize),
    #[error("canvas command {0} holds a number that is not finite")]
    CanvasNumber(usize),
    #[error("a plot holds more than {0} points")]
    Points(usize),
    #[error("the series {0} has a point that is not a number")]
    Point(String),
    #[error("{0} has a row of {1} cells for {2} columns")]
    RowWidth(String, usize, usize),
    #[error("two components are named {0}")]
    DuplicateId(String),
    #[error("table {0} has two rows named {1}")]
    DuplicateRow(String, String),
    #[error("table {0} has two columns named {1}")]
    DuplicateColumn(String, String),
    #[error("tabs {0} has no tabs")]
    NoTabs(String),
    #[error("more than one table is shown at once")]
    Tables,
    #[error("{0} is not an http(s) address")]
    Href(String),
}

impl View {
    /// Whether the host can draw this view within `limits`.
    pub fn check(&self, limits: &Limits) -> Result<(), Problem> {
        let mut seen = Seen::default();
        let shown = self.body.check(limits, 1, &mut seen)?;
        if shown > 1 {
            return Err(Problem::Tables);
        }
        Ok(())
    }
}

#[derive(Default)]
struct Seen {
    nodes: usize,
    cells: usize,
    ids: HashSet<String>,
}

impl Seen {
    fn name(&mut self, id: &str) -> Result<(), Problem> {
        if self.ids.insert(id.to_string()) {
            Ok(())
        } else {
            Err(Problem::DuplicateId(id.to_string()))
        }
    }
}

impl Node {
    /// The most tables this node can show at once.
    fn check(&self, limits: &Limits, depth: usize, seen: &mut Seen) -> Result<usize, Problem> {
        if depth > limits.depth {
            return Err(Problem::Depth(limits.depth));
        }
        seen.nodes += 1;
        if seen.nodes > limits.nodes {
            return Err(Problem::Nodes(limits.nodes));
        }
        match self {
            Node::Stack { children } => {
                let mut shown = 0;
                for child in children {
                    shown += child.check(limits, depth + 1, seen)?;
                }
                Ok(shown)
            }
            Node::Tabs { id, tabs } => {
                seen.name(id)?;
                if tabs.is_empty() {
                    return Err(Problem::NoTabs(id.clone()));
                }
                let mut shown = 0;
                for tab in tabs {
                    shown = shown.max(tab.body.check(limits, depth + 1, seen)?);
                }
                Ok(shown)
            }
            Node::Stats { .. } | Node::Text { .. } => Ok(0),
            Node::Bars { bars } => {
                let segments: usize = bars.iter().map(|bar| bar.segments.len()).sum();
                if segments > limits.segments {
                    return Err(Problem::Segments(limits.segments));
                }
                let off = bars
                    .iter()
                    .flat_map(|bar| &bar.segments)
                    .find(|segment| !segment.value.is_finite() || segment.value < 0.0);
                match off {
                    Some(segment) => Err(Problem::Value(segment.label.clone())),
                    None => Ok(0),
                }
            }
            Node::Flow { nodes, links } => {
                if nodes.len() > limits.flow_nodes {
                    return Err(Problem::FlowNodes(limits.flow_nodes));
                }
                if links.len() > limits.flow_links {
                    return Err(Problem::FlowLinks(limits.flow_links));
                }
                flow_columns(nodes, links).map(|_| 0)
            }
            Node::Canvas {
                width,
                height,
                commands,
            } => {
                let side = f32::from(limits.canvas_side);
                if ![width, height].iter().all(|n| (1.0..=side).contains(*n)) {
                    return Err(Problem::CanvasSize(limits.canvas_side));
                }
                if commands.len() > limits.commands {
                    return Err(Problem::Commands(limits.commands));
                }
                match commands.iter().position(|command| !command.finite()) {
                    Some(at) => Err(Problem::CanvasNumber(at)),
                    None => Ok(0),
                }
            }
            Node::Plot { series, .. } => {
                let points: usize = series.iter().map(|s| s.points.len()).sum();
                if points > limits.points {
                    return Err(Problem::Points(limits.points));
                }
                let off = series
                    .iter()
                    .find(|s| s.points.iter().flatten().any(|v| !v.is_finite()));
                match off {
                    Some(series) => Err(Problem::Point(series.label.clone())),
                    None => Ok(0),
                }
            }
            Node::Grid { columns, rows } => {
                let cells = columns.len() * (rows.len() + 1);
                if cells > limits.grid_cells {
                    return Err(Problem::GridCells(limits.grid_cells));
                }
                for row in rows {
                    if row.len() != columns.len() {
                        return Err(Problem::RowWidth(
                            "a grid".to_string(),
                            row.len(),
                            columns.len(),
                        ));
                    }
                    links(row)?;
                }
                Ok(0)
            }
            Node::Table { id, columns, rows } => {
                seen.name(id)?;
                let mut names = HashSet::new();
                for column in columns {
                    if !names.insert(column.id.as_str()) {
                        return Err(Problem::DuplicateColumn(id.clone(), column.id.clone()));
                    }
                }
                seen.cells += columns.len() * rows.len();
                if seen.cells > limits.cells {
                    return Err(Problem::Cells(limits.cells));
                }
                let mut ids = HashSet::new();
                for row in rows {
                    if row.cells.len() != columns.len() {
                        return Err(Problem::RowWidth(
                            format!("table {id}"),
                            row.cells.len(),
                            columns.len(),
                        ));
                    }
                    if !ids.insert(row.id.as_str()) {
                        return Err(Problem::DuplicateRow(id.clone(), row.id.clone()));
                    }
                    links(&row.cells)?;
                }
                Ok(1)
            }
        }
    }
}

fn links(cells: &[Cell]) -> Result<(), Problem> {
    for cell in cells {
        if let Some(href) = &cell.href
            && !(href.starts_with("https://") || href.starts_with("http://"))
        {
            return Err(Problem::Href(href.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        Bar, Cell, Column, Limits, Node, PlotKind, Problem, Reply, Row, Segment, Series, Tab, Tone,
        View,
    };

    fn table(id: &str, rows: Vec<Row>) -> Node {
        Node::Table {
            id: id.to_string(),
            columns: vec![
                Column {
                    id: "ticket".into(),
                    title: "Ticket".into(),
                    kind: Default::default(),
                    width: Default::default(),
                },
                Column {
                    id: "verdict".into(),
                    title: "Verdict".into(),
                    kind: Default::default(),
                    width: Default::default(),
                },
            ],
            rows,
        }
    }

    fn row(id: &str) -> Row {
        Row {
            id: id.to_string(),
            cells: vec![Cell::text(id), Cell::text("affected").tone(Tone::Bad)],
        }
    }

    fn view(body: Node) -> View {
        View {
            title: "Report".into(),
            body,
        }
    }

    #[test]
    fn a_reply_reads_back_as_written() {
        let reply = Reply::View(view(Node::Tabs {
            id: "category".into(),
            tabs: vec![Tab {
                label: "All".into(),
                count: Some(1),
                body: table("all", vec![row("T-1")]),
            }],
        }));
        let json = serde_json::to_string(&reply).unwrap();
        assert!(json.contains(r#"{"tabs":{"id":"category""#), "{json}");
        assert_eq!(serde_json::from_str::<Reply>(&json).unwrap(), reply);
        assert_eq!(
            serde_json::from_str::<Reply>(r#"{"error":"no verdicts"}"#).unwrap(),
            Reply::Error("no verdicts".into())
        );
    }

    #[test]
    fn an_unknown_component_or_field_is_refused() {
        let unknown = r#"{"title":"x","body":{"script":{"src":"x.js"}}}"#;
        assert!(serde_json::from_str::<View>(unknown).is_err());
        let extra = r##"{"title":"x","body":{"text":{"text":"hi","color":"#f00"}}}"##;
        assert!(serde_json::from_str::<View>(extra).is_err());
        let tone = r#"{"title":"x","body":{"text":{"text":"hi","tone":"purple"}}}"#;
        assert!(serde_json::from_str::<View>(tone).is_err());
    }

    #[test]
    fn a_tab_shows_one_table_and_a_stack_may_not_show_two() {
        let tabs = Node::Tabs {
            id: "t".into(),
            tabs: vec![
                Tab {
                    label: "a".into(),
                    count: None,
                    body: table("a", vec![row("1")]),
                },
                Tab {
                    label: "b".into(),
                    count: None,
                    body: table("b", vec![row("1")]),
                },
            ],
        };
        assert_eq!(view(tabs.clone()).check(&Limits::DEFAULT), Ok(()));
        let two = Node::Stack {
            children: vec![tabs, table("c", vec![])],
        };
        assert_eq!(view(two).check(&Limits::DEFAULT), Err(Problem::Tables));
    }

    #[test]
    fn rows_must_fit_their_columns_and_be_named_once() {
        let short = table(
            "t",
            vec![Row {
                id: "1".into(),
                cells: vec![Cell::text("only")],
            }],
        );
        assert_eq!(
            view(short).check(&Limits::DEFAULT),
            Err(Problem::RowWidth("table t".into(), 1, 2))
        );
        let twice = table("t", vec![row("1"), row("1")]);
        assert_eq!(
            view(twice).check(&Limits::DEFAULT),
            Err(Problem::DuplicateRow("t".into(), "1".into()))
        );
        let same = Node::Tabs {
            id: "t".into(),
            tabs: vec![Tab {
                label: "a".into(),
                count: None,
                body: table("t", vec![]),
            }],
        };
        assert_eq!(
            view(same).check(&Limits::DEFAULT),
            Err(Problem::DuplicateId("t".into()))
        );
    }

    #[test]
    fn a_link_must_be_http() {
        let bad = table(
            "t",
            vec![Row {
                id: "1".into(),
                cells: vec![Cell::text("x").link("file:///etc/passwd"), Cell::text("y")],
            }],
        );
        assert_eq!(
            view(bad).check(&Limits::DEFAULT),
            Err(Problem::Href("file:///etc/passwd".into()))
        );
    }

    #[test]
    fn bar_values_are_finite_and_not_negative() {
        let bars = |value: f64, count: usize| {
            view(Node::Bars {
                bars: vec![Bar {
                    label: "3.4".into(),
                    segments: vec![
                        Segment {
                            label: "affected".into(),
                            value,
                            tone: Tone::Bad,
                        };
                        count
                    ],
                }],
            })
        };
        assert_eq!(bars(3.0, 2).check(&Limits::DEFAULT), Ok(()));
        assert_eq!(bars(0.0, 1).check(&Limits::DEFAULT), Ok(()));
        for value in [-1.0, f64::INFINITY] {
            assert_eq!(
                bars(value, 1).check(&Limits::DEFAULT),
                Err(Problem::Value("affected".into()))
            );
        }
        assert!(
            serde_json::from_str::<Node>(
                r#"{"bars":{"bars":[{"label":"a","segments":[{"label":"b","value":null}]}]}}"#
            )
            .is_err()
        );
        assert_eq!(
            bars(1.0, 1_001).check(&Limits::DEFAULT),
            Err(Problem::Segments(1_000))
        );
    }

    #[test]
    fn plot_points_are_finite_and_counted() {
        let plot = |points: Vec<[f64; 2]>| {
            view(Node::Plot {
                kind: PlotKind::Scatter,
                x: "score".into(),
                y: "days".into(),
                series: vec![Series {
                    label: "affected".into(),
                    tone: Tone::Bad,
                    points,
                }],
            })
        };
        assert_eq!(
            plot(vec![[7.5, 12.0], [-1.0, 0.0]]).check(&Limits::DEFAULT),
            Ok(())
        );
        assert_eq!(
            plot(vec![[1.0, f64::NAN]]).check(&Limits::DEFAULT),
            Err(Problem::Point("affected".into()))
        );
        assert_eq!(
            plot(vec![[0.0, 0.0]; 20_001]).check(&Limits::DEFAULT),
            Err(Problem::Points(20_000))
        );
        let json = serde_json::to_string(&plot(vec![[1.0, 2.0]])).unwrap();
        assert!(json.contains(r#""kind":"scatter""#), "{json}");
        assert!(json.contains(r#""points":[[1.0,2.0]]"#), "{json}");
        assert_eq!(
            serde_json::from_str::<View>(&json).unwrap(),
            plot(vec![[1.0, 2.0]])
        );
        let pie = r#"{"plot":{"kind":"pie","x":"a","y":"b","series":[]}}"#;
        assert!(serde_json::from_str::<Node>(pie).is_err());
    }

    #[test]
    fn a_view_over_its_limits_is_refused() {
        let limits = Limits {
            nodes: 3,
            depth: 2,
            cells: 2,
            grid_cells: 4,
            segments: 2,
            points: 2,
            commands: 2,
            canvas_side: 10,
            flow_nodes: 2,
            flow_links: 2,
        };
        let deep = Node::Stack {
            children: vec![Node::Stack {
                children: vec![Node::Stats { items: vec![] }],
            }],
        };
        assert_eq!(view(deep).check(&limits), Err(Problem::Depth(2)));
        let wide = Node::Stack {
            children: (0..3).map(|_| Node::Stats { items: vec![] }).collect(),
        };
        assert_eq!(view(wide).check(&limits), Err(Problem::Nodes(3)));
        let big = table("t", vec![row("1"), row("2")]);
        assert_eq!(view(big).check(&limits), Err(Problem::Cells(2)));
        let grid = Node::Grid {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![Cell::text("1"), Cell::text("2")]; 2],
        };
        assert_eq!(view(grid).check(&limits), Err(Problem::GridCells(4)));
        assert_eq!(
            view(Node::Tabs {
                id: "t".into(),
                tabs: vec![]
            })
            .check(&limits),
            Err(Problem::NoTabs("t".into()))
        );
    }
}

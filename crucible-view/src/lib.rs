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

pub use crate::canvas::{
    ARC_STEPS, CANVAS_REACH, CURVE_STEPS, Context2d, Draw, LineCap, NotAStyle, Style, TextAlign,
};
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
    /// Markdown: headings, lists, tables, code, links, and diagrams in `mermaid` fences. A link
    /// is a [`Link`]; one that is not leads nowhere.
    Markdown { text: String },
    /// A Mermaid diagram from its source.
    Mermaid { source: String },
}

/// Where an address leads: the web, or a place in the run the view is drawn for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link<'a> {
    /// `http://` or `https://` and the rest.
    Web(&'a str),
    /// `run:task/<task>`, the task as a [`File`] names it.
    Task(&'a str),
    /// `run:file/<task>/<path>`, held as `<task>/<path>`.
    File(&'a str),
}

impl<'a> Link<'a> {
    pub fn parse(href: &'a str) -> Option<Link<'a>> {
        if href.starts_with("https://") || href.starts_with("http://") {
            return Some(Link::Web(href));
        }
        let place = href.strip_prefix("run:")?;
        if let Some(task) = place.strip_prefix("task/") {
            return (!task.is_empty()).then_some(Link::Task(task));
        }
        let file = place.strip_prefix("file/")?;
        let (task, path) = file.split_once('/')?;
        (!task.is_empty() && !path.is_empty()).then_some(Link::File(file))
    }
}

/// The address of `task` in the run.
pub fn task_link(task: &str) -> String {
    format!("run:task/{task}")
}

/// The address of the captured file `file` in the run.
pub fn file_link(file: &File) -> String {
    format!("run:file/{}/{}", file.task, file.path)
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
    /// From 0 to [`MAGNITUDE`].
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
    /// Each point's x then y, both within [`MAGNITUDE`] of zero.
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
    /// Right-aligned figures, sorted as numbers: a cell's `value`, else its text when that is
    /// a number.
    Number,
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// Unique within its table.
    pub id: String,
    pub cells: Vec<Cell>,
    /// Markdown shown under the table while the row is selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cell {
    pub text: String,
    /// What a table sorts the cell by in place of its text, within [`MAGNITUDE`] of zero: the
    /// seconds behind "3 min", the bytes behind "1.2 MB", a date as a timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    /// The [`Link`] the cell opens.
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

    pub fn value(mut self, value: f64) -> Cell {
        self.value = Some(value);
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

/// The largest value a bar segment, plot point or ribbon may hold, either side of zero.
pub const MAGNITUDE: f64 = 1e15;

/// How much a view may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub nodes: usize,
    pub depth: usize,
    /// Table cells across the view.
    pub cells: usize,
    /// Cells in one grid, its header among them.
    pub grid_cells: usize,
    /// Stats, bars, segments, series, grid cells, table columns, and flow stages and ribbons
    /// across the view.
    pub items: usize,
    /// Plot points, and the points of every canvas path filled or stroked, across the view.
    pub points: usize,
    /// Points in one stretch of a canvas path, from one move or close to the next.
    pub path_points: usize,
    /// Stages and ribbons in one flow.
    pub flow_nodes: usize,
    pub flow_links: usize,
    /// Canvas commands across the view.
    pub commands: usize,
    /// The longest side of a canvas, in points.
    pub canvas_side: u16,
    /// Bytes in one title, label, cell or address.
    pub text: usize,
    /// Bytes of Markdown and Mermaid source across the view's components, and in one row's
    /// detail.
    pub prose: usize,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        nodes: 2_000,
        depth: 8,
        cells: 500_000,
        grid_cells: 600,
        items: 20_000,
        points: 50_000,
        path_points: 1_000,
        flow_nodes: 60,
        flow_links: 400,
        commands: 20_000,
        canvas_side: 4_000,
        text: 20_000,
        prose: 100_000,
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
    #[error(
        "the view holds more than {0} stats, bars, segments, series, grid cells, columns, \
         stages and ribbons"
    )]
    Items(usize),
    #[error("a title, label, cell or address is longer than {0} bytes")]
    Text(usize),
    #[error("the bar segment {0} is not a number from 0 to 1e15")]
    Value(String),
    #[error("a flow holds more than {0} stages")]
    FlowNodes(usize),
    #[error("a flow holds more than {0} ribbons")]
    FlowLinks(usize),
    #[error("a flow has two stages named {0}")]
    FlowDuplicate(String),
    #[error("a flow's ribbon names the stage {0}, which it does not have")]
    FlowUnknown(String),
    #[error("the ribbon from {0} to {1} has a value that is not above 0 and at most 1e15")]
    FlowValue(String, String),
    #[error("a flow's ribbons lead back to a stage they came from")]
    FlowCycle,
    #[error("a canvas is not between 1 and {0} points on each side")]
    CanvasSize(u16),
    #[error("the view's canvases hold more than {0} commands")]
    Commands(usize),
    #[error("canvas command {0} holds a number that is not within 1,000,000 of zero")]
    CanvasNumber(usize),
    #[error("a canvas path runs more than {0} points without a move or a close")]
    PathPoints(usize),
    #[error("the view's plots and canvas paths hold more than {0} points")]
    Points(usize),
    #[error("the series {0} has a point that is not a number within 1e15 of zero")]
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
    #[error("{0} is not an http(s) address or a run link")]
    Href(String),
    #[error("the cell {0} has a value that is not a number within 1e15 of zero")]
    CellValue(String),
    #[error("the view's Markdown and diagrams hold more than {0} bytes")]
    Prose(usize),
    #[error("a row's detail is longer than {0} bytes")]
    Detail(usize),
}

impl View {
    /// Whether the host can draw this view within `limits`.
    pub fn check(&self, limits: &Limits) -> Result<(), Problem> {
        let mut seen = Seen::default();
        fits(&self.title, limits)?;
        let shown = self.body.check(limits, 1, &mut seen)?;
        if shown > 1 {
            return Err(Problem::Tables);
        }
        Ok(())
    }
}

/// What the view holds so far.
#[derive(Default)]
struct Seen {
    nodes: usize,
    cells: usize,
    items: usize,
    points: usize,
    commands: usize,
    prose: usize,
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

    fn items(&mut self, more: usize, limits: &Limits) -> Result<(), Problem> {
        self.items = self.items.saturating_add(more);
        if self.items > limits.items {
            return Err(Problem::Items(limits.items));
        }
        Ok(())
    }

    fn points(&mut self, more: usize, limits: &Limits) -> Result<(), Problem> {
        self.points = self.points.saturating_add(more);
        if self.points > limits.points {
            return Err(Problem::Points(limits.points));
        }
        Ok(())
    }
}

fn fits(text: &str, limits: &Limits) -> Result<(), Problem> {
    if text.len() > limits.text {
        return Err(Problem::Text(limits.text));
    }
    Ok(())
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
                fits(id, limits)?;
                seen.name(id)?;
                if tabs.is_empty() {
                    return Err(Problem::NoTabs(id.clone()));
                }
                let mut shown = 0;
                for tab in tabs {
                    fits(&tab.label, limits)?;
                    shown = shown.max(tab.body.check(limits, depth + 1, seen)?);
                }
                Ok(shown)
            }
            Node::Stats { items } => {
                seen.items(items.len(), limits)?;
                for item in items {
                    fits(&item.label, limits)?;
                    fits(&item.value, limits)?;
                }
                Ok(0)
            }
            Node::Text { text, .. } => fits(text, limits).map(|()| 0),
            Node::Markdown { text: source } | Node::Mermaid { source } => {
                seen.prose = seen.prose.saturating_add(source.len());
                if seen.prose > limits.prose {
                    return Err(Problem::Prose(limits.prose));
                }
                Ok(0)
            }
            Node::Bars { bars } => {
                seen.items(bars.len(), limits)?;
                for bar in bars {
                    fits(&bar.label, limits)?;
                    seen.items(bar.segments.len(), limits)?;
                    for segment in &bar.segments {
                        fits(&segment.label, limits)?;
                        if !(0.0..=MAGNITUDE).contains(&segment.value) {
                            return Err(Problem::Value(segment.label.clone()));
                        }
                    }
                }
                Ok(0)
            }
            Node::Flow { nodes, links } => {
                if nodes.len() > limits.flow_nodes {
                    return Err(Problem::FlowNodes(limits.flow_nodes));
                }
                if links.len() > limits.flow_links {
                    return Err(Problem::FlowLinks(limits.flow_links));
                }
                seen.items(nodes.len() + links.len(), limits)?;
                for node in nodes {
                    fits(&node.id, limits)?;
                    fits(&node.label, limits)?;
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
                seen.commands = seen.commands.saturating_add(commands.len());
                if seen.commands > limits.commands {
                    return Err(Problem::Commands(limits.commands));
                }
                let (mut path, mut stretch) = (0_usize, 0_usize);
                for (at, command) in commands.iter().enumerate() {
                    if !command.in_reach() {
                        return Err(Problem::CanvasNumber(at));
                    }
                    match command {
                        Draw::BeginPath => (path, stretch) = (0, 0),
                        Draw::Fill | Draw::Stroke => seen.points(path, limits)?,
                        Draw::FillRect(_) | Draw::StrokeRect(_) => seen.points(4, limits)?,
                        Draw::Font(text) | Draw::FillText { text, .. } => fits(text, limits)?,
                        Draw::MoveTo(_) => (path, stretch) = (path + 1, 1),
                        Draw::ClosePath | Draw::Rect(_) => {
                            path += command.path_points();
                            stretch = 1;
                        }
                        other => {
                            path += other.path_points();
                            stretch += other.path_points();
                        }
                    }
                    if stretch > limits.path_points {
                        return Err(Problem::PathPoints(limits.path_points));
                    }
                }
                Ok(0)
            }
            Node::Plot { x, y, series, .. } => {
                fits(x, limits)?;
                fits(y, limits)?;
                seen.items(series.len(), limits)?;
                for one in series {
                    fits(&one.label, limits)?;
                    seen.points(one.points.len(), limits)?;
                    let within = |v: &f64| (-MAGNITUDE..=MAGNITUDE).contains(v);
                    if !one.points.iter().flatten().all(within) {
                        return Err(Problem::Point(one.label.clone()));
                    }
                }
                Ok(0)
            }
            Node::Grid { columns, rows } => {
                let cells = columns
                    .len()
                    .max(1)
                    .saturating_mul(rows.len().saturating_add(1));
                if cells > limits.grid_cells {
                    return Err(Problem::GridCells(limits.grid_cells));
                }
                seen.items(cells, limits)?;
                for title in columns {
                    fits(title, limits)?;
                }
                for row in rows {
                    if row.len() != columns.len() {
                        return Err(Problem::RowWidth(
                            "a grid".to_string(),
                            row.len(),
                            columns.len(),
                        ));
                    }
                    cells_fit(row, limits)?;
                }
                Ok(0)
            }
            Node::Table { id, columns, rows } => {
                fits(id, limits)?;
                seen.name(id)?;
                seen.items(columns.len(), limits)?;
                let mut names = HashSet::new();
                for column in columns {
                    fits(&column.id, limits)?;
                    fits(&column.title, limits)?;
                    if !names.insert(column.id.as_str()) {
                        return Err(Problem::DuplicateColumn(id.clone(), column.id.clone()));
                    }
                }
                seen.cells = seen
                    .cells
                    .saturating_add(columns.len().max(1).saturating_mul(rows.len()));
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
                    fits(&row.id, limits)?;
                    if row
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.len() > limits.prose)
                    {
                        return Err(Problem::Detail(limits.prose));
                    }
                    if !ids.insert(row.id.as_str()) {
                        return Err(Problem::DuplicateRow(id.clone(), row.id.clone()));
                    }
                    cells_fit(&row.cells, limits)?;
                }
                Ok(1)
            }
        }
    }
}

fn cells_fit(cells: &[Cell], limits: &Limits) -> Result<(), Problem> {
    for cell in cells {
        fits(&cell.text, limits)?;
        if cell
            .value
            .is_some_and(|value| !(-MAGNITUDE..=MAGNITUDE).contains(&value))
        {
            return Err(Problem::CellValue(cell.text.clone()));
        }
        if let Some(href) = &cell.href {
            fits(href, limits)?;
            if Link::parse(href).is_none() {
                return Err(Problem::Href(href.clone()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        Bar, Cell, Column, File, Limits, Link, Node, PlotKind, Problem, Reply, Row, Segment,
        Series, Stat, Tab, Tone, View, file_link, task_link,
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
            detail: None,
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
                detail: None,
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
                detail: None,
            }],
        );
        assert_eq!(
            view(bad).check(&Limits::DEFAULT),
            Err(Problem::Href("file:///etc/passwd".into()))
        );
    }

    #[test]
    fn a_cell_s_value_is_a_bounded_number_and_is_left_out_when_absent() {
        let valued = |value: f64| {
            table(
                "t",
                vec![Row {
                    id: "1".into(),
                    cells: vec![Cell::text("3 min").value(value), Cell::text("y")],
                    detail: None,
                }],
            )
        };
        assert_eq!(view(valued(180.0)).check(&Limits::DEFAULT), Ok(()));
        assert_eq!(view(valued(-1e15)).check(&Limits::DEFAULT), Ok(()));
        for off in [f64::NAN, f64::INFINITY, 1.1e15] {
            assert_eq!(
                view(valued(off)).check(&Limits::DEFAULT),
                Err(Problem::CellValue("3 min".into()))
            );
        }
        assert_eq!(
            serde_json::to_string(&Cell::text("3 min").value(180.0)).unwrap(),
            r#"{"text":"3 min","value":180.0,"tone":"neutral"}"#
        );
        assert_eq!(
            serde_json::to_string(&Cell::text("x")).unwrap(),
            r#"{"text":"x","tone":"neutral"}"#
        );
        let column: Column =
            serde_json::from_str(r#"{"id":"n","title":"N","kind":"number"}"#).unwrap();
        assert_eq!(column.kind, crate::ColumnKind::Number);
    }

    #[test]
    fn a_link_leads_to_the_web_or_into_the_run() {
        assert_eq!(
            Link::parse("https://example.com/a"),
            Some(Link::Web("https://example.com/a"))
        );
        assert_eq!(
            Link::parse("run:task/triage[go]"),
            Some(Link::Task("triage[go]"))
        );
        assert_eq!(
            Link::parse("run:file/triage-go/evidence/probe.json"),
            Some(Link::File("triage-go/evidence/probe.json"))
        );
        for off in [
            "",
            "run:",
            "run:task/",
            "run:file/",
            "run:file/triage-go",
            "run:file/triage-go/",
            "run:file//x",
            "run:pane/files",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "slack://open",
            "HTTP://example.com",
        ] {
            assert_eq!(Link::parse(off), None, "{off}");
        }
        let file = File {
            path: "verdicts.json".into(),
            task: "triage[go]".into(),
            text: String::new(),
        };
        assert_eq!(
            Link::parse(&file_link(&file)),
            Some(Link::File("triage[go]/verdicts.json"))
        );
        assert_eq!(
            Link::parse(&task_link(&file.task)),
            Some(Link::Task("triage[go]"))
        );
        let linked = table(
            "t",
            vec![Row {
                id: "1".into(),
                cells: vec![
                    Cell::text("x").link(task_link("triage")),
                    Cell::text("y").link(file_link(&file)),
                ],
                detail: None,
            }],
        );
        assert_eq!(view(linked).check(&Limits::DEFAULT), Ok(()));
    }

    #[test]
    fn markdown_and_diagrams_share_a_budget_and_a_row_s_detail_has_its_own() {
        let markdown = |bytes: usize| Node::Markdown {
            text: "x".repeat(bytes),
        };
        let mermaid = |bytes: usize| Node::Mermaid {
            source: "x".repeat(bytes),
        };
        let pair = |a: Node, b: Node| {
            view(Node::Stack {
                children: vec![a, b],
            })
        };
        assert_eq!(pair(markdown(6), mermaid(6)).check(&TIGHT), Ok(()));
        assert_eq!(
            pair(markdown(6), mermaid(7)).check(&TIGHT),
            Err(Problem::Prose(12))
        );
        let detailed = |bytes: usize| {
            view(Node::Stack {
                children: vec![
                    markdown(12),
                    Node::Table {
                        id: "t".into(),
                        columns: vec![],
                        rows: vec![
                            Row {
                                id: "1".into(),
                                cells: vec![],
                                detail: Some("x".repeat(bytes)),
                            },
                            Row {
                                id: "2".into(),
                                cells: vec![],
                                detail: Some("x".repeat(12)),
                            },
                        ],
                    },
                ],
            })
        };
        assert_eq!(detailed(12).check(&TIGHT), Ok(()));
        assert_eq!(detailed(13).check(&TIGHT), Err(Problem::Detail(12)));

        let json = r##"{"markdown":{"text":"# Hi"}}"##;
        assert_eq!(
            serde_json::from_str::<Node>(json).unwrap(),
            Node::Markdown {
                text: "# Hi".into()
            }
        );
        let row = Row {
            id: "1".into(),
            cells: vec![],
            detail: Some("why".into()),
        };
        assert_eq!(
            serde_json::to_string(&row).unwrap(),
            r#"{"id":"1","cells":[],"detail":"why"}"#
        );
        let bare: Row = serde_json::from_str(r#"{"id":"1","cells":[]}"#).unwrap();
        assert_eq!(bare.detail, None);
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
        assert_eq!(bars(1e15, 1).check(&Limits::DEFAULT), Ok(()));
        for value in [-1.0, f64::INFINITY, f64::NAN, 1.1e15, f64::MAX] {
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
        assert_eq!(bars(1.0, 19_999).check(&Limits::DEFAULT), Ok(()));
        assert_eq!(
            bars(1.0, 20_000).check(&Limits::DEFAULT),
            Err(Problem::Items(20_000))
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
        assert_eq!(plot(vec![[-1e15, 1e15]]).check(&Limits::DEFAULT), Ok(()));
        for off in [f64::NAN, f64::INFINITY, -1.1e15, f64::MAX] {
            assert_eq!(
                plot(vec![[1.0, off]]).check(&Limits::DEFAULT),
                Err(Problem::Point("affected".into()))
            );
        }
        assert_eq!(
            plot(vec![[0.0, 0.0]; 50_001]).check(&Limits::DEFAULT),
            Err(Problem::Points(50_000))
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

    const TIGHT: Limits = Limits {
        nodes: 3,
        depth: 2,
        cells: 2,
        grid_cells: 4,
        items: 6,
        points: 6,
        path_points: 4,
        commands: 8,
        canvas_side: 10,
        flow_nodes: 2,
        flow_links: 2,
        text: 8,
        prose: 12,
    };

    #[test]
    fn a_view_over_its_limits_is_refused() {
        let limits = TIGHT;
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

    #[test]
    fn rows_without_columns_still_count() {
        let grid = |rows: usize| {
            view(Node::Grid {
                columns: vec![],
                rows: vec![vec![]; rows],
            })
        };
        assert_eq!(grid(599).check(&Limits::DEFAULT), Ok(()));
        assert_eq!(
            grid(600).check(&Limits::DEFAULT),
            Err(Problem::GridCells(600))
        );
        let table = |rows: usize| {
            view(Node::Table {
                id: "t".into(),
                columns: vec![],
                rows: (0..rows)
                    .map(|i| Row {
                        id: i.to_string(),
                        cells: vec![],
                        detail: None,
                    })
                    .collect(),
            })
        };
        assert_eq!(table(500_000).check(&Limits::DEFAULT), Ok(()));
        assert_eq!(
            table(500_001).check(&Limits::DEFAULT),
            Err(Problem::Cells(500_000))
        );
    }

    #[test]
    fn limits_hold_across_the_whole_view() {
        let stat = || Stat {
            label: "a".into(),
            value: "1".into(),
            tone: Tone::Neutral,
        };
        let stats = |count: usize| Node::Stats {
            items: vec![stat(); count],
        };
        let series = |points: usize| Series {
            label: "s".into(),
            tone: Tone::Neutral,
            points: vec![[0.0, 0.0]; points],
        };
        let plot = |all: Vec<Series>| Node::Plot {
            kind: PlotKind::Line,
            x: "x".into(),
            y: "y".into(),
            series: all,
        };
        let pair = |a: Node, b: Node| {
            view(Node::Stack {
                children: vec![a, b],
            })
        };
        assert_eq!(pair(stats(3), stats(3)).check(&TIGHT), Ok(()));
        assert_eq!(
            pair(stats(3), stats(4)).check(&TIGHT),
            Err(Problem::Items(6))
        );
        assert_eq!(
            view(plot(vec![series(0); 7])).check(&TIGHT),
            Err(Problem::Items(6))
        );
        assert_eq!(
            view(Node::Bars {
                bars: (0..7)
                    .map(|_| Bar {
                        label: "b".into(),
                        segments: vec![],
                    })
                    .collect(),
            })
            .check(&TIGHT),
            Err(Problem::Items(6))
        );
        assert_eq!(
            pair(plot(vec![series(3)]), plot(vec![series(3)])).check(&TIGHT),
            Ok(())
        );
        assert_eq!(
            pair(plot(vec![series(3)]), plot(vec![series(4)])).check(&TIGHT),
            Err(Problem::Points(6))
        );
        let wide = Node::Table {
            id: "t".into(),
            columns: (0..7)
                .map(|i| Column {
                    id: i.to_string(),
                    title: String::new(),
                    kind: Default::default(),
                    width: Default::default(),
                })
                .collect(),
            rows: vec![],
        };
        assert_eq!(view(wide).check(&TIGHT), Err(Problem::Items(6)));
    }

    #[test]
    fn a_long_title_label_cell_or_address_is_refused() {
        let long = "x".repeat(9);
        let over = |body: Node| view(body).check(&TIGHT) == Err(Problem::Text(8));
        assert_eq!(
            View {
                title: long.clone(),
                body: Node::Stats { items: vec![] },
            }
            .check(&TIGHT),
            Err(Problem::Text(8))
        );
        assert!(over(Node::Text {
            text: long.clone(),
            tone: Tone::Neutral,
        }));
        assert!(over(Node::Stats {
            items: vec![Stat {
                label: "a".into(),
                value: long.clone(),
                tone: Tone::Neutral,
            }],
        }));
        assert!(over(Node::Tabs {
            id: "t".into(),
            tabs: vec![Tab {
                label: long.clone(),
                count: None,
                body: Node::Stats { items: vec![] },
            }],
        }));
        assert!(over(Node::Grid {
            columns: vec!["a".into()],
            rows: vec![vec![Cell::text(long.clone())]],
        }));
        assert!(over(Node::Grid {
            columns: vec!["a".into()],
            rows: vec![vec![Cell::text("a").link(format!("https://{long}"))]],
        }));
        assert!(over(Node::Table {
            id: "t".into(),
            columns: vec![],
            rows: vec![Row {
                id: long.clone(),
                cells: vec![],
                detail: None,
            }],
        }));
        assert!(!over(Node::Text {
            text: "x".repeat(8),
            tone: Tone::Neutral,
        }));
    }
}

//! The components a pack's view may use, and the documents a view plugin exchanges with its host.
//!
//! A plugin is a WebAssembly module with no imports. The host calls `crucible_inputs` for the
//! captured file paths it reads, then `crucible_view` with a [`Request`] holding those files, and
//! gets a [`Reply`]. The host draws the [`View`]; the plugin draws nothing.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

mod canvas;
mod flow;
#[cfg(target_arch = "wasm32")]
pub mod guest;

pub use crate::canvas::{
    ARC_STEPS, CANVAS_REACH, CURVE_STEPS, Context2d, Draw, LineCap, NotAStyle, Style, TextAlign,
    TextBaseline,
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

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub files: Vec<File>,
    /// Each [`Control`] of the last view by id, with the reader's value; empty until the reader
    /// changes one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Param>,
}

impl Request {
    /// The value of the select `id`.
    pub fn select(&self, id: &str) -> Option<&str> {
        match self.params.get(id)? {
            Param::Select(value) => Some(value),
            Param::Toggle(_) | Param::Range(_) => None,
        }
    }

    /// Whether the toggle `id` is on.
    pub fn toggle(&self, id: &str) -> Option<bool> {
        match self.params.get(id)? {
            Param::Toggle(on) => Some(*on),
            Param::Select(_) | Param::Range(_) => None,
        }
    }

    /// The value of the range `id`.
    pub fn range(&self, id: &str) -> Option<f64> {
        match self.params.get(id)? {
            Param::Range(value) => Some(*value),
            Param::Select(_) | Param::Toggle(_) => None,
        }
    }
}

/// A control's value, under the name of its kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Param {
    Select(String),
    Toggle(bool),
    Range(f64),
}

/// A choice the reader makes above the view. Changing one runs the plugin again with every
/// control's value in [`Request::params`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    /// One of `options`; `value` is one of theirs.
    Select {
        id: String,
        label: String,
        options: Vec<Choice>,
        value: String,
    },
    Toggle {
        id: String,
        label: String,
        on: bool,
    },
    /// A number from `min` to `max` in steps of `step`.
    Range {
        id: String,
        label: String,
        min: f64,
        max: f64,
        step: f64,
        value: f64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl Control {
    /// A select of `options`, each a value and its label.
    pub fn select(id: &str, label: &str, options: &[(&str, &str)], value: &str) -> Control {
        Control::Select {
            id: id.to_string(),
            label: label.to_string(),
            options: options
                .iter()
                .map(|(value, label)| Choice {
                    value: value.to_string(),
                    label: label.to_string(),
                })
                .collect(),
            value: value.to_string(),
        }
    }

    pub fn toggle(id: &str, label: &str, on: bool) -> Control {
        Control::Toggle {
            id: id.to_string(),
            label: label.to_string(),
            on,
        }
    }

    pub fn range(id: &str, label: &str, [min, max]: [f64; 2], step: f64, value: f64) -> Control {
        Control::Range {
            id: id.to_string(),
            label: label.to_string(),
            min,
            max,
            step,
            value,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Control::Select { id, .. } | Control::Toggle { id, .. } | Control::Range { id, .. } => {
                id
            }
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Control::Select { label, .. }
            | Control::Toggle { label, .. }
            | Control::Range { label, .. } => label,
        }
    }

    /// `asked` when it is a value of this control's kind, a select's among its options and a
    /// range's moved onto its nearest step or its `max`; otherwise the control's own value.
    pub fn coerce(&self, asked: Option<&Param>) -> Param {
        match (self, asked) {
            (Control::Select { options, .. }, Some(Param::Select(asked)))
                if options.iter().any(|choice| &choice.value == asked) =>
            {
                Param::Select(asked.clone())
            }
            (Control::Select { value, .. }, _) => Param::Select(value.clone()),
            (Control::Toggle { .. }, Some(Param::Toggle(on))) => Param::Toggle(*on),
            (Control::Toggle { on, .. }, _) => Param::Toggle(*on),
            (Control::Range { min, max, step, .. }, Some(Param::Range(asked)))
                if asked.is_finite() && *step > 0.0 =>
            {
                let asked = asked.max(*min).min(*max);
                let stepped = (min + ((asked - min) / step).round() * step)
                    .max(*min)
                    .min(*max);
                if max - asked < (asked - stepped).abs() {
                    Param::Range(*max)
                } else {
                    Param::Range(stepped)
                }
            }
            (Control::Range { value, .. }, _) => Param::Range(*value),
        }
    }
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
    /// Drawn in a row above the body.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<Control>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// Children top to bottom.
    Stack { children: Vec<Node> },
    /// Children side by side at equal widths, wrapping onto further lines when narrow. Holds
    /// no table.
    Columns { children: Vec<Node> },
    /// A body under a title the reader folds and unfolds.
    Section {
        id: String,
        title: String,
        /// Whether it starts unfolded.
        #[serde(default = "unfolded", skip_serializing_if = "is_unfolded")]
        open: bool,
        body: Box<Node>,
    },
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
        #[serde(default, skip_serializing_if = "Scale::is_linear")]
        x_scale: Scale,
        #[serde(default, skip_serializing_if = "Scale::is_linear")]
        y_scale: Scale,
        /// The names an x value indexes, from 0; empty for a numeric x axis.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        categories: Vec<String>,
    },
    /// Values counted into bins of equal width, drawn as bars.
    Histogram {
        /// What the values measure.
        x: String,
        values: Vec<f64>,
        /// From 1 to [`MAX_BINS`]; the app picks when not given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bins: Option<u32>,
        #[serde(default)]
        tone: Tone,
    },
    /// A matrix of values, each cell shaded by its place between the least and the greatest.
    Heatmap {
        /// The columns' labels.
        x: Vec<String>,
        /// The rows' labels.
        y: Vec<String>,
        /// A row of values per `y` label, a value per `x` label.
        values: Vec<Vec<f64>>,
        #[serde(default)]
        tone: Tone,
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

fn unfolded() -> bool {
    true
}

fn is_unfolded(open: &bool) -> bool {
    *open
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

/// How a plot's axis places its values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scale {
    #[default]
    Linear,
    /// Powers of ten evenly apart; every value is above zero.
    Log,
    /// Unix seconds, shown as dates and times in UTC within [`TIME_SPAN`].
    Time,
}

impl Scale {
    fn is_linear(&self) -> bool {
        *self == Scale::Linear
    }
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

/// The largest value a bar segment, plot point, histogram value, heatmap cell or ribbon may
/// hold, either side of zero.
pub const MAGNITUDE: f64 = 1e15;

/// The first and last second a time axis may show: the start of the years 1 and 9999, in Unix
/// seconds.
pub const TIME_SPAN: [f64; 2] = [-62_135_596_800.0, 253_370_764_800.0];

/// The most bins a histogram may ask for.
pub const MAX_BINS: u32 = 100;

/// How much a view may hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub nodes: usize,
    pub depth: usize,
    /// Table cells across the view.
    pub cells: usize,
    /// Cells in one grid, its header among them.
    pub grid_cells: usize,
    /// Stats, bars, segments, series, plot categories, grid cells, heatmap cells and labels,
    /// table columns, flow stages and ribbons, and canvas hints across the view.
    pub items: usize,
    /// Plot points, histogram values, and the points of every canvas path filled or stroked,
    /// across the view.
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
    pub controls: usize,
    /// Options in one select.
    pub options: usize,
    /// Steps from one end of a range to the other.
    pub steps: u32,
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
        controls: 12,
        options: 100,
        steps: 1_000,
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
        "the view holds more than {0} stats, bars, segments, series, categories, grid and \
         heatmap cells, heatmap labels, columns, stages, ribbons and canvas hints"
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
    #[error("the view's plots, histograms and canvas paths hold more than {0} points")]
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
    #[error("columns may not hold a table")]
    ColumnsTable,
    #[error("the series {0} has a value that is not above 0 on a log axis")]
    Log(String),
    #[error("the series {0} has a time outside the years 1 to 9998")]
    Time(String),
    #[error("the series {0} has an x that is not the index of one of the plot's categories")]
    Category(String),
    #[error("a plot with categories has a log or time x axis")]
    CategoryScale,
    #[error("a histogram's bin count is not from 1 to {0}")]
    Bins(u32),
    #[error("the histogram {0} has a value that is not a number within 1e15 of zero")]
    HistogramValue(String),
    #[error("a heatmap has {0} rows of values for {1} y labels")]
    HeatmapRows(usize, usize),
    #[error("the heatmap row {0} has {1} values for {2} x labels")]
    HeatmapRow(String, usize, usize),
    #[error("the heatmap row {0} has a value that is not a number within 1e15 of zero")]
    HeatmapValue(String),
    #[error("the view has more than {0} controls")]
    Controls(usize),
    #[error("two controls are named {0}")]
    DuplicateControl(String),
    #[error("the select {0} has more than {1} options")]
    Options(String, usize),
    #[error("the select {0} has two options with the value or label {1}")]
    DuplicateOption(String, String),
    #[error("the control {0} has a value it does not offer")]
    ControlValue(String),
    #[error(
        "the range {0} is not numbers within 1e15 of zero with its min below its max and its \
         step above 0"
    )]
    Range(String),
    #[error("the range {0} has more than {1} steps")]
    Steps(String, u32),
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
        controls_fit(&self.controls, limits)
    }
}

fn controls_fit(controls: &[Control], limits: &Limits) -> Result<(), Problem> {
    if controls.len() > limits.controls {
        return Err(Problem::Controls(limits.controls));
    }
    let mut ids = HashSet::new();
    for control in controls {
        let id = control.id();
        fits(id, limits)?;
        fits(control.label(), limits)?;
        if !ids.insert(id) {
            return Err(Problem::DuplicateControl(id.to_string()));
        }
        match control {
            Control::Select { options, value, .. } => {
                if options.len() > limits.options {
                    return Err(Problem::Options(id.to_string(), limits.options));
                }
                let (mut values, mut labels) = (HashSet::new(), HashSet::new());
                for choice in options {
                    fits(&choice.value, limits)?;
                    fits(&choice.label, limits)?;
                    if !values.insert(choice.value.as_str()) {
                        return Err(Problem::DuplicateOption(
                            id.to_string(),
                            choice.value.clone(),
                        ));
                    }
                    if !labels.insert(choice.label.as_str()) {
                        return Err(Problem::DuplicateOption(
                            id.to_string(),
                            choice.label.clone(),
                        ));
                    }
                }
                if !values.contains(value.as_str()) {
                    return Err(Problem::ControlValue(id.to_string()));
                }
            }
            Control::Toggle { .. } => {}
            Control::Range {
                min,
                max,
                step,
                value,
                ..
            } => {
                if ![min, max, step, value].into_iter().all(within) || min >= max || *step <= 0.0 {
                    return Err(Problem::Range(id.to_string()));
                }
                if (max - min) / step > f64::from(limits.steps) {
                    return Err(Problem::Steps(id.to_string(), limits.steps));
                }
                if !(*min..=*max).contains(value) {
                    return Err(Problem::ControlValue(id.to_string()));
                }
            }
        }
    }
    Ok(())
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
    tables: usize,
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
            Node::Columns { children } => {
                let tables = seen.tables;
                for child in children {
                    child.check(limits, depth + 1, seen)?;
                }
                if seen.tables > tables {
                    return Err(Problem::ColumnsTable);
                }
                Ok(0)
            }
            Node::Section {
                id, title, body, ..
            } => {
                fits(id, limits)?;
                fits(title, limits)?;
                seen.name(id)?;
                body.check(limits, depth + 1, seen)
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
                        Draw::Hint { text, .. } => {
                            fits(text, limits)?;
                            seen.items(1, limits)?;
                        }
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
            Node::Plot {
                x,
                y,
                series,
                x_scale,
                y_scale,
                categories,
                ..
            } => {
                fits(x, limits)?;
                fits(y, limits)?;
                seen.items(series.len(), limits)?;
                seen.items(categories.len(), limits)?;
                for name in categories {
                    fits(name, limits)?;
                }
                if !categories.is_empty() && *x_scale != Scale::Linear {
                    return Err(Problem::CategoryScale);
                }
                for one in series {
                    fits(&one.label, limits)?;
                    seen.points(one.points.len(), limits)?;
                    if !one.points.iter().flatten().all(within) {
                        return Err(Problem::Point(one.label.clone()));
                    }
                    for [px, py] in &one.points {
                        on_scale(*x_scale, *px, &one.label)?;
                        on_scale(*y_scale, *py, &one.label)?;
                        let indexes = categories.is_empty()
                            || (px.fract() == 0.0 && *px >= 0.0 && *px < categories.len() as f64);
                        if !indexes {
                            return Err(Problem::Category(one.label.clone()));
                        }
                    }
                }
                Ok(0)
            }
            Node::Histogram {
                x, values, bins, ..
            } => {
                fits(x, limits)?;
                seen.points(values.len(), limits)?;
                if bins.is_some_and(|bins| !(1..=MAX_BINS).contains(&bins)) {
                    return Err(Problem::Bins(MAX_BINS));
                }
                if !values.iter().all(within) {
                    return Err(Problem::HistogramValue(x.clone()));
                }
                Ok(0)
            }
            Node::Heatmap { x, y, values, .. } => {
                let cells = x.len().saturating_mul(y.len());
                seen.items(
                    cells.saturating_add(x.len()).saturating_add(y.len()),
                    limits,
                )?;
                for label in x.iter().chain(y) {
                    fits(label, limits)?;
                }
                if values.len() != y.len() {
                    return Err(Problem::HeatmapRows(values.len(), y.len()));
                }
                for (row, label) in values.iter().zip(y) {
                    if row.len() != x.len() {
                        return Err(Problem::HeatmapRow(label.clone(), row.len(), x.len()));
                    }
                    if !row.iter().all(within) {
                        return Err(Problem::HeatmapValue(label.clone()));
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
                seen.tables += 1;
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

fn within(value: &f64) -> bool {
    (-MAGNITUDE..=MAGNITUDE).contains(value)
}

/// Whether `value` can be placed on an axis of `scale`.
fn on_scale(scale: Scale, value: f64, series: &str) -> Result<(), Problem> {
    match scale {
        Scale::Linear => Ok(()),
        Scale::Log if value > 0.0 => Ok(()),
        Scale::Log => Err(Problem::Log(series.to_string())),
        Scale::Time if (TIME_SPAN[0]..=TIME_SPAN[1]).contains(&value) => Ok(()),
        Scale::Time => Err(Problem::Time(series.to_string())),
    }
}

fn cells_fit(cells: &[Cell], limits: &Limits) -> Result<(), Problem> {
    for cell in cells {
        fits(&cell.text, limits)?;
        if cell.value.is_some_and(|value| !within(&value)) {
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
        Bar, Cell, Column, File, Limits, Link, MAX_BINS, Node, PlotKind, Problem, Reply, Row,
        Scale, Segment, Series, Stat, TIME_SPAN, Tab, Tone, View, file_link, task_link,
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
            controls: vec![],
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
                x_scale: Scale::Linear,
                y_scale: Scale::Linear,
                categories: vec![],
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

    fn scaled(x_scale: Scale, y_scale: Scale, categories: &[&str], points: Vec<[f64; 2]>) -> View {
        view(Node::Plot {
            kind: PlotKind::Line,
            x: "x".into(),
            y: "y".into(),
            series: vec![Series {
                label: "s".into(),
                tone: Tone::Neutral,
                points,
            }],
            x_scale,
            y_scale,
            categories: categories.iter().map(|name| name.to_string()).collect(),
        })
    }

    #[test]
    fn a_plot_s_axes_read_back_as_written_and_default_to_linear() {
        let bare: Node =
            serde_json::from_str(r#"{"plot":{"kind":"line","x":"a","y":"b","series":[]}}"#)
                .unwrap();
        let Node::Plot {
            x_scale,
            y_scale,
            categories,
            ..
        } = &bare
        else {
            panic!("a plot");
        };
        assert_eq!((*x_scale, *y_scale), (Scale::Linear, Scale::Linear));
        assert!(categories.is_empty());
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"plot":{"kind":"line","x":"a","y":"b","series":[]}}"#
        );
        let set = scaled(Scale::Time, Scale::Log, &[], vec![[0.0, 1.0]]);
        let json = serde_json::to_string(&set).unwrap();
        assert!(
            json.contains(r#""x_scale":"time","y_scale":"log""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<View>(&json).unwrap(), set);
        let named = scaled(Scale::Linear, Scale::Linear, &["3.4", "3.5"], vec![]);
        let json = serde_json::to_string(&named).unwrap();
        assert!(json.contains(r#""categories":["3.4","3.5"]"#), "{json}");
        assert_eq!(serde_json::from_str::<View>(&json).unwrap(), named);
        for off in [
            r#"{"plot":{"kind":"line","x":"a","y":"b","series":[],"x_scale":"sqrt"}}"#,
            r#"{"plot":{"kind":"line","x":"a","y":"b","series":[],"scale":"log"}}"#,
            r#"{"plot":{"kind":"line","x":"a","y":"b","series":[],"categories":"a"}}"#,
        ] {
            assert!(serde_json::from_str::<Node>(off).is_err(), "{off}");
        }
    }

    #[test]
    fn a_log_axis_holds_only_values_above_zero() {
        let on_x = |x: f64| scaled(Scale::Log, Scale::Linear, &[], vec![[x, -5.0]]);
        let on_y = |y: f64| scaled(Scale::Linear, Scale::Log, &[], vec![[-5.0, y]]);
        for fine in [5e-324, 1e-300, 1.0, 1e15] {
            assert_eq!(on_x(fine).check(&Limits::DEFAULT), Ok(()));
            assert_eq!(on_y(fine).check(&Limits::DEFAULT), Ok(()));
        }
        for off in [0.0, -0.0, -1.0, -1e15] {
            assert_eq!(
                on_x(off).check(&Limits::DEFAULT),
                Err(Problem::Log("s".into()))
            );
            assert_eq!(
                on_y(off).check(&Limits::DEFAULT),
                Err(Problem::Log("s".into()))
            );
        }
        for off in [f64::NAN, f64::INFINITY, 1.1e15] {
            assert_eq!(
                on_y(off).check(&Limits::DEFAULT),
                Err(Problem::Point("s".into()))
            );
        }
    }

    #[test]
    fn a_time_axis_holds_the_years_1_to_9998() {
        let at = |t: f64| scaled(Scale::Time, Scale::Linear, &[], vec![[t, 1.0]]);
        for fine in [TIME_SPAN[0], 0.0, 1_763_251_200.5, TIME_SPAN[1]] {
            assert_eq!(at(fine).check(&Limits::DEFAULT), Ok(()), "{fine}");
        }
        for off in [TIME_SPAN[0] - 1.0, TIME_SPAN[1] + 1.0, -1e15, 1e15] {
            assert_eq!(
                at(off).check(&Limits::DEFAULT),
                Err(Problem::Time("s".into())),
                "{off}"
            );
        }
        let on_y = scaled(Scale::Linear, Scale::Time, &[], vec![[1e15, 1e15]]);
        assert_eq!(on_y.check(&Limits::DEFAULT), Err(Problem::Time("s".into())));
    }

    #[test]
    fn a_categorical_x_is_an_index_into_the_names() {
        let names = ["3.3", "3.4", "3.5"];
        let at = |x: f64| scaled(Scale::Linear, Scale::Linear, &names, vec![[x, 1.0]]);
        for fine in [0.0, 1.0, 2.0, -0.0] {
            assert_eq!(at(fine).check(&Limits::DEFAULT), Ok(()), "{fine}");
        }
        for off in [3.0, -1.0, 0.5, 2.0000001, 1e15] {
            assert_eq!(
                at(off).check(&Limits::DEFAULT),
                Err(Problem::Category("s".into())),
                "{off}"
            );
        }
        assert_eq!(
            scaled(Scale::Linear, Scale::Linear, &[], vec![[0.5, 1.0]]).check(&Limits::DEFAULT),
            Ok(()),
            "no categories, any x"
        );
        for scale in [Scale::Log, Scale::Time] {
            assert_eq!(
                scaled(scale, Scale::Linear, &names, vec![]).check(&Limits::DEFAULT),
                Err(Problem::CategoryScale)
            );
        }
        assert_eq!(
            Problem::CategoryScale.to_string(),
            "a plot with categories has a log or time x axis"
        );
        assert_eq!(
            scaled(Scale::Linear, Scale::Log, &names, vec![[1.0, 2.0]]).check(&Limits::DEFAULT),
            Ok(())
        );
        let many: Vec<&str> = vec!["c"; 6];
        assert_eq!(
            scaled(Scale::Linear, Scale::Linear, &many, vec![]).check(&TIGHT),
            Err(Problem::Items(6)),
            "the series and its categories are items"
        );
        assert_eq!(
            scaled(Scale::Linear, Scale::Linear, &many[..5], vec![]).check(&TIGHT),
            Ok(())
        );
        assert_eq!(
            scaled(Scale::Linear, Scale::Linear, &["123456789"], vec![]).check(&TIGHT),
            Err(Problem::Text(8))
        );
    }

    fn histogram(values: Vec<f64>, bins: Option<u32>) -> View {
        view(Node::Histogram {
            x: "days".into(),
            values,
            bins,
            tone: Tone::Neutral,
        })
    }

    #[test]
    fn a_histogram_s_values_are_bounded_and_counted_as_points() {
        assert_eq!(histogram(vec![], None).check(&Limits::DEFAULT), Ok(()));
        assert_eq!(
            histogram(vec![-1e15, 0.0, 1e15], Some(1)).check(&Limits::DEFAULT),
            Ok(())
        );
        assert_eq!(
            histogram(vec![1.0], Some(MAX_BINS)).check(&Limits::DEFAULT),
            Ok(())
        );
        for bins in [0, MAX_BINS + 1, u32::MAX] {
            assert_eq!(
                histogram(vec![1.0], Some(bins)).check(&Limits::DEFAULT),
                Err(Problem::Bins(100))
            );
        }
        for off in [f64::NAN, f64::NEG_INFINITY, 1.1e15] {
            assert_eq!(
                histogram(vec![1.0, off], None).check(&Limits::DEFAULT),
                Err(Problem::HistogramValue("days".into()))
            );
        }
        assert_eq!(histogram(vec![0.0; 6], None).check(&TIGHT), Ok(()));
        assert_eq!(
            histogram(vec![0.0; 7], None).check(&TIGHT),
            Err(Problem::Points(6))
        );
        let shared = view(Node::Stack {
            children: vec![
                histogram(vec![0.0; 3], None).body,
                scaled(Scale::Linear, Scale::Linear, &[], vec![[0.0, 0.0]; 4]).body,
            ],
        });
        assert_eq!(shared.check(&TIGHT), Err(Problem::Points(6)));
        let json = serde_json::to_string(&histogram(vec![2.0], None)).unwrap();
        assert!(
            json.contains(r#"{"histogram":{"x":"days","values":[2.0],"tone":"neutral"}}"#),
            "{json}"
        );
        let read: Node =
            serde_json::from_str(r#"{"histogram":{"x":"d","values":[1,2.5],"bins":20}}"#).unwrap();
        assert_eq!(
            read,
            Node::Histogram {
                x: "d".into(),
                values: vec![1.0, 2.5],
                bins: Some(20),
                tone: Tone::Neutral,
            }
        );
        for off in [
            r#"{"histogram":{"x":"d","values":[1],"bins":2.5}}"#,
            r#"{"histogram":{"x":"d","values":[1],"bins":-1}}"#,
            r#"{"histogram":{"x":"d","values":[null]}}"#,
            r#"{"histogram":{"values":[1]}}"#,
        ] {
            assert!(serde_json::from_str::<Node>(off).is_err(), "{off}");
        }
    }

    fn heatmap(x: &[&str], y: &[&str], values: Vec<Vec<f64>>) -> View {
        view(Node::Heatmap {
            x: x.iter().map(|label| label.to_string()).collect(),
            y: y.iter().map(|label| label.to_string()).collect(),
            values,
            tone: Tone::Neutral,
        })
    }

    #[test]
    fn a_heatmap_is_a_full_matrix_of_bounded_values() {
        assert_eq!(
            heatmap(&["a", "b"], &["r"], vec![vec![1.0, -1e15]]).check(&Limits::DEFAULT),
            Ok(())
        );
        assert_eq!(heatmap(&[], &[], vec![]).check(&Limits::DEFAULT), Ok(()));
        assert_eq!(
            heatmap(&[], &["r"], vec![vec![]]).check(&Limits::DEFAULT),
            Ok(())
        );
        assert_eq!(
            heatmap(&["a"], &["r", "s"], vec![vec![1.0]]).check(&Limits::DEFAULT),
            Err(Problem::HeatmapRows(1, 2))
        );
        assert_eq!(
            heatmap(&["a"], &[], vec![vec![1.0]]).check(&Limits::DEFAULT),
            Err(Problem::HeatmapRows(1, 0))
        );
        assert_eq!(
            heatmap(&["a", "b"], &["r", "s"], vec![vec![1.0, 2.0], vec![3.0]])
                .check(&Limits::DEFAULT),
            Err(Problem::HeatmapRow("s".into(), 1, 2))
        );
        assert_eq!(
            heatmap(&["a"], &["r"], vec![vec![1.0, 2.0]]).check(&Limits::DEFAULT),
            Err(Problem::HeatmapRow("r".into(), 2, 1))
        );
        for off in [f64::NAN, f64::INFINITY, -1.1e15] {
            assert_eq!(
                heatmap(&["a"], &["r"], vec![vec![off]]).check(&Limits::DEFAULT),
                Err(Problem::HeatmapValue("r".into()))
            );
        }
        assert_eq!(
            heatmap(&["a"], &["r", "s"], vec![vec![1.0], vec![2.0]]).check(&TIGHT),
            Ok(()),
            "two cells and three labels"
        );
        assert_eq!(
            heatmap(&["a", "b"], &["r", "s"], vec![vec![1.0; 2]; 2]).check(&TIGHT),
            Err(Problem::Items(6)),
            "four cells and four labels"
        );
        assert_eq!(
            heatmap(&["123456789"], &["r"], vec![vec![1.0]]).check(&TIGHT),
            Err(Problem::Text(8))
        );
        assert_eq!(
            heatmap(&["a"], &["123456789"], vec![vec![1.0]]).check(&TIGHT),
            Err(Problem::Text(8))
        );
        let huge = Node::Heatmap {
            x: vec![String::new(); 200],
            y: vec![String::new(); 200],
            values: vec![],
            tone: Tone::Neutral,
        };
        assert_eq!(
            view(huge).check(&Limits::DEFAULT),
            Err(Problem::Items(20_000)),
            "counted before the rows are read"
        );
        let json = serde_json::to_string(&heatmap(&["a"], &["r"], vec![vec![1.0]])).unwrap();
        assert_eq!(
            json,
            r#"{"title":"Report","body":{"heatmap":{"x":["a"],"y":["r"],"values":[[1.0]],"tone":"neutral"}}}"#
        );
        assert!(
            serde_json::from_str::<Node>(r#"{"heatmap":{"x":[],"y":[],"values":[],"z":1}}"#)
                .is_err()
        );
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
        controls: 2,
        options: 3,
        steps: 10,
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
            x_scale: Scale::Linear,
            y_scale: Scale::Linear,
            categories: vec![],
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

    fn section(id: &str, open: bool, body: Node) -> Node {
        Node::Section {
            id: id.into(),
            title: "More".into(),
            open,
            body: Box::new(body),
        }
    }

    fn columns(children: Vec<Node>) -> Node {
        Node::Columns { children }
    }

    fn stats() -> Node {
        Node::Stats { items: vec![] }
    }

    #[test]
    fn columns_read_back_as_written_and_may_be_empty() {
        let json = r#"{"columns":{"children":[{"text":{"text":"a","tone":"neutral"}},{"stats":{"items":[]}}]}}"#;
        let node: Node = serde_json::from_str(json).unwrap();
        assert_eq!(
            node,
            columns(vec![
                Node::Text {
                    text: "a".into(),
                    tone: Tone::Neutral,
                },
                stats(),
            ])
        );
        assert_eq!(serde_json::to_string(&node).unwrap(), json);
        assert_eq!(view(columns(vec![])).check(&Limits::DEFAULT), Ok(()));
        assert!(serde_json::from_str::<Node>(r#"{"columns":{"children":[],"gap":4}}"#).is_err());
        assert!(serde_json::from_str::<Node>(r#"{"columns":{}}"#).is_err());
    }

    #[test]
    fn columns_hold_no_table_however_deep() {
        assert_eq!(
            view(columns(vec![stats(), table("t", vec![])])).check(&Limits::DEFAULT),
            Err(Problem::ColumnsTable)
        );
        let hidden = Node::Tabs {
            id: "tabs".into(),
            tabs: vec![
                Tab {
                    label: "a".into(),
                    count: None,
                    body: stats(),
                },
                Tab {
                    label: "b".into(),
                    count: None,
                    body: section("s", false, table("t", vec![row("1")])),
                },
            ],
        };
        assert_eq!(
            view(columns(vec![Node::Stack {
                children: vec![hidden.clone()],
            }]))
            .check(&Limits::DEFAULT),
            Err(Problem::ColumnsTable)
        );
        assert_eq!(view(hidden.clone()).check(&Limits::DEFAULT), Ok(()));
        let beside = Node::Stack {
            children: vec![columns(vec![stats(), stats()]), table("t", vec![row("1")])],
        };
        assert_eq!(view(beside).check(&Limits::DEFAULT), Ok(()));
    }

    #[test]
    fn columns_count_toward_nodes_and_depth() {
        let limits = TIGHT;
        assert_eq!(view(columns(vec![stats(), stats()])).check(&limits), Ok(()));
        assert_eq!(
            view(columns(vec![stats(), stats(), stats()])).check(&limits),
            Err(Problem::Nodes(3))
        );
        assert_eq!(
            view(columns(vec![columns(vec![stats()])])).check(&limits),
            Err(Problem::Depth(2))
        );
    }

    #[test]
    fn a_section_starts_open_unless_it_says_otherwise() {
        let bare: Node = serde_json::from_str(
            r#"{"section":{"id":"s","title":"More","body":{"stats":{"items":[]}}}}"#,
        )
        .unwrap();
        assert_eq!(bare, section("s", true, stats()));
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"section":{"id":"s","title":"More","body":{"stats":{"items":[]}}}}"#
        );
        let closed = section("s", false, stats());
        let json = serde_json::to_string(&closed).unwrap();
        assert_eq!(
            json,
            r#"{"section":{"id":"s","title":"More","open":false,"body":{"stats":{"items":[]}}}}"#
        );
        assert_eq!(serde_json::from_str::<Node>(&json).unwrap(), closed);
        for off in [
            r#"{"section":{"id":"s","title":"More"}}"#,
            r#"{"section":{"title":"More","body":{"stats":{"items":[]}}}}"#,
            r#"{"section":{"id":"s","title":"More","open":"yes","body":{"stats":{"items":[]}}}}"#,
            r#"{"section":{"id":"s","title":"More","folded":true,"body":{"stats":{"items":[]}}}}"#,
        ] {
            assert!(serde_json::from_str::<Node>(off).is_err(), "{off}");
        }
    }

    #[test]
    fn a_section_is_named_once_and_its_title_and_id_are_bounded() {
        let pair = |a: Node, b: Node| {
            view(Node::Stack {
                children: vec![a, b],
            })
        };
        assert_eq!(
            pair(section("s", true, stats()), section("s", false, stats())).check(&Limits::DEFAULT),
            Err(Problem::DuplicateId("s".into()))
        );
        assert_eq!(
            view(section("t", true, table("t", vec![]))).check(&Limits::DEFAULT),
            Err(Problem::DuplicateId("t".into()))
        );
        let tabbed = Node::Tabs {
            id: "s".into(),
            tabs: vec![Tab {
                label: "a".into(),
                count: None,
                body: stats(),
            }],
        };
        assert_eq!(
            pair(tabbed, section("s", true, stats())).check(&Limits::DEFAULT),
            Err(Problem::DuplicateId("s".into()))
        );
        assert_eq!(
            view(section(&"x".repeat(9), true, stats())).check(&TIGHT),
            Err(Problem::Text(8))
        );
        let titled = Node::Section {
            id: "s".into(),
            title: "x".repeat(9),
            open: true,
            body: Box::new(stats()),
        };
        assert_eq!(view(titled).check(&TIGHT), Err(Problem::Text(8)));
        assert_eq!(
            view(section(&"x".repeat(8), true, stats())).check(&TIGHT),
            Ok(())
        );
        assert_eq!(
            view(section("a", true, section("b", true, stats()))).check(&TIGHT),
            Err(Problem::Depth(2))
        );
    }

    #[test]
    fn a_folded_section_s_table_counts_as_shown() {
        for open in [true, false] {
            assert_eq!(
                view(section("s", open, table("t", vec![row("1")]))).check(&Limits::DEFAULT),
                Ok(())
            );
            let two = Node::Stack {
                children: vec![section("s", open, table("a", vec![])), table("b", vec![])],
            };
            assert_eq!(view(two).check(&Limits::DEFAULT), Err(Problem::Tables));
            let folded_pair = Node::Stack {
                children: vec![
                    section("s", open, table("a", vec![])),
                    section("u", !open, table("b", vec![])),
                ],
            };
            assert_eq!(
                view(folded_pair).check(&Limits::DEFAULT),
                Err(Problem::Tables)
            );
        }
    }

    #[test]
    fn a_long_title_label_cell_or_address_is_refused() {
        let long = "x".repeat(9);
        let over = |body: Node| view(body).check(&TIGHT) == Err(Problem::Text(8));
        assert_eq!(
            View {
                title: long.clone(),
                body: Node::Stats { items: vec![] },
                controls: vec![],
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

    fn controlled(controls: Vec<crate::Control>) -> View {
        View {
            controls,
            ..view(Node::Stats { items: vec![] })
        }
    }

    #[test]
    fn controls_read_back_as_written_and_are_left_out_when_none() {
        use crate::{Choice, Control};
        let with = controlled(vec![
            Control::select("lane", "Lane", &[("all", "All"), ("go", "Go")], "all"),
            Control::toggle("open", "Open only", true),
            Control::range("cvss", "Min CVSS", [0.0, 10.0], 0.5, 7.0),
        ]);
        let json = serde_json::to_string(&with).unwrap();
        assert!(
            json.ends_with(
                r#""controls":[{"select":{"id":"lane","label":"Lane","options":[{"value":"all","label":"All"},{"value":"go","label":"Go"}],"value":"all"}},{"toggle":{"id":"open","label":"Open only","on":true}},{"range":{"id":"cvss","label":"Min CVSS","min":0.0,"max":10.0,"step":0.5,"value":7.0}}]}"#
            ),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<View>(&json).unwrap(), with);
        assert_eq!(with.check(&Limits::DEFAULT), Ok(()));
        let none = controlled(vec![]);
        let json = serde_json::to_string(&none).unwrap();
        assert!(!json.contains("controls"), "{json}");
        assert_eq!(serde_json::from_str::<View>(&json).unwrap(), none);
        for off in [
            r#"{"slider":{"id":"a","label":"A"}}"#,
            r#"{"toggle":{"id":"a","label":"A","on":true,"hint":"x"}}"#,
            r#"{"toggle":{"id":"a","label":"A"}}"#,
            r#"{"select":{"id":"a","label":"A","options":[{"value":"x","label":"X","tone":"bad"}],"value":"x"}}"#,
        ] {
            assert!(serde_json::from_str::<Control>(off).is_err(), "{off}");
        }
        let choice: Choice = serde_json::from_str(r#"{"value":"go","label":"Go"}"#).unwrap();
        assert_eq!(choice.label, "Go");
    }

    #[test]
    fn controls_are_few_and_named_once_with_bounded_labels() {
        use crate::Control;
        let toggle = |id: &str| Control::toggle(id, "On", false);
        assert_eq!(
            controlled(vec![toggle("a"), toggle("b")]).check(&TIGHT),
            Ok(())
        );
        assert_eq!(
            controlled(vec![toggle("a"), toggle("b"), toggle("c")]).check(&TIGHT),
            Err(Problem::Controls(2))
        );
        assert_eq!(
            controlled(vec![toggle("a"), toggle("a")]).check(&TIGHT),
            Err(Problem::DuplicateControl("a".into()))
        );
        let tabs = Node::Tabs {
            id: "a".into(),
            tabs: vec![Tab {
                label: "x".into(),
                count: None,
                body: Node::Stats { items: vec![] },
            }],
        };
        assert_eq!(
            View {
                controls: vec![toggle("a")],
                ..view(tabs)
            }
            .check(&Limits::DEFAULT),
            Ok(()),
            "controls and components are named apart"
        );
        assert_eq!(
            controlled(vec![toggle(&"x".repeat(9))]).check(&TIGHT),
            Err(Problem::Text(8))
        );
        assert_eq!(
            controlled(vec![Control::toggle("a", &"x".repeat(9), true)]).check(&TIGHT),
            Err(Problem::Text(8))
        );
        assert_eq!(
            controlled(vec![Control::toggle("", "", true)]).check(&TIGHT),
            Ok(())
        );
    }

    #[test]
    fn a_select_s_value_is_one_of_a_bounded_set_of_distinct_options() {
        use crate::Control;
        let select = |options: &[(&str, &str)], value: &str| {
            controlled(vec![Control::select("s", "S", options, value)]).check(&TIGHT)
        };
        assert_eq!(select(&[("a", "A"), ("b", "B"), ("c", "C")], "c"), Ok(()));
        assert_eq!(
            select(&[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D")], "a"),
            Err(Problem::Options("s".into(), 3))
        );
        assert_eq!(
            select(&[("a", "A"), ("b", "B")], "z"),
            Err(Problem::ControlValue("s".into()))
        );
        assert_eq!(select(&[], ""), Err(Problem::ControlValue("s".into())));
        assert_eq!(
            select(&[("a", "A"), ("a", "B")], "a"),
            Err(Problem::DuplicateOption("s".into(), "a".into()))
        );
        assert_eq!(
            select(&[("a", "A"), ("b", "A")], "a"),
            Err(Problem::DuplicateOption("s".into(), "A".into()))
        );
        assert_eq!(select(&[("a", &"x".repeat(9))], "a"), Err(Problem::Text(8)));
        assert_eq!(select(&[(&"x".repeat(9), "A")], "a"), Err(Problem::Text(8)));
    }

    #[test]
    fn a_range_is_finite_ordered_and_steps_a_bounded_number_of_times() {
        use crate::Control;
        let range = |min: f64, max: f64, step: f64, value: f64| {
            controlled(vec![Control::range("r", "R", [min, max], step, value)]).check(&TIGHT)
        };
        let off = Err(Problem::Range("r".into()));
        assert_eq!(range(0.0, 10.0, 1.0, 0.0), Ok(()));
        assert_eq!(range(0.0, 10.0, 1.0, 10.0), Ok(()));
        assert_eq!(range(-1e15, 1e15, 2e14, 3.3), Ok(()));
        assert_eq!(
            range(0.0, 10.0, 0.999, 1.0),
            Err(Problem::Steps("r".into(), 10))
        );
        assert_eq!(
            range(0.0, 1.0, 1e-300, 1.0),
            Err(Problem::Steps("r".into(), 10))
        );
        assert_eq!(
            range(-1e15, 1e15, f64::MIN_POSITIVE, 0.0),
            Err(Problem::Steps("r".into(), 10))
        );
        assert_eq!(
            range(0.0, 10.0, 1.0, 10.5),
            Err(Problem::ControlValue("r".into()))
        );
        assert_eq!(
            range(0.0, 10.0, 1.0, -0.5),
            Err(Problem::ControlValue("r".into()))
        );
        for (min, max, step, value) in [
            (5.0, 5.0, 1.0, 5.0),
            (6.0, 5.0, 1.0, 5.0),
            (0.0, 10.0, 0.0, 1.0),
            (0.0, 10.0, -1.0, 1.0),
            (f64::NAN, 10.0, 1.0, 1.0),
            (0.0, f64::NAN, 1.0, 1.0),
            (0.0, 10.0, f64::NAN, 1.0),
            (0.0, 10.0, 1.0, f64::NAN),
            (f64::NEG_INFINITY, 10.0, 1.0, 1.0),
            (0.0, f64::INFINITY, 1.0, 1.0),
            (0.0, 10.0, f64::INFINITY, 1.0),
            (0.0, 2e15, 1e15, 1.0),
            (-2e15, 0.0, 1e15, -1.0),
        ] {
            assert_eq!(
                range(min, max, step, value),
                off,
                "{min} {max} {step} {value}"
            );
        }
    }

    #[test]
    fn a_request_s_params_are_left_out_when_none_and_named_by_kind() {
        use crate::{Param, Request};
        let bare = Request {
            files: vec![],
            params: Default::default(),
        };
        assert_eq!(serde_json::to_string(&bare).unwrap(), r#"{"files":[]}"#);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"files":[]}"#).unwrap(),
            bare
        );
        let asked = Request {
            files: vec![],
            params: [
                ("lane".to_string(), Param::Select("go".into())),
                ("open".to_string(), Param::Toggle(true)),
                ("cvss".to_string(), Param::Range(7.5)),
            ]
            .into(),
        };
        let json = serde_json::to_string(&asked).unwrap();
        assert_eq!(
            json,
            r#"{"files":[],"params":{"cvss":{"range":7.5},"lane":{"select":"go"},"open":{"toggle":true}}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), asked);
        assert_eq!(asked.select("lane"), Some("go"));
        assert_eq!(asked.toggle("open"), Some(true));
        assert_eq!(asked.range("cvss"), Some(7.5));
        assert_eq!(asked.select("open"), None);
        assert_eq!(asked.toggle("cvss"), None);
        assert_eq!(asked.range("lane"), None);
        assert_eq!(asked.select("missing"), None);
        assert!(serde_json::from_str::<Request>(r#"{"files":[],"params":{"a":7}}"#).is_err());
        assert!(serde_json::from_str::<Request>(r#"{"files":[],"extra":1}"#).is_err());
    }

    #[test]
    fn a_control_takes_only_a_value_it_offers() {
        use crate::{Control, Param};
        let select = Control::select("s", "S", &[("a", "A"), ("b", "B")], "a");
        let picked = |control: &Control, asked: Param| control.coerce(Some(&asked));
        assert_eq!(
            picked(&select, Param::Select("b".into())),
            Param::Select("b".into())
        );
        assert_eq!(
            picked(&select, Param::Select("z".into())),
            Param::Select("a".into())
        );
        assert_eq!(
            picked(&select, Param::Toggle(true)),
            Param::Select("a".into())
        );
        assert_eq!(select.coerce(None), Param::Select("a".into()));
        let toggle = Control::toggle("t", "T", true);
        assert_eq!(picked(&toggle, Param::Toggle(false)), Param::Toggle(false));
        assert_eq!(picked(&toggle, Param::Range(0.0)), Param::Toggle(true));
        assert_eq!(toggle.coerce(None), Param::Toggle(true));
        let range = Control::range("r", "R", [1.0, 10.0], 0.5, 7.0);
        assert_eq!(picked(&range, Param::Range(3.3)), Param::Range(3.5));
        assert_eq!(picked(&range, Param::Range(3.2)), Param::Range(3.0));
        assert_eq!(picked(&range, Param::Range(-50.0)), Param::Range(1.0));
        assert_eq!(picked(&range, Param::Range(1e300)), Param::Range(10.0));
        assert_eq!(picked(&range, Param::Range(f64::NAN)), Param::Range(7.0));
        assert_eq!(
            picked(&range, Param::Range(f64::INFINITY)),
            Param::Range(7.0)
        );
        assert_eq!(picked(&range, Param::Select("3".into())), Param::Range(7.0));
        assert_eq!(range.coerce(None), Param::Range(7.0));
        let uneven = Control::range("r", "R", [0.0, 10.0], 3.0, 0.0);
        assert_eq!(picked(&uneven, Param::Range(9.9)), Param::Range(10.0));
        assert_eq!(picked(&uneven, Param::Range(9.4)), Param::Range(9.0));
        assert_eq!(picked(&uneven, Param::Range(7.4)), Param::Range(6.0));
        for at in [0.0, 3.0, 9.0, 10.0] {
            assert_eq!(picked(&uneven, Param::Range(at)), Param::Range(at), "{at}");
        }
        let unchecked = Control::range("r", "R", [10.0, 0.0], f64::NAN, 1.0);
        assert_eq!(picked(&unchecked, Param::Range(5.0)), Param::Range(1.0));
        let backwards = Control::range("r", "R", [10.0, 0.0], 1.0, 1.0);
        assert!(matches!(
            picked(&backwards, Param::Range(5.0)),
            Param::Range(value) if value.is_finite()
        ));
    }
}

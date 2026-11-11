//! The run workspace's layout: columns of stacked groups, each group a set of tabbed panes.
//!
//! ```text
//!   column 0 (share .55)        column 1 (share .45)
//!  +---------------------------+----------------------+
//!  | [Graph]                   | [Log] Data Files ... |  group 0 of each column
//!  |                           |                      |
//!  +---------------------------+                      |
//!  | [Tasks] Pack  Launch      |                      |  group 1 of column 0
//!  |                           |                      |
//!  +---------------------------+----------------------+
//! ```
//!
//! Shares are fractions of the parent's length and sum to 1 at each level. Every pane appears
//! at most once; a pane that appears nowhere is closed and can be reopened.

use serde::{Deserialize, Serialize};

/// The smallest share a column or group may be resized to.
pub const MIN_SHARE: f32 = 0.12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneKind {
    Tasks,
    Timeline,
    Graph,
    Data,
    Files,
    Transcript,
    Compare,
    Pack,
    Launch,
    Log,
    Agent,
}

impl PaneKind {
    pub const ALL: [PaneKind; 11] = [
        PaneKind::Tasks,
        PaneKind::Timeline,
        PaneKind::Graph,
        PaneKind::Data,
        PaneKind::Files,
        PaneKind::Transcript,
        PaneKind::Compare,
        PaneKind::Pack,
        PaneKind::Launch,
        PaneKind::Log,
        PaneKind::Agent,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaneKind::Tasks => "Tasks",
            PaneKind::Timeline => "Timeline",
            PaneKind::Graph => "Graph",
            PaneKind::Data => "Data",
            PaneKind::Files => "Files",
            PaneKind::Transcript => "Transcript",
            PaneKind::Compare => "Compare",
            PaneKind::Pack => "Pack",
            PaneKind::Launch => "Launch",
            PaneKind::Log => "Log",
            PaneKind::Agent => "Agent",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub tabs: Vec<PaneKind>,
    pub active: PaneKind,
    pub share: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub groups: Vec<Group>,
    pub share: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub columns: Vec<Column>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// Where a dragged pane lands, relative to the group holding `anchor`: among its tabs, or in a
/// new group or column on one of its edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dock {
    Into(PaneKind),
    Split(PaneKind, Edge),
}

/// A group's position: column index, then group index within the column.
pub type At = (usize, usize);

/// A gutter between two neighbours: columns `i` and `i + 1`, or groups `g` and `g + 1` of
/// column `c`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Divider {
    Columns(usize),
    Groups(usize, usize),
}

/// A tab being dragged: where the press was and where the pointer is, in window points, and
/// where the pane would land if dropped now.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PaneDrag {
    pub kind: PaneKind,
    pub origin: [f32; 2],
    pub pointer: [f32; 2],
    pub moved: bool,
    pub dock: Option<Dock>,
}

/// Window rects `[x, y, w, h]` the view last laid out, for hit-testing drags.
#[derive(Clone, Debug, Default)]
pub struct Geometry {
    pub area: [f32; 4],
    pub columns: std::collections::HashMap<usize, [f32; 4]>,
    pub groups: std::collections::HashMap<At, [f32; 4]>,
    pub tabs: std::collections::HashMap<PaneKind, [f32; 4]>,
}

impl Workspace {
    /// The layout for a run whose task graph is `depth` layers deep: the graph stacked over the
    /// tasks, taller the deeper the graph, beside the log.
    pub fn for_depth(depth: usize) -> Workspace {
        let graph = (0.12 + 0.07 * depth as f32).clamp(0.22, 0.55);
        Workspace {
            columns: vec![
                Column {
                    groups: vec![
                        group(&[PaneKind::Graph, PaneKind::Timeline], graph),
                        group(
                            &[
                                PaneKind::Tasks,
                                PaneKind::Compare,
                                PaneKind::Pack,
                                PaneKind::Launch,
                            ],
                            1.0 - graph,
                        ),
                    ],
                    share: 0.56,
                },
                Column {
                    groups: vec![group(
                        &[
                            PaneKind::Log,
                            PaneKind::Data,
                            PaneKind::Files,
                            PaneKind::Transcript,
                        ],
                        1.0,
                    )],
                    share: 0.44,
                },
            ],
        }
    }

    pub fn group(&self, (c, g): At) -> Option<&Group> {
        self.columns.get(c)?.groups.get(g)
    }

    pub fn locate(&self, kind: PaneKind) -> Option<At> {
        self.columns.iter().enumerate().find_map(|(c, column)| {
            column
                .groups
                .iter()
                .position(|group| group.tabs.contains(&kind))
                .map(|g| (c, g))
        })
    }

    /// Whether `kind` is the front tab of its group.
    pub fn showing(&self, kind: PaneKind) -> bool {
        self.locate(kind)
            .and_then(|at| self.group(at))
            .is_some_and(|group| group.active == kind)
    }

    pub fn closed(&self) -> impl Iterator<Item = PaneKind> + '_ {
        PaneKind::ALL
            .into_iter()
            .filter(|kind| self.locate(*kind).is_none())
    }

    /// Shows `kind`: brings it to the front of its group, or reopens it when it was closed,
    /// beside a pane it shares a group with by default, else in the first group. The agent
    /// reopens in a group of its own under the pack.
    pub fn activate(&mut self, kind: PaneKind) {
        let at = match self.locate(kind) {
            Some(at) => at,
            None if kind == PaneKind::Agent && self.locate(PaneKind::Pack).is_some() => {
                return self.place(kind, PaneKind::Pack, Some(Edge::Bottom));
            }
            None => {
                let default = Workspace::for_depth(0);
                let home = default
                    .locate(kind)
                    .and_then(|at| default.group(at))
                    .into_iter()
                    .flat_map(|group| group.tabs.iter())
                    .find_map(|other| self.locate(*other))
                    .unwrap_or((0, 0));
                let Some(group) = self
                    .columns
                    .get_mut(home.0)
                    .and_then(|c| c.groups.get_mut(home.1))
                else {
                    *self = default;
                    return self.activate(kind);
                };
                group.tabs.push(kind);
                home
            }
        };
        if let Some(group) = self.columns[at.0].groups.get_mut(at.1) {
            group.active = kind;
        }
    }

    /// Closes `kind`, unless it is the last open pane.
    pub fn close(&mut self, kind: PaneKind) {
        if self.closed().count() + 1 >= PaneKind::ALL.len() {
            return;
        }
        self.remove(kind);
    }

    /// Moves `kind` to `dock`. Dropping a pane on its own group, or splitting a group that holds
    /// only that pane, leaves the layout as it was.
    pub fn dock(&mut self, kind: PaneKind, dock: Dock) {
        let (anchor, edge) = match dock {
            Dock::Into(anchor) => (anchor, None),
            Dock::Split(anchor, edge) => (anchor, Some(edge)),
        };
        let (Some(from), Some(to)) = (self.locate(kind), self.locate(anchor)) else {
            return;
        };
        let target = &self.columns[to.0].groups[to.1];
        if from == to && (edge.is_none() || target.tabs.len() == 1) {
            self.activate(kind);
            return;
        }
        // The pane that still marks the target group once `kind` has left it.
        let Some(anchor) = target.tabs.iter().copied().find(|tab| *tab != kind) else {
            return;
        };
        self.remove(kind);
        self.place(kind, anchor, edge);
    }

    /// Puts `kind`, which is in no group, in `anchor`'s group or on one of its edges.
    fn place(&mut self, kind: PaneKind, anchor: PaneKind, edge: Option<Edge>) {
        let Some((c, g)) = self.locate(anchor) else {
            return;
        };
        match edge {
            None => {
                let group = &mut self.columns[c].groups[g];
                group.tabs.push(kind);
                group.active = kind;
            }
            Some(edge @ (Edge::Top | Edge::Bottom)) => {
                let column = &mut self.columns[c];
                let half = column.groups[g].share / 2.0;
                column.groups[g].share = half;
                let at = if edge == Edge::Top { g } else { g + 1 };
                column.groups.insert(at, group(&[kind], half));
            }
            Some(edge @ (Edge::Left | Edge::Right)) => {
                let half = self.columns[c].share / 2.0;
                self.columns[c].share = half;
                let at = if edge == Edge::Left { c } else { c + 1 };
                self.columns.insert(
                    at,
                    Column {
                        groups: vec![group(&[kind], 1.0)],
                        share: half,
                    },
                );
            }
        }
    }

    /// Sets column `i`'s share to `start + delta`, taking the difference from column `i + 1`,
    /// whose share was `next` when the drag began.
    pub fn resize_columns(&mut self, i: usize, (start, next): (f32, f32), delta: f32) {
        if i + 1 >= self.columns.len() {
            return;
        }
        let (a, b) = split(start, next, delta);
        self.columns[i].share = a;
        self.columns[i + 1].share = b;
    }

    /// [`Self::resize_columns`] for groups `g` and `g + 1` of column `c`.
    pub fn resize_groups(&mut self, c: usize, g: usize, (start, next): (f32, f32), delta: f32) {
        let Some(column) = self.columns.get_mut(c) else {
            return;
        };
        if g + 1 >= column.groups.len() {
            return;
        }
        let (a, b) = split(start, next, delta);
        column.groups[g].share = a;
        column.groups[g + 1].share = b;
    }

    /// Repairs a layout read from disk: drops repeated and empty entries, fixes each group's
    /// active pane, and rescales shares to sum to 1. A layout with no panes left is `None`.
    pub fn repaired(mut self) -> Option<Workspace> {
        let mut seen = Vec::new();
        for column in &mut self.columns {
            for group in &mut column.groups {
                group.tabs.retain(|tab| {
                    let fresh = !seen.contains(tab);
                    seen.push(*tab);
                    fresh
                });
                if !group.tabs.contains(&group.active)
                    && let Some(first) = group.tabs.first()
                {
                    group.active = *first;
                }
            }
            column.groups.retain(|group| !group.tabs.is_empty());
        }
        self.columns.retain(|column| !column.groups.is_empty());
        if self.columns.is_empty() {
            return None;
        }
        normalize(self.columns.iter_mut().map(|column| &mut column.share));
        for column in &mut self.columns {
            normalize(column.groups.iter_mut().map(|group| &mut group.share));
        }
        Some(self)
    }

    /// Takes `kind` out, folding an emptied group or column's share into its neighbours.
    fn remove(&mut self, kind: PaneKind) {
        let Some((c, g)) = self.locate(kind) else {
            return;
        };
        let group = &mut self.columns[c].groups[g];
        group.tabs.retain(|tab| *tab != kind);
        if let Some(first) = group.tabs.first()
            && group.active == kind
        {
            group.active = *first;
        }
        if group.tabs.is_empty() {
            self.columns[c].groups.remove(g);
            normalize(
                self.columns[c]
                    .groups
                    .iter_mut()
                    .map(|group| &mut group.share),
            );
        }
        if self.columns[c].groups.is_empty() {
            self.columns.remove(c);
            normalize(self.columns.iter_mut().map(|column| &mut column.share));
        }
    }
}

/// Where a pane dropped at window point `p` lands. `groups` are the groups' window rects
/// `[x, y, w, h]` by position; the top `header` points of a group are its tab bar, which takes
/// the pane as a tab, as does the group's middle. The outer quarter on each side splits.
pub fn hit(ws: &Workspace, groups: &[(At, [f32; 4])], header: f32, p: [f32; 2]) -> Option<Dock> {
    let (at, [x, y, w, h]) = groups
        .iter()
        .copied()
        .find(|(_, [x, y, w, h])| p[0] >= *x && p[0] < x + w && p[1] >= *y && p[1] < y + h)?;
    let anchor = ws.group(at)?.active;
    if p[1] - y < header || w <= 0.0 || h <= header {
        return Some(Dock::Into(anchor));
    }
    let u = (p[0] - x) / w;
    let v = (p[1] - y - header) / (h - header);
    let edges = [
        (u, Edge::Left),
        (1.0 - u, Edge::Right),
        (v, Edge::Top),
        (1.0 - v, Edge::Bottom),
    ];
    let (near, edge) = edges
        .into_iter()
        .fold((f32::INFINITY, Edge::Left), |best, e| {
            if e.0 < best.0 { e } else { best }
        });
    Some(if near < 0.25 {
        Dock::Split(anchor, edge)
    } else {
        Dock::Into(anchor)
    })
}

/// How deep `tasks` (name, dependencies) make the graph: the longest dependency chain, in
/// layers. A dependency on a task already on the chain (a cycle) adds nothing.
pub fn depth<'a>(tasks: impl IntoIterator<Item = (&'a str, &'a [String])>) -> usize {
    use std::collections::{HashMap, HashSet};

    fn layer<'a>(
        name: &'a str,
        deps: &HashMap<&'a str, &'a [String]>,
        memo: &mut HashMap<&'a str, usize>,
        chain: &mut HashSet<&'a str>,
    ) -> usize {
        if let Some(n) = memo.get(name) {
            return *n;
        }
        if !chain.insert(name) {
            return 0;
        }
        let below = deps
            .get(name)
            .into_iter()
            .flat_map(|ds| ds.iter())
            .filter_map(|d| deps.get_key_value(d.as_str()).map(|(k, _)| *k))
            .map(|d| layer(d, deps, memo, chain))
            .max()
            .unwrap_or(0);
        chain.remove(name);
        memo.insert(name, below + 1);
        below + 1
    }

    let deps: HashMap<&str, &[String]> = tasks.into_iter().collect();
    let mut memo = HashMap::new();
    let mut chain = HashSet::new();
    deps.keys()
        .map(|name| layer(name, &deps, &mut memo, &mut chain))
        .max()
        .unwrap_or(0)
}

/// The group on `edge`'s side of `from` in the laid-out `rects` (`[x, y, w, h]`): of those wholly
/// past that side, the ones that line up with it across first, then the nearest.
pub fn neighbour(
    rects: &std::collections::HashMap<At, [f32; 4]>,
    from: At,
    edge: Edge,
) -> Option<At> {
    let [fx, fy, fw, fh] = *rects.get(&from)?;
    let overlap = |a0: f32, a1: f32, b0: f32, b1: f32| (a1.min(b1) - a0.max(b0)).max(0.0);
    rects
        .iter()
        .filter(|(at, _)| **at != from)
        .filter_map(|(at, &[x, y, w, h])| {
            let (gap, across, offset) = match edge {
                Edge::Left => (fx - (x + w), overlap(y, y + h, fy, fy + fh), (y - fy).abs()),
                Edge::Right => (
                    x - (fx + fw),
                    overlap(y, y + h, fy, fy + fh),
                    (y - fy).abs(),
                ),
                Edge::Top => (fy - (y + h), overlap(x, x + w, fx, fx + fw), (x - fx).abs()),
                Edge::Bottom => (
                    y - (fy + fh),
                    overlap(x, x + w, fx, fx + fw),
                    (x - fx).abs(),
                ),
            };
            (gap > -1.0).then_some((*at, across <= 0.0, gap + offset))
        })
        .min_by(|a, b| {
            (a.1, a.2)
                .partial_cmp(&(b.1, b.2))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(at, _, _)| at)
}

fn group(tabs: &[PaneKind], share: f32) -> Group {
    Group {
        tabs: tabs.to_vec(),
        active: tabs[0],
        share,
    }
}

fn split(start: f32, next: f32, delta: f32) -> (f32, f32) {
    let total = start + next;
    if total <= 2.0 * MIN_SHARE {
        return (start, next);
    }
    let a = (start + delta).clamp(MIN_SHARE, total - MIN_SHARE);
    (a, total - a)
}

fn normalize<'a>(shares: impl Iterator<Item = &'a mut f32>) {
    let mut shares: Vec<&mut f32> = shares.collect();
    for share in &mut shares {
        if !share.is_finite() || **share <= 0.0 {
            **share = MIN_SHARE;
        }
    }
    let total: f32 = shares.iter().map(|s| **s).sum();
    for share in shares {
        *share /= total;
    }
}

#[cfg(test)]
mod tests {
    use crate::workspace::{Dock, Edge, MIN_SHARE, PaneKind, Workspace, depth, hit, neighbour};

    use PaneKind::{
        Agent, Compare, Data, Files, Graph, Launch, Log, Pack, Tasks, Timeline, Transcript,
    };

    #[test]
    fn a_neighbour_is_the_nearest_group_on_that_side_lined_up_first() {
        //   +------+-----+
        //   | 0,0  |     |
        //   +------+ 1,0 |
        //   | 0,1  |     |
        //   +--+---+-----+
        //   |0,2|
        //   +---+
        let rects = std::collections::HashMap::from([
            ((0, 0), [0.0, 0.0, 600.0, 300.0]),
            ((0, 1), [0.0, 304.0, 600.0, 300.0]),
            ((0, 2), [0.0, 608.0, 200.0, 100.0]),
            ((1, 0), [604.0, 0.0, 400.0, 604.0]),
        ]);
        let go = |from, edge| neighbour(&rects, from, edge);
        assert_eq!(go((0, 0), Edge::Bottom), Some((0, 1)));
        assert_eq!(go((0, 1), Edge::Bottom), Some((0, 2)));
        assert_eq!(go((0, 1), Edge::Top), Some((0, 0)));
        assert_eq!(go((0, 0), Edge::Right), Some((1, 0)));
        assert_eq!(go((0, 1), Edge::Right), Some((1, 0)));
        assert_eq!(go((1, 0), Edge::Left), Some((0, 0)));
        assert_eq!(go((0, 0), Edge::Left), None);
        assert_eq!(go((0, 0), Edge::Top), None);
        assert_eq!(
            go((1, 0), Edge::Bottom),
            Some((0, 2)),
            "nothing lines up, so the nearest"
        );
        assert_eq!(go((9, 9), Edge::Left), None);
    }

    fn tabs(w: &Workspace) -> Vec<Vec<Vec<PaneKind>>> {
        w.columns
            .iter()
            .map(|c| c.groups.iter().map(|g| g.tabs.clone()).collect())
            .collect()
    }

    fn sums_to_one(w: &Workspace) {
        let cols: f32 = w.columns.iter().map(|c| c.share).sum();
        assert!((cols - 1.0).abs() < 1e-5, "columns sum {cols}");
        for c in &w.columns {
            let groups: f32 = c.groups.iter().map(|g| g.share).sum();
            assert!((groups - 1.0).abs() < 1e-5, "groups sum {groups}");
        }
    }

    #[test]
    fn the_default_stacks_the_graph_over_the_tasks_beside_the_log() {
        let w = Workspace::for_depth(3);
        assert_eq!(
            tabs(&w),
            vec![
                vec![vec![Graph, Timeline], vec![Tasks, Compare, Pack, Launch]],
                vec![vec![Log, Data, Files, Transcript]]
            ]
        );
        sums_to_one(&w);
        assert_eq!(w.closed().collect::<Vec<_>>(), vec![Agent]);
    }

    #[test]
    fn a_deeper_graph_gets_a_taller_pane_within_bounds() {
        let share = |d| Workspace::for_depth(d).columns[0].groups[0].share;
        assert!(share(1) < share(4));
        assert!(share(4) < share(6));
        assert!((share(0) - 0.22).abs() < 1e-6);
        assert!((share(40) - 0.55).abs() < 1e-6);
    }

    #[test]
    fn depth_counts_the_longest_chain_and_survives_cycles() {
        let t =
            |n: &'static str, d: &[&str]| (n, d.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let graph = [
            t("draft", &[]),
            t("shape", &["draft"]),
            t("polish", &["shape"]),
            t("audit", &["polish"]),
            t("side", &["draft"]),
            t("ghost", &["missing"]),
        ];
        let view = graph.iter().map(|(n, d)| (*n, d.as_slice()));
        assert_eq!(depth(view), 4);
        let cyclic = [t("a", &["b"]), t("b", &["a"])];
        assert_eq!(depth(cyclic.iter().map(|(n, d)| (*n, d.as_slice()))), 2);
        assert_eq!(depth(std::iter::empty::<(&str, &[String])>()), 0);
    }

    #[test]
    fn docking_into_a_group_adds_a_tab_and_shows_it() {
        let mut w = Workspace::for_depth(2);
        w.dock(Log, Dock::Into(Tasks));
        assert_eq!(
            tabs(&w),
            vec![
                vec![
                    vec![Graph, Timeline],
                    vec![Tasks, Compare, Pack, Launch, Log]
                ],
                vec![vec![Data, Files, Transcript]]
            ]
        );
        assert_eq!(w.columns[0].groups[1].active, Log);
        sums_to_one(&w);
    }

    #[test]
    fn splitting_an_edge_makes_a_new_group_or_column_from_half_the_target() {
        let mut w = Workspace::for_depth(2);
        let tasks_share = w.columns[0].groups[1].share;
        w.dock(Pack, Dock::Split(Tasks, Edge::Bottom));
        assert_eq!(
            tabs(&w)[0],
            vec![
                vec![Graph, Timeline],
                vec![Tasks, Compare, Launch],
                vec![Pack]
            ]
        );
        assert!((w.columns[0].groups[2].share - tasks_share / 2.0).abs() < 1e-6);
        sums_to_one(&w);

        w.dock(Graph, Dock::Split(Log, Edge::Right));
        assert_eq!(
            tabs(&w),
            vec![
                vec![vec![Timeline], vec![Tasks, Compare, Launch], vec![Pack]],
                vec![vec![Log, Data, Files, Transcript]],
                vec![vec![Graph]]
            ]
        );
        sums_to_one(&w);

        w.dock(Launch, Dock::Split(Tasks, Edge::Left));
        assert_eq!(tabs(&w)[0], vec![vec![Launch]]);
        sums_to_one(&w);
    }

    #[test]
    fn splitting_a_pane_off_its_own_group_works_but_a_lone_pane_stays_put() {
        let mut w = Workspace::for_depth(2);
        w.dock(Pack, Dock::Split(Pack, Edge::Top));
        assert_eq!(
            tabs(&w)[0],
            vec![
                vec![Graph, Timeline],
                vec![Pack],
                vec![Tasks, Compare, Launch]
            ]
        );
        let before = w.clone();
        w.dock(Pack, Dock::Split(Pack, Edge::Left));
        assert_eq!(w, before);
        w.dock(Graph, Dock::Into(Timeline));
        assert_eq!(tabs(&w), tabs(&before));
        assert_eq!(w.columns[0].groups[0].active, Graph);
    }

    #[test]
    fn closing_folds_space_back_and_never_closes_the_last_pane() {
        let mut w = Workspace::for_depth(2);
        w.close(Log);
        w.close(Data);
        w.close(Files);
        w.close(Transcript);
        assert_eq!(w.columns.len(), 1);
        sums_to_one(&w);
        w.close(Timeline);
        w.close(Graph);
        w.close(Pack);
        w.close(Launch);
        w.close(Compare);
        w.close(Tasks);
        assert_eq!(tabs(&w), vec![vec![vec![Tasks]]]);
        assert_eq!(w.closed().count(), 10);
        w.activate(Log);
        assert_eq!(tabs(&w), vec![vec![vec![Tasks, Log]]]);
        assert_eq!(w.columns[0].groups[0].active, Log);
    }

    #[test]
    fn a_closed_pane_reopens_beside_its_default_neighbours_and_shows() {
        let mut w = Workspace::for_depth(2);
        w.close(Data);
        assert!(!w.showing(Data));
        w.activate(Data);
        assert_eq!(tabs(&w)[1], vec![vec![Log, Files, Transcript, Data]]);
        assert!(w.showing(Data) && !w.showing(Log));
        w.close(Data);
        w.close(Files);
        w.close(Log);
        w.close(Transcript);
        w.activate(Data);
        assert_eq!(
            tabs(&w),
            vec![vec![
                vec![Graph, Timeline, Data],
                vec![Tasks, Compare, Pack, Launch]
            ]]
        );
    }

    #[test]
    fn the_agent_opens_under_the_pack_and_docks_like_any_pane() {
        let mut w = Workspace::for_depth(2);
        let pack = w.columns[0].groups[1].share;
        w.activate(Agent);
        assert_eq!(
            tabs(&w)[0],
            vec![
                vec![Graph, Timeline],
                vec![Tasks, Compare, Pack, Launch],
                vec![Agent]
            ]
        );
        assert!(w.showing(Agent));
        assert_eq!(w.columns[0].groups[2].share, pack / 2.0);
        sums_to_one(&w);
        w.activate(Agent);
        assert_eq!(tabs(&w)[0].len(), 3);
        w.dock(Agent, Dock::Split(Log, Edge::Right));
        assert_eq!(tabs(&w)[2], vec![vec![Agent]]);
        assert_eq!(tabs(&w)[0].len(), 2);
        sums_to_one(&w);
        w.close(Agent);
        assert_eq!(tabs(&w), tabs(&Workspace::for_depth(2)));
        sums_to_one(&w);

        let mut w = Workspace::for_depth(2);
        w.close(Pack);
        w.activate(Agent);
        assert_eq!(tabs(&w)[0][0], vec![Graph, Timeline, Agent]);
    }

    #[test]
    fn closing_the_active_tab_shows_its_neighbour() {
        let mut w = Workspace::for_depth(2);
        w.activate(Pack);
        w.close(Pack);
        assert_eq!(w.columns[0].groups[1].active, Tasks);
    }

    #[test]
    fn resizing_trades_share_between_neighbours_and_keeps_a_floor() {
        let mut w = Workspace::for_depth(2);
        let (a, b) = (w.columns[0].share, w.columns[1].share);
        w.resize_columns(0, (a, b), 0.1);
        assert!((w.columns[0].share - (a + 0.1)).abs() < 1e-6);
        sums_to_one(&w);
        w.resize_columns(0, (a, b), 5.0);
        assert!((w.columns[1].share - MIN_SHARE).abs() < 1e-6);
        let (g0, g1) = (w.columns[0].groups[0].share, w.columns[0].groups[1].share);
        w.resize_groups(0, 0, (g0, g1), -5.0);
        assert!((w.columns[0].groups[0].share - MIN_SHARE).abs() < 1e-6);
        sums_to_one(&w);
        let before = w.clone();
        w.resize_columns(1, (0.5, 0.5), 0.1);
        w.resize_groups(1, 0, (0.5, 0.5), 0.1);
        w.resize_groups(9, 0, (0.5, 0.5), 0.1);
        assert_eq!(w, before);
    }

    #[test]
    fn a_layout_from_disk_is_repaired_or_refused() {
        let text = r#"
            [[columns]]
            share = 3.0
            [[columns.groups]]
            tabs = ["log", "log", "tasks"]
            active = "graph"
            share = 1.0
            [[columns.groups]]
            tabs = []
            active = "pack"
            share = 1.0
            [[columns]]
            share = 1.0
            [[columns.groups]]
            tabs = ["tasks"]
            active = "tasks"
            share = 0.0
        "#;
        let w: Workspace = toml::from_str(text).unwrap();
        let w = w.repaired().unwrap();
        assert_eq!(tabs(&w), vec![vec![vec![Log, Tasks]]]);
        assert_eq!(w.columns[0].groups[0].active, Log);
        sums_to_one(&w);
        let empty: Workspace = toml::from_str("columns = []").unwrap();
        assert_eq!(empty.repaired(), None);
    }

    #[test]
    fn a_drop_point_picks_a_tab_bar_middle_or_edge() {
        let w = Workspace::for_depth(2);
        let groups = [
            ((0, 0), [0.0, 0.0, 400.0, 200.0]),
            ((1, 0), [410.0, 0.0, 300.0, 400.0]),
        ];
        let at = |x, y| hit(&w, &groups, 30.0, [x, y]);
        assert_eq!(at(200.0, 10.0), Some(Dock::Into(Graph)));
        assert_eq!(at(10.0, 10.0), Some(Dock::Into(Graph)));
        assert_eq!(at(200.0, 115.0), Some(Dock::Into(Graph)));
        assert_eq!(at(20.0, 115.0), Some(Dock::Split(Graph, Edge::Left)));
        assert_eq!(at(390.0, 115.0), Some(Dock::Split(Graph, Edge::Right)));
        assert_eq!(at(200.0, 40.0), Some(Dock::Split(Graph, Edge::Top)));
        assert_eq!(at(200.0, 195.0), Some(Dock::Split(Graph, Edge::Bottom)));
        assert_eq!(at(560.0, 380.0), Some(Dock::Split(Log, Edge::Bottom)));
        assert_eq!(at(405.0, 100.0), None);
        assert_eq!(at(560.0, 500.0), None);
    }

    #[test]
    fn a_layout_round_trips_through_toml() {
        let w = Workspace::for_depth(4);
        let text = toml::to_string(&w).unwrap();
        assert_eq!(toml::from_str::<Workspace>(&text).unwrap(), w);
    }
}

//! What a drawn report keeps between frames: the report, the reader's controls, tabs and
//! sections, and the shown table's filter, sort and selection.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crucible_view::{Cell, Column, ColumnKind, Control, Node, Param, Row, View};
use rune::TextEdit;
use rune_kit::{SortDir, TableState};

use crate::report::play::Play;
use crate::report::{Asked, Report};

/// The table a view is showing.
#[derive(Clone, Copy)]
pub struct Table<'a> {
    pub id: &'a str,
    pub columns: &'a [Column],
    pub rows: &'a [Row],
}

/// What the Report pane keeps between frames: the report it shows, the reader's controls, the
/// tab picked in each set of tabs, the sections the reader folded or unfolded, and the shown
/// table's filter, sort and selection.
pub struct Shown {
    /// The report all of this was kept for.
    report: Option<Arc<Report>>,
    /// The controls of the last view drawn for the selection.
    controls: Vec<Control>,
    /// What the reader set each control to, by id.
    picks: HashMap<String, Param>,
    /// What the plugin is asked with: every control's value as of the reader's last change,
    /// less those a later view no longer offers.
    asked: BTreeMap<String, Param>,
    /// The range being dragged and where it is, set on release.
    sliding: Option<(String, f64)>,
    /// The select whose options are open.
    pub choosing: Option<String>,
    /// The live view's state and events.
    pub play: Play,
    tabs: HashMap<String, usize>,
    sections: HashMap<String, bool>,
    pub table: TableState,
    /// What the box over the shown table holds: a row is drawn when it holds every word.
    pub filter: TextEdit,
    sorted: Option<(String, SortDir)>,
    /// The shown table's rows that pass the filter, as sorted; `None` when that is every row
    /// as written.
    order: Option<Vec<usize>>,
}

/// A [`Shown`] kept for one key, such as the selection, and started over for another.
pub struct Scoped<K> {
    key: Option<K>,
    pub shown: Shown,
}

impl<K> Default for Scoped<K> {
    fn default() -> Self {
        Scoped {
            key: None,
            shown: Shown::default(),
        }
    }
}

impl<K: PartialEq + Clone> Scoped<K> {
    /// Follows the pane to `report` under `key`: another key starts over, and another report
    /// for the same one keeps its controls and tabs and filters and sorts its table again.
    pub fn follow(&mut self, key: Option<&K>, report: Option<&Arc<Report>>) {
        if self.key.as_ref() != key {
            self.key = key.cloned();
            self.shown = Shown::default();
        }
        self.shown.follow(report);
    }
}

/// What a cell sorts by before its text: its value, or its text as a number in a number
/// column.
fn figure(cell: &Cell, kind: ColumnKind) -> Option<f64> {
    cell.value.or_else(|| {
        (kind == ColumnKind::Number)
            .then(|| cell.text.trim().replace(',', "").parse::<f64>().ok())
            .flatten()
            .filter(|figure| figure.is_finite())
    })
}

/// Whether `text` holds `word`, which is in lower case, in any case.
fn holds(text: &str, word: &str) -> bool {
    if text.is_ascii() && word.is_ascii() {
        text.as_bytes()
            .windows(word.len().max(1))
            .any(|window| window.eq_ignore_ascii_case(word.as_bytes()))
    } else {
        text.to_lowercase().contains(word)
    }
}

/// The rows of `table` holding every word of `query`, sorted by `sort` when the table has that
/// column: figures first and in order, then text in natural order. `None` when that is every
/// row as written.
fn order(table: &Table<'_>, sort: Option<(&str, SortDir)>, query: &str) -> Option<Vec<usize>> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let sort = sort.and_then(|(column, dir)| {
        let at = table.columns.iter().position(|c| c.id == column)?;
        Some((at, table.columns[at].kind, dir))
    });
    if words.is_empty() && sort.is_none() {
        return None;
    }
    let mut rows: Vec<usize> = (0..table.rows.len())
        .filter(|row| {
            let row = &table.rows[*row];
            words.iter().all(|word| {
                holds(&row.id, word) || row.cells.iter().any(|cell| holds(&cell.text, word))
            })
        })
        .collect();
    if let Some((at, kind, dir)) = sort {
        let key = |row: usize| {
            table.rows[row]
                .cells
                .get(at)
                .map_or((None, ""), |cell| (figure(cell, kind), cell.text.as_str()))
        };
        let keys: Vec<(Option<f64>, &str)> = (0..table.rows.len()).map(key).collect();
        rows.sort_by(|&a, &b| match (keys[a], keys[b]) {
            ((Some(x), _), (Some(y), _)) => dir.apply(x.total_cmp(&y)),
            ((Some(_), _), (None, _)) => Ordering::Less,
            ((None, _), (Some(_), _)) => Ordering::Greater,
            ((None, x), (None, y)) => dir.apply(rune_kit::natural_cmp(x, y)),
        });
    }
    Some(rows)
}

impl Default for Shown {
    fn default() -> Self {
        Shown {
            report: None,
            controls: Vec::new(),
            picks: HashMap::new(),
            asked: BTreeMap::new(),
            sliding: None,
            choosing: None,
            play: Play::default(),
            tabs: HashMap::new(),
            sections: HashMap::new(),
            table: fresh_table(),
            filter: TextEdit::new(""),
            sorted: None,
            order: None,
        }
    }
}

fn fresh_table() -> TableState {
    TableState::new().key("report-table")
}

impl Shown {
    /// Follows the pane to `report`: another report keeps the controls and tabs and filters
    /// and sorts its table again.
    pub fn follow(&mut self, report: Option<&Arc<Report>>) {
        if self.holds(report) {
            return;
        }
        self.report = report.cloned();
        match report {
            Some(report) => self.play.answer(report),
            None => self.play.rest(),
        }
        self.declare();
        self.arrange();
    }

    /// Whether `report` is the one this follows.
    pub fn holds(&self, report: Option<&Arc<Report>>) -> bool {
        match (report, &self.report) {
            (Some(new), Some(held)) => Arc::ptr_eq(new, held),
            (None, None) => true,
            _ => false,
        }
    }

    fn view(&self) -> Option<&View> {
        self.report.as_ref()?.view.as_ref().ok()
    }

    /// Takes the controls of the report's view, when it has one, and drops what the reader set
    /// on controls it no longer has.
    fn declare(&mut self) {
        let Some(view) = self
            .report
            .as_ref()
            .and_then(|report| report.view.as_ref().ok())
        else {
            return;
        };
        self.controls.clone_from(&view.controls);
        let declared = |id: &str| view.controls.iter().any(|control| control.id() == id);
        let offered = |id: &String, value: &mut Param| {
            view.controls
                .iter()
                .any(|control| control.id() == id && control.coerce(Some(value)) == *value)
        };
        self.picks.retain(offered);
        self.asked.retain(offered);
        self.sliding = self.sliding.take().filter(|(id, _)| declared(id));
        self.choosing = self.choosing.take().filter(|id| declared(id));
    }

    /// The controls of the last view drawn for the selection.
    pub fn controls(&self) -> &[Control] {
        &self.controls
    }

    /// What `control` shows: where it is being dragged, else what the reader set it to, else
    /// its own value.
    pub fn value(&self, control: &Control) -> Param {
        match &self.sliding {
            Some((id, at)) if id == control.id() => control.coerce(Some(&Param::Range(*at))),
            _ => control.coerce(self.picks.get(control.id())),
        }
    }

    /// Sets the control `id`, ending any drag, and asks with every control's value.
    pub fn set(&mut self, id: &str, value: Param) {
        self.sliding = None;
        self.choosing = None;
        let Some(control) = self.controls.iter().find(|control| control.id() == id) else {
            return;
        };
        self.picks
            .insert(id.to_string(), control.coerce(Some(&value)));
        self.asked = self
            .controls
            .iter()
            .map(|control| {
                (
                    control.id().to_string(),
                    control.coerce(self.picks.get(control.id())),
                )
            })
            .collect();
    }

    /// Moves the range `id` to `at` without asking the plugin again.
    pub fn slide(&mut self, id: &str, at: f64) {
        self.sliding = Some((id.to_string(), at));
    }

    /// Ends a drag without setting the range.
    pub fn cancel_slide(&mut self) {
        self.sliding = None;
    }

    /// Ends a drag of the range `id` where it is.
    pub fn release(&mut self, id: &str) {
        if let Some((sliding, at)) = self.sliding.take() {
            if sliding == id {
                self.set(id, Param::Range(at));
            } else {
                self.sliding = Some((sliding, at));
            }
        }
    }

    /// What the plugin is asked with next: every control's value once the reader has set one,
    /// nothing before, and the live view's turn, state and event.
    pub fn asked(&self) -> Asked {
        self.play.asked(&self.asked)
    }

    /// Whether the report was built with other params than its controls now hold.
    pub fn pending(&self) -> bool {
        self.report
            .as_ref()
            .is_some_and(|report| report.view.is_ok() && report.asked.params != self.asked)
    }

    /// The picked tab of the `len` tabs named `id`.
    pub fn tab(&self, id: &str, len: usize) -> usize {
        self.tabs
            .get(id)
            .copied()
            .unwrap_or(0)
            .min(len.saturating_sub(1))
    }

    pub fn pick(&mut self, id: &str, tab: usize) {
        self.tabs.insert(id.to_string(), tab);
        self.table = fresh_table();
        self.filter = TextEdit::new("");
        self.sorted = None;
        self.order = None;
    }

    /// Whether the section `id`, which starts `open` or not, is unfolded.
    pub fn open(&self, id: &str, open: bool) -> bool {
        self.sections.get(id).copied().unwrap_or(open)
    }

    /// Folds the section `id` when it is unfolded and unfolds it when it is folded, then
    /// filters and sorts the table that leaves shown.
    pub fn toggle(&mut self, id: &str, open: bool) {
        let unfolded = self.open(id, open);
        self.sections.insert(id.to_string(), !unfolded);
        self.arrange();
    }

    /// The one table the view shows with the tabs as picked and the sections as folded.
    pub fn table(&self) -> Option<Table<'_>> {
        self.table_in(&self.view()?.body)
    }

    /// Whether `node` takes the height its parent leaves, as a table and tabs do, rather than
    /// the height of its content.
    pub fn fills(&self, node: &Node) -> bool {
        match node {
            Node::Stack { children } => children.iter().any(|child| self.fills(child)),
            Node::Section { id, open, body, .. } => self.open(id, *open) && self.fills(body),
            Node::Tabs { .. } | Node::Table { .. } => true,
            _ => false,
        }
    }

    fn table_in<'a>(&self, node: &'a Node) -> Option<Table<'a>> {
        match node {
            Node::Stack { children } => children.iter().find_map(|child| self.table_in(child)),
            Node::Tabs { id, tabs } => self.table_in(&tabs.get(self.tab(id, tabs.len()))?.body),
            Node::Section { id, open, body, .. } => {
                self.open(id, *open).then(|| self.table_in(body)).flatten()
            }
            Node::Table { id, columns, rows } => Some(Table { id, columns, rows }),
            _ => None,
        }
    }

    /// How many rows of the shown table are drawn.
    pub fn rows(&self) -> usize {
        self.table().map_or(0, |table| {
            self.order.as_ref().map_or(table.rows.len(), Vec::len)
        })
    }

    /// Row `i` of the shown table as filtered and sorted.
    pub fn row(&self, i: usize) -> Option<&Row> {
        let i = match &self.order {
            Some(order) => *order.get(i)?,
            None => i,
        };
        self.table()?.rows.get(i)
    }

    /// The shown table as filtered and sorted, as CSV.
    pub fn csv(&self) -> Option<String> {
        let table = self.table()?;
        let rows = (0..self.rows()).filter_map(|i| self.row(i));
        Some(csv(
            table.columns,
            rows,
            crucible_view::Limits::DEFAULT.cells,
        ))
    }

    /// The first address the shown table's row `id` links to.
    pub fn link(&self, id: &str) -> Option<String> {
        let row = self.table()?.rows.iter().find(|row| row.id == id)?;
        row.cells.iter().find_map(|cell| cell.href.clone())
    }

    /// The shown table's one selected row, when it has a detail.
    pub fn detail(&self) -> Option<&Row> {
        let id = self
            .table
            .selected()
            .next()
            .filter(|_| self.table.selected_len() == 1)?;
        let row = self.table()?.rows.iter().find(|row| row.id == id)?;
        row.detail.is_some().then_some(row)
    }

    /// Sorts the shown table by `column`.
    pub fn sort(&mut self, column: &str, dir: SortDir) {
        self.sorted = Some((column.to_string(), dir));
        self.arrange();
    }

    /// Filters and sorts the shown table by what the filter box and the sort now say.
    pub fn arrange(&mut self) {
        let sort = self
            .sorted
            .as_ref()
            .map(|(column, dir)| (column.as_str(), *dir));
        self.order = self
            .table()
            .and_then(|table| order(&table, sort, &self.filter.text));
    }
}

/// `rows` under a line of the `columns`' titles, as CSV (RFC 4180), the first `cells` cells
/// at most. A field a spreadsheet would read as a formula starts with an apostrophe.
pub fn csv<'a>(columns: &[Column], rows: impl Iterator<Item = &'a Row>, cells: usize) -> String {
    let mut out = String::new();
    let mut line = |fields: &mut dyn Iterator<Item = &str>| {
        for (i, text) in fields.enumerate() {
            if i > 0 {
                out.push(',');
            }
            field(text, &mut out);
        }
        out.push_str("\r\n");
    };
    line(&mut columns.iter().map(|column| column.title.as_str()));
    let width = columns.len().max(1);
    for row in rows.take(cells / width) {
        line(&mut row.cells.iter().map(|cell| cell.text.as_str()));
    }
    out
}

fn field(text: &str, out: &mut String) {
    let quoted = text.contains([',', '"', '\r', '\n']);
    if quoted {
        out.push('"');
    }
    if text.starts_with(['=', '+', '-', '@', '\t', '\r']) && !plain_number(text) {
        out.push('\'');
    }
    for c in text.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    if quoted {
        out.push('"');
    }
}

/// Whether `text` is a finite number in digits, a sign, a point and an exponent alone.
fn plain_number(text: &str) -> bool {
    text.bytes()
        .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b))
        && text.parse::<f64>().is_ok_and(f64::is_finite)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crucible_view::{Cell, Column, ColumnKind, Node, Param, Row, Tab, View};
    use rune_kit::SortDir;

    use crate::Selection;
    use crate::report::Report;
    use crate::report::shown::{Scoped, Shown};

    fn table(id: &str, names: &[&str]) -> Node {
        Node::Table {
            id: id.into(),
            columns: vec![Column {
                id: "name".into(),
                title: "Name".into(),
                kind: Default::default(),
                width: Default::default(),
            }],
            rows: names
                .iter()
                .map(|name| Row {
                    id: name.to_string(),
                    cells: vec![Cell::text(*name)],
                    detail: None,
                })
                .collect(),
        }
    }

    fn report_of(view: View) -> Arc<Report> {
        Arc::new(Report {
            asked: Default::default(),
            author: None,
            view: Ok(view),
        })
    }

    /// A [`Shown`] following a report of `view`.
    fn holding(view: View) -> Shown {
        let mut shown = Shown::default();
        shown.follow(Some(&report_of(view)));
        shown
    }

    /// A [`Shown`] following a report whose body is `node`.
    fn holding_node(node: Node) -> Shown {
        holding(View {
            controls: vec![],
            live: Default::default(),
            title: "T".into(),
            body: node,
        })
    }

    /// The ids of the shown table's rows as drawn, or `None` when no table is shown.
    fn ids(shown: &Shown) -> Option<Vec<String>> {
        shown.table()?;
        Some(
            (0..shown.rows())
                .map(|i| shown.row(i).unwrap().id.clone())
                .collect(),
        )
    }

    fn tabbed() -> View {
        View {
            controls: vec![],
            live: Default::default(),
            title: "T".into(),
            body: Node::Stack {
                children: vec![
                    Node::Stats { items: vec![] },
                    Node::Tabs {
                        id: "by".into(),
                        tabs: vec![
                            Tab {
                                label: "Summary".into(),
                                count: None,
                                body: Node::Grid {
                                    columns: vec![],
                                    rows: vec![],
                                },
                            },
                            Tab {
                                label: "All".into(),
                                count: Some(3),
                                body: table("all", &["item10", "item2", "item1"]),
                            },
                        ],
                    },
                ],
            },
        }
    }

    #[test]
    fn the_shown_table_is_the_picked_tab_s() {
        let view = tabbed();
        let body = view.body.clone();
        let mut shown = holding(view);
        assert!(shown.table().is_none(), "Summary holds no table");
        assert_eq!(shown.rows(), 0);
        assert!(shown.fills(&body), "tabs fill");
        shown.pick("by", 1);
        assert_eq!(shown.table().unwrap().id, "all");
        assert!(shown.fills(&body));
        shown.pick("by", 9);
        assert_eq!(shown.tab("by", 2), 1, "a tab past the end is the last");
    }

    fn section(id: &str, open: bool, body: Node) -> Node {
        Node::Section {
            id: id.into(),
            title: id.into(),
            open,
            body: Box::new(body),
        }
    }

    #[test]
    fn a_folded_section_shows_no_table_until_it_is_unfolded() {
        for open in [true, false] {
            let view = View {
                controls: vec![],
                live: Default::default(),
                title: "T".into(),
                body: Node::Stack {
                    children: vec![
                        Node::Markdown { text: "x".into() },
                        section("more", open, table("t", &["a"])),
                    ],
                },
            };
            assert_eq!(
                view.check(
                    crucible_view::ViewKind::Run,
                    &crucible_view::Limits::DEFAULT
                ),
                Ok(())
            );
            let body = view.body.clone();
            let mut shown = holding(view);
            assert_eq!(shown.open("more", open), open);
            assert_eq!(shown.table().is_some(), open);
            assert_eq!(shown.fills(&body), open);
            shown.toggle("more", open);
            assert_eq!(shown.open("more", open), !open);
            assert_eq!(shown.table().is_some(), !open);
            assert_eq!(shown.fills(&body), !open);
            shown.toggle("more", open);
            assert_eq!(shown.open("more", open), open);
            assert_eq!(shown.table().is_some(), open);
            assert!(!shown.open("other", false), "another section keeps its own");
            assert!(shown.open("other", true));
        }
    }

    #[test]
    fn a_section_in_a_tab_shows_its_table_only_while_both_are_picked_and_open() {
        let mut shown = holding_node(Node::Tabs {
            id: "by".into(),
            tabs: vec![
                Tab {
                    label: "a".into(),
                    count: None,
                    body: Node::Stats { items: vec![] },
                },
                Tab {
                    label: "b".into(),
                    count: None,
                    body: section("rows", false, table("t", &["a"])),
                },
            ],
        });
        shown.toggle("rows", false);
        assert!(shown.table().is_none(), "tab a is picked");
        shown.pick("by", 1);
        assert_eq!(shown.table().unwrap().id, "t");
        assert!(
            shown.open("rows", false),
            "picking a tab keeps the sections"
        );
        shown.toggle("rows", false);
        assert!(shown.table().is_none());
    }

    #[test]
    fn unfolding_a_section_filters_and_sorts_its_table_again() {
        let report = |names: &[&str]| {
            report_of(View {
                controls: vec![],
                live: Default::default(),
                title: "T".into(),
                body: section("more", true, table("t", names)),
            })
        };
        let selection = Selection::Local("a".into());
        let first = report(&["b2", "a1", "b1"]);
        let mut scoped = Scoped::default();
        scoped.follow(Some(&selection), Some(&first));
        let shown = &mut scoped.shown;
        shown.filter = rune::TextEdit::new("b");
        shown.sort("name", SortDir::Ascending);
        assert_eq!(ids(shown).unwrap(), ["b1", "b2"]);

        shown.toggle("more", true);
        assert_eq!(ids(shown), None);
        let second = report(&["b9", "a1", "b3", "c1"]);
        scoped.follow(Some(&selection), Some(&second));
        assert_eq!(ids(&scoped.shown), None, "still folded");
        scoped.shown.toggle("more", true);
        assert_eq!(
            ids(&scoped.shown).unwrap(),
            ["b3", "b9"],
            "the filter and sort apply to the rows that came while it was folded"
        );
    }

    #[test]
    fn columns_and_content_size_to_their_content() {
        let shown = Shown::default();
        let content = [
            Node::Columns { children: vec![] },
            Node::Stats { items: vec![] },
            Node::Markdown { text: "x".into() },
            Node::Stack { children: vec![] },
            section("s", true, Node::Stats { items: vec![] }),
            section(
                "t",
                false,
                Node::Tabs {
                    id: "x".into(),
                    tabs: vec![],
                },
            ),
        ];
        for node in &content {
            assert!(!shown.fills(node), "{node:?}");
        }
        let filling = [
            table("t", &[]),
            Node::Tabs {
                id: "x".into(),
                tabs: vec![],
            },
            Node::Stack {
                children: vec![Node::Stats { items: vec![] }, table("t", &[])],
            },
            section(
                "s",
                true,
                Node::Stack {
                    children: vec![table("t", &[])],
                },
            ),
        ];
        for node in &filling {
            assert!(shown.fills(node), "{node:?}");
        }
        let columned = Node::Columns {
            children: vec![Node::Tabs {
                id: "x".into(),
                tabs: vec![],
            }],
        };
        assert!(
            !shown.fills(&columned),
            "tabs in columns size to their content"
        );
        assert!(shown.table_in(&columned).is_none());
    }

    #[test]
    fn a_sort_orders_rows_naturally_until_the_tab_changes() {
        let mut shown = holding(tabbed());
        shown.pick("by", 1);
        let ids = |shown: &Shown| -> Vec<String> {
            (0..3).map(|i| shown.row(i).unwrap().id.clone()).collect()
        };
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
        shown.sort("name", SortDir::Ascending);
        assert_eq!(ids(&shown), ["item1", "item2", "item10"]);
        shown.sort("name", SortDir::Descending);
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
        shown.sort("missing", SortDir::Ascending);
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
        assert!(shown.row(3).is_none());
        shown.sort("name", SortDir::Ascending);
        shown.pick("by", 1);
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
    }

    #[test]
    fn another_report_is_sorted_again_and_another_selection_starts_over() {
        let report = |names: &[&str]| {
            let mut view = tabbed();
            let Node::Stack { children } = &mut view.body else {
                panic!("tabbed is a stack");
            };
            let Node::Tabs { tabs, .. } = &mut children[1] else {
                panic!("tabbed holds tabs");
            };
            tabs[1].body = table("all", names);
            report_of(view)
        };
        let ids = |scoped: &Scoped<Selection>| ids(&scoped.shown).unwrap();
        let local = |name: &str| Selection::Local(name.into());
        let first = report(&["item10", "item2", "item1"]);
        let mut shown = Scoped::default();
        shown.follow(Some(&local("a")), Some(&first));
        shown.shown.pick("by", 1);
        shown.shown.sort("name", SortDir::Ascending);
        assert_eq!(ids(&shown), ["item1", "item2", "item10"]);
        shown.follow(Some(&local("a")), Some(&first));
        assert_eq!(ids(&shown), ["item1", "item2", "item10"]);

        let second = report(&["b9", "b10", "a1"]);
        shown.follow(Some(&local("a")), Some(&second));
        assert!(shown.shown.holds(Some(&second)));
        assert!(!shown.shown.holds(Some(&first)));
        assert!(!shown.shown.holds(None));
        assert_eq!(shown.shown.tab("by", 2), 1, "the picked tab is kept");
        assert_eq!(ids(&shown), ["a1", "b9", "b10"]);

        let failed = Arc::new(Report {
            asked: Default::default(),
            author: None,
            view: Err("torn".into()),
        });
        shown.follow(Some(&local("a")), Some(&failed));
        shown.follow(Some(&local("a")), Some(&second));
        assert_eq!(
            ids(&shown),
            ["a1", "b9", "b10"],
            "the sort outlasts a report that failed"
        );

        shown.shown.filter = rune::TextEdit::new("B");
        shown.follow(Some(&local("a")), Some(&first));
        assert_eq!(ids(&shown), Vec::<String>::new());
        shown.follow(Some(&local("a")), Some(&second));
        assert_eq!(ids(&shown), ["b9", "b10"], "and so does the filter");

        shown.shown.toggle("more", true);
        shown.follow(Some(&local("a")), Some(&first));
        assert!(
            !shown.shown.open("more", true),
            "another report keeps the sections"
        );
        shown.follow(Some(&local("b")), Some(&second));
        assert_eq!(
            shown.shown.tab("by", 2),
            0,
            "another selection starts on its first tab"
        );
        assert!(
            shown.shown.open("more", true),
            "and with its sections as they start"
        );
        shown.shown.pick("by", 1);
        assert_eq!(ids(&shown), ["b9", "b10", "a1"]);
        shown.follow(None, None);
        assert_eq!(shown.shown.tab("by", 2), 0);
        assert!(shown.shown.holds(None));
        assert!(!shown.shown.holds(Some(&second)));
    }

    #[test]
    fn a_number_column_sorts_by_figure_and_a_value_sorts_any_column() {
        let cells = |texts: &[(&str, Option<f64>)]| -> Vec<Row> {
            texts
                .iter()
                .enumerate()
                .map(|(i, (text, value))| Row {
                    id: format!("r{i}"),
                    cells: vec![Cell {
                        value: *value,
                        ..Cell::text(*text)
                    }],
                    detail: None,
                })
                .collect()
        };
        let column = |kind: ColumnKind| {
            vec![Column {
                id: "n".into(),
                title: "N".into(),
                kind,
                width: Default::default(),
            }]
        };
        let holding_rows = |kind: ColumnKind, rows: Vec<Row>| {
            holding_node(Node::Table {
                id: "t".into(),
                columns: column(kind),
                rows,
            })
        };
        let texts = |shown: &Shown| -> Vec<String> {
            (0..shown.rows())
                .map(|i| shown.row(i).unwrap().cells[0].text.clone())
                .collect()
        };

        let rows = cells(&[
            ("1.25", None),
            ("-3", None),
            ("n/a", None),
            ("1,200", None),
            ("1.5", None),
            ("", None),
        ]);
        let mut shown = holding_rows(ColumnKind::Number, rows.clone());
        shown.sort("n", SortDir::Ascending);
        assert_eq!(texts(&shown), ["-3", "1.25", "1.5", "1,200", "", "n/a"]);
        shown.sort("n", SortDir::Descending);
        assert_eq!(
            texts(&shown),
            ["1,200", "1.5", "1.25", "-3", "n/a", ""],
            "what is not a number stays last"
        );
        let mut shown = holding_rows(ColumnKind::Text, rows);
        shown.sort("n", SortDir::Ascending);
        assert_eq!(texts(&shown)[..2], ["", "-3"]);

        let rows = cells(&[
            ("3 min", Some(180.0)),
            ("45 s", Some(45.0)),
            ("2 h", Some(7200.0)),
            ("unknown", None),
        ]);
        let mut shown = holding_rows(ColumnKind::Text, rows);
        shown.sort("n", SortDir::Ascending);
        assert_eq!(texts(&shown), ["45 s", "3 min", "2 h", "unknown"]);
    }

    #[test]
    fn the_filter_keeps_rows_holding_every_word_in_any_case() {
        let rows: Vec<Row> = [
            ("DEMO-1", "affected", "grpc handler reachable"),
            ("DEMO-2", "not_affected", "gRPC absent from image"),
            ("DEMO-3", "affected", "Jinja2 sandbox escape"),
            ("DEMO-4", "needs_review", "caf\u{e9} \u{c9}clair"),
        ]
        .into_iter()
        .map(|(id, verdict, why)| Row {
            id: id.into(),
            cells: vec![Cell::text(verdict), Cell::text(why)],
            detail: None,
        })
        .collect();
        let columns: Vec<Column> = ["verdict", "why"]
            .into_iter()
            .map(|id| Column {
                id: id.into(),
                title: id.into(),
                kind: Default::default(),
                width: Default::default(),
            })
            .collect();
        let mut shown = holding_node(Node::Table {
            id: "t".into(),
            columns,
            rows,
        });
        let kept = |query: &str, shown: &mut Shown| -> Vec<String> {
            shown.filter = rune::TextEdit::new(query);
            shown.arrange();
            ids(shown).unwrap()
        };
        assert_eq!(kept("", &mut shown).len(), 4);
        assert_eq!(kept("GRPC", &mut shown), ["DEMO-1", "DEMO-2"]);
        assert_eq!(kept("grpc  affected", &mut shown), ["DEMO-1", "DEMO-2"]);
        assert_eq!(kept("grpc reachable", &mut shown), ["DEMO-1"]);
        assert_eq!(
            kept("demo-3", &mut shown),
            ["DEMO-3"],
            "the row's id counts"
        );
        assert_eq!(kept("\u{c9}CLAIR", &mut shown), ["DEMO-4"]);
        assert_eq!(kept("nothing", &mut shown), Vec::<String>::new());
        assert!(shown.row(0).is_none());

        shown.sort("why", SortDir::Descending);
        assert_eq!(kept("affected", &mut shown), ["DEMO-3", "DEMO-1", "DEMO-2"]);
        shown.pick("tabs", 1);
        assert_eq!(shown.filter.text, "");
        assert_eq!(shown.rows(), 4);
    }

    #[test]
    fn a_link_is_a_row_s_first_address_and_a_detail_needs_one_selected_row_with_one() {
        let cell = |text: &str, href: Option<&str>| Cell {
            href: href.map(str::to_string),
            ..Cell::text(text)
        };
        let rows = vec![
            Row {
                id: "a".into(),
                cells: vec![
                    cell("x", None),
                    cell("y", Some("run:task/one")),
                    cell("z", Some("https://example.com")),
                ],
                detail: Some("about a".into()),
            },
            Row {
                id: "b".into(),
                cells: vec![cell("x", None)],
                detail: None,
            },
        ];
        let mut shown = holding_node(Node::Table {
            id: "t".into(),
            columns: vec![],
            rows,
        });
        assert_eq!(shown.link("a").as_deref(), Some("run:task/one"));
        assert_eq!(shown.link("b"), None);
        assert_eq!(shown.link("missing"), None);
        assert!(shown.detail().is_none(), "nothing selected");
        shown.table.select("a");
        assert_eq!(shown.detail().map(|row| row.id.as_str()), Some("a"));
        shown.table.select("b");
        assert!(shown.detail().is_none(), "b has no detail");
        assert!(Shown::default().link("a").is_none());
        assert!(Shown::default().detail().is_none());
        assert!(Shown::default().csv().is_none());
    }

    fn controlled(controls: Vec<crucible_view::Control>) -> Arc<Report> {
        report_of(View {
            title: "T".into(),
            body: Node::Stats { items: vec![] },
            controls,
            live: Default::default(),
        })
    }

    #[test]
    fn controls_are_kept_for_a_selection_and_dropped_when_the_view_drops_them() {
        use crucible_view::Control;
        let lane = || Control::select("lane", "Lane", &[("all", "All"), ("go", "Go")], "all");
        let open = || Control::toggle("open", "Open", false);
        let cvss = || Control::range("cvss", "CVSS", [0.0, 10.0], 0.5, 0.0);
        let local = |name: &str| Selection::Local(name.into());
        let a = local("a");
        let first = controlled(vec![lane(), open(), cvss()]);
        let mut shown = Scoped::default();
        shown.follow(Some(&a), Some(&first));
        assert_eq!(shown.shown.controls().len(), 3);
        assert!(
            shown.shown.asked().params.is_empty(),
            "nothing set, nothing asked"
        );
        assert_eq!(shown.shown.value(&lane()), Param::Select("all".into()));
        assert!(!shown.shown.pending(), "built with what the controls hold");

        shown.shown.set("lane", Param::Select("go".into()));
        assert!(shown.shown.pending(), "built before the lane was set");
        let every = shown.shown.asked().params;
        assert_eq!(
            every.clone().into_iter().collect::<Vec<_>>(),
            [
                ("cvss".to_string(), Param::Range(0.0)),
                ("lane".to_string(), Param::Select("go".into())),
                ("open".to_string(), Param::Toggle(false)),
            ],
            "once one is set, every control is sent"
        );

        shown.shown.set("lane", Param::Select("rust".into()));
        assert_eq!(
            shown.shown.asked().params["lane"],
            Param::Select("all".into()),
            "a value the select does not offer falls back to its own"
        );
        shown.shown.set("open", Param::Range(3.0));
        assert_eq!(shown.shown.asked().params["open"], Param::Toggle(false));
        shown.shown.set("lane", Param::Select("go".into()));
        shown.shown.set("open", Param::Toggle(true));
        shown.shown.set("ghost", Param::Toggle(true));
        assert!(
            !shown.shown.asked().params.contains_key("ghost"),
            "only declared controls go"
        );

        shown.shown.slide("cvss", 7.3);
        assert_eq!(shown.shown.value(&cvss()), Param::Range(7.5));
        assert_eq!(
            shown.shown.asked().params["cvss"],
            Param::Range(0.0),
            "not while dragged"
        );
        shown.shown.release("other");
        assert_eq!(
            shown.shown.value(&cvss()),
            Param::Range(7.5),
            "another range's release"
        );
        shown.shown.release("cvss");
        assert_eq!(shown.shown.asked().params["cvss"], Param::Range(7.5));
        shown.shown.slide("cvss", 2.0);
        shown.shown.cancel_slide();
        assert_eq!(
            shown.shown.value(&cvss()),
            Param::Range(7.5),
            "a cancelled drag sets nothing"
        );
        shown.shown.slide("cvss", f64::NAN);
        assert_eq!(shown.shown.value(&cvss()), Param::Range(0.0));
        shown.shown.release("cvss");
        assert_eq!(shown.shown.asked().params["cvss"], Param::Range(0.0));
        shown.shown.set("cvss", Param::Range(1e300));
        assert_eq!(shown.shown.asked().params["cvss"], Param::Range(10.0));

        let refreshed = controlled(vec![lane(), open(), cvss()]);
        shown.follow(Some(&a), Some(&refreshed));
        assert_eq!(
            shown.shown.asked().params["lane"],
            Param::Select("go".into()),
            "kept"
        );

        let failed = Arc::new(Report {
            author: None,
            asked: Default::default(),
            view: Err("torn".into()),
        });
        shown.follow(Some(&a), Some(&failed));
        assert_eq!(
            shown.shown.controls().len(),
            3,
            "a failed view keeps the last controls"
        );
        assert!(!shown.shown.pending(), "a failed view waits on nothing");
        assert_eq!(
            shown.shown.asked().params["lane"],
            Param::Select("go".into())
        );

        shown.shown.choosing = Some("lane".into());
        shown.shown.slide("cvss", 4.0);
        let fewer = controlled(vec![open()]);
        shown.follow(Some(&a), Some(&fewer));
        assert_eq!(
            shown.shown.asked().params.into_iter().collect::<Vec<_>>(),
            [("open".to_string(), Param::Toggle(true))],
            "a control the view no longer has is dropped"
        );
        assert_eq!(shown.shown.choosing, None);
        shown.follow(Some(&a), Some(&first));
        assert_eq!(
            shown.shown.value(&lane()),
            Param::Select("all".into()),
            "and stays dropped"
        );
        assert_eq!(shown.shown.value(&cvss()), Param::Range(0.0));

        let none = controlled(vec![]);
        shown.follow(Some(&a), Some(&none));
        assert!(shown.shown.controls().is_empty());
        assert!(shown.shown.asked().params.is_empty());

        shown.follow(Some(&a), Some(&first));
        shown.shown.set("open", Param::Toggle(true));
        shown.follow(Some(&local("b")), Some(&first));
        assert!(
            shown.shown.asked().params.is_empty(),
            "another selection starts over"
        );
        assert_eq!(
            shown.shown.controls().len(),
            3,
            "with its own report's controls"
        );
        assert_eq!(shown.shown.value(&open()), Param::Toggle(false));
        shown.shown.set("open", Param::Toggle(true));
        shown.follow(None, None);
        assert!(shown.shown.controls().is_empty());
        assert!(
            shown.shown.asked().params.is_empty(),
            "and so does no selection"
        );
    }

    #[test]
    fn a_view_that_moves_its_controls_never_asks_again_on_its_own() {
        use crucible_view::Control;
        let a = Selection::Local("a".into());
        let low = || controlled(vec![Control::range("cvss", "CVSS", [0.0, 4.0], 1.0, 0.0)]);
        let high = || controlled(vec![Control::range("cvss", "CVSS", [5.0, 10.0], 1.0, 5.0)]);
        let lanes = |options: &[(&str, &str)]| {
            controlled(vec![
                Control::select("lane", "Lane", options, "all"),
                Control::toggle("open", "Open", false),
            ])
        };
        let mut shown = Scoped::default();
        shown.follow(Some(&a), Some(&low()));
        shown.shown.set("cvss", Param::Range(2.0));
        let asked = shown.shown.asked().params;
        assert_eq!(asked["cvss"], Param::Range(2.0));
        for _ in 0..3 {
            shown.follow(Some(&a), Some(&high()));
            assert!(
                shown.shown.asked().params.is_empty(),
                "a value the view no longer offers is not asked for"
            );
            shown.follow(Some(&a), Some(&low()));
            assert!(
                shown.shown.asked().params.is_empty(),
                "and is not asked again"
            );
        }

        shown.follow(Some(&a), Some(&lanes(&[("all", "All"), ("go", "Go")])));
        shown.shown.set("lane", Param::Select("go".into()));
        assert_eq!(shown.shown.asked().params.len(), 2);
        shown.follow(Some(&a), Some(&lanes(&[("all", "All")])));
        assert_eq!(
            shown.shown.asked().params.into_iter().collect::<Vec<_>>(),
            [("open".to_string(), Param::Toggle(false))]
        );
        shown.follow(Some(&a), Some(&lanes(&[("all", "All"), ("go", "Go")])));
        assert_eq!(
            shown.shown.asked().params.len(),
            1,
            "a view arriving never adds to what is asked"
        );
        assert_eq!(
            shown.shown.value(&Control::select(
                "lane",
                "Lane",
                &[("all", "All"), ("go", "Go")],
                "all"
            )),
            Param::Select("all".into()),
            "what is shown is what was asked"
        );

        shown.shown.set("ghost", Param::Toggle(true));
        assert!(!shown.shown.asked().params.contains_key("ghost"));
        shown.follow(
            Some(&a),
            Some(&lanes(&[("all", "All"), ("ghost", "Ghost")])),
        );
        shown.shown.set("open", Param::Toggle(true));
        assert_eq!(
            shown.shown.asked().params["lane"],
            Param::Select("all".into())
        );
    }

    fn csv_of(columns: &[&str], rows: &[&[&str]], cells: usize) -> String {
        let columns: Vec<Column> = columns
            .iter()
            .map(|title| Column {
                id: title.to_string(),
                title: title.to_string(),
                kind: Default::default(),
                width: Default::default(),
            })
            .collect();
        let rows: Vec<Row> = rows
            .iter()
            .enumerate()
            .map(|(i, cells)| Row {
                id: i.to_string(),
                cells: cells.iter().map(|text| Cell::text(*text)).collect(),
                detail: None,
            })
            .collect();
        crate::report::shown::csv(&columns, rows.iter(), cells)
    }

    #[test]
    fn csv_quotes_a_field_holding_a_comma_quote_or_line_break_and_doubles_its_quotes() {
        assert_eq!(
            csv_of(&["Ticket", "Why"], &[&["DEMO-1", "plain"]], 100),
            "Ticket,Why\r\nDEMO-1,plain\r\n"
        );
        assert_eq!(
            csv_of(
                &["a,b", "say \"hi\""],
                &[&["x,y", "line\nbreak"], &["cr\rhere", "\"\""]],
                100
            ),
            "\"a,b\",\"say \"\"hi\"\"\"\r\n\"x,y\",\"line\nbreak\"\r\n\"cr\rhere\",\"\"\"\"\"\"\r\n"
        );
        assert_eq!(csv_of(&["a", "b"], &[&["", ""]], 100), "a,b\r\n,\r\n");
        assert_eq!(csv_of(&[], &[], 100), "\r\n");
        assert_eq!(
            csv_of(&["ü"], &[&["naïve \u{1F600}"]], 100),
            "ü\r\nnaïve \u{1F600}\r\n"
        );
    }

    #[test]
    fn csv_guards_a_field_a_spreadsheet_would_read_as_a_formula() {
        let one = |text: &str| {
            csv_of(&["c"], &[&[text]], 100)
                .trim_start_matches("c\r\n")
                .to_string()
        };
        for (text, written) in [
            ("=SUM(A1:A9)", "'=SUM(A1:A9)\r\n"),
            ("+1+1", "'+1+1\r\n"),
            ("-1+1", "'-1+1\r\n"),
            ("@cmd", "'@cmd\r\n"),
            ("\t=1", "'\t=1\r\n"),
            ("-", "'-\r\n"),
            ("-inf", "'-inf\r\n"),
            ("-NaN", "'-NaN\r\n"),
            ("-1e400", "'-1e400\r\n"),
            (
                "=HYPERLINK(\"x\",\"y\")",
                "\"'=HYPERLINK(\"\"x\"\",\"\"y\"\")\"\r\n",
            ),
            ("-3 min", "'-3 min\r\n"),
            ("-3.5", "-3.5\r\n"),
            ("+7", "+7\r\n"),
            ("-1e5", "-1e5\r\n"),
            ("-.5", "-.5\r\n"),
            ("1=1", "1=1\r\n"),
            (" =1", " =1\r\n"),
            ("a@b", "a@b\r\n"),
        ] {
            assert_eq!(one(text), written, "{text:?}");
        }
        assert_eq!(
            csv_of(&["=title"], &[], 100),
            "'=title\r\n",
            "a column title is guarded too"
        );
    }

    #[test]
    fn csv_writes_at_most_the_cells_it_is_given_room_for() {
        let rows: Vec<&[&str]> = vec![&["1", "a"], &["2", "b"], &["3", "c"]];
        assert_eq!(csv_of(&["n", "s"], &rows, 6).lines().count(), 4);
        assert_eq!(csv_of(&["n", "s"], &rows, 5).lines().count(), 3);
        assert_eq!(csv_of(&["n", "s"], &rows, 2).lines().count(), 2);
        assert_eq!(csv_of(&["n", "s"], &rows, 1), "n,s\r\n");
        assert_eq!(csv_of(&["n", "s"], &rows, 0), "n,s\r\n");
        let bare: Vec<&[&str]> = vec![&[]; 3];
        assert_eq!(csv_of(&[], &bare, 2), "\r\n\r\n\r\n");
    }

    #[test]
    fn csv_follows_the_table_as_filtered_and_sorted() {
        let mut shown = holding_node(table("t", &["item10", "item2", "other", "item1"]));
        shown.filter = rune::TextEdit::new("item");
        shown.sort("name", SortDir::Descending);
        assert_eq!(shown.csv().unwrap(), "Name\r\nitem10\r\nitem2\r\nitem1\r\n");
    }
}

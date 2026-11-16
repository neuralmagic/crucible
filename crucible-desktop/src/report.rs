//! A pack's own view of a run: a plugin in the pack's `ui/` folder, given the captured files it
//! asks for, and what the Report pane keeps while it shows the result. The plugin is
//! `ui/<name>.wasm`, or `ui/<name>.wasm.b64` holding the same bytes as base64 text, which is the
//! form a controller's packs can carry, or `ui/<name>.js`, JavaScript source the app compiles
//! and runs itself. A task may also write a plugin during the run and capture it as
//! `<name>.view.wasm` or `<name>.view.js`; a local run's newest one is drawn instead of the
//! pack's.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crucible_view::{Column, File, Node, Request, Row, View};
use rune_kit::{SortDir, TableState};

use base64::Engine;

use crate::captured::SHOWN_BYTES;
use crate::pack::PackFiles;
use crate::plugin::{JsEngine, Limits, Loaded, Plugin, Script};

/// Where a pack keeps its view plugins.
const PLUGINS: &str = "ui";
/// How a plugin kept as base64 text is named.
const ENCODED: &str = ".wasm.b64";
/// How a plugin written in JavaScript is named.
const SCRIPT: &str = ".js";
/// How a plugin a task wrote during the run is named among its captured files.
const WRITTEN: [&str; 2] = [".view.wasm", ".view.js"];
/// Where a local run keeps the files its tasks captured, one directory per task.
const CAPTURED: &str = "state/files";

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The plugin's file name without `.wasm`, `.wasm.b64` or `.view.wasm`.
    pub name: String,
    /// The task that wrote the plugin during the run; `None` for the pack's own.
    pub author: Option<String>,
    pub view: Result<View, String>,
}

/// A live run's report, as the Report pane asks the live worker for it. Asked again when any of
/// it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub run: String,
    /// The plugin's path in the run's pack, and its length there.
    pub plugin: (String, usize),
    /// Each file the run captured, by key and listed size.
    pub listed: Vec<(String, u64)>,
    /// The run has stopped, so a captured file's copy on disk can stand in for it.
    pub settled: bool,
}

/// What the live worker built for a [`Wanted`].
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    pub wanted: Wanted,
    pub report: Report,
}

/// The plugin a controller's pack carries as text, base64 or JavaScript: its path and that
/// text. The first by path.
pub fn text_plugin(pack: &PackFiles) -> Option<(&str, &str)> {
    pack.files
        .iter()
        .filter(|file| {
            file.path
                .strip_prefix(PLUGINS)
                .and_then(|rest| rest.strip_prefix('/'))
                .is_some_and(|name| {
                    !name.contains('/') && (name.ends_with(ENCODED) || name.ends_with(SCRIPT))
                })
        })
        .filter_map(|file| Some((file.path.as_str(), file.text.as_deref()?)))
        .min_by_key(|(path, _)| *path)
}

/// A plugin's name from its path.
pub fn name_of(path: &str) -> String {
    let file = path.rsplit('/').next().unwrap_or(path);
    WRITTEN
        .iter()
        .chain(&[ENCODED, ".wasm", SCRIPT])
        .find_map(|suffix| file.strip_suffix(suffix))
        .unwrap_or(file)
        .to_string()
}

/// The engine JavaScript plugins run in: the module `CRUCIBLE_JS_ENGINE` names, else the one
/// `just desktop-js-engine` fetches into this crate's assets.
pub fn js_engine() -> Result<Arc<JsEngine>, String> {
    let path = std::env::var_os("CRUCIBLE_JS_ENGINE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/js-engine/plugin.wasm")
        });
    let wasm = fs::read(&path)
        .map_err(|e| format!("reading the JavaScript engine {}: {e}", path.display()))?;
    JsEngine::load(&wasm, Limits::DEFAULT)
        .map(Arc::new)
        .map_err(|fault| fault.to_string())
}

/// The plugin `bytes` hold, in the form `path` names, and the captured file paths it reads.
pub fn load(path: &str, bytes: &[u8]) -> Result<(Loaded, Vec<String>), String> {
    let plugin = if path.ends_with(SCRIPT) {
        Loaded::Js(Script::new(js_engine()?, &String::from_utf8_lossy(bytes)))
    } else {
        let decoded;
        let wasm = if path.ends_with(ENCODED) {
            let packed: String = String::from_utf8_lossy(bytes).split_whitespace().collect();
            decoded = base64::engine::general_purpose::STANDARD
                .decode(packed)
                .map_err(|e| format!("the plugin is not base64: {e}"))?;
            decoded.as_slice()
        } else {
            bytes
        };
        Loaded::Wasm(Plugin::load(wasm, Limits::DEFAULT).map_err(|f| f.to_string())?)
    };
    let inputs = plugin.inputs().map_err(|f| f.to_string())?;
    Ok((plugin, inputs))
}

/// The view `plugin` builds from `files`, the captured copies of its `inputs`.
pub fn run(plugin: &Loaded, inputs: &[String], files: Vec<File>) -> Result<View, String> {
    if files.is_empty() && !inputs.is_empty() {
        return Err(format!("No task has captured {} yet.", inputs.join(" or ")));
    }
    plugin
        .view(&Request { files })
        .map_err(|fault| fault.to_string())
}

/// A file's path, size and modification time.
type Mark = (PathBuf, u64, Option<SystemTime>);

fn mark(path: &Path) -> Option<Mark> {
    let meta = fs::metadata(path).ok()?;
    meta.is_file()
        .then(|| (path.to_path_buf(), meta.len(), meta.modified().ok()))
}

/// The pack's first plugin by name.
fn plugin_file(pack: &Path) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(pack.join(PLUGINS))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.ends_with(".wasm") || name.ends_with(ENCODED) || name.ends_with(SCRIPT)
                })
        })
        .collect();
    found.sort();
    found.into_iter().next()
}

/// The files below `dir`, at most `depth` directories down.
fn files_below(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth > 0 {
                files_below(&path, depth - 1, found);
            }
        } else {
            found.push(path);
        }
    }
}

/// The plugin a task of the local run captured most recently, and that task.
fn written_plugin(pack: &Path) -> Option<(PathBuf, String)> {
    let captured = pack.join(CAPTURED);
    let mut found = Vec::new();
    files_below(&captured, 3, &mut found);
    found
        .into_iter()
        .filter(|path| {
            let path = path.to_string_lossy();
            WRITTEN.iter().any(|suffix| path.ends_with(suffix))
        })
        .filter_map(|path| {
            let modified = fs::metadata(&path).ok()?.modified().ok()?;
            let task = path.strip_prefix(&captured).ok()?.iter().next()?;
            Some((modified, task.to_string_lossy().into_owned(), path))
        })
        .max()
        .map(|(_, task, path)| (path, task))
}

/// Each task directory's copy of each of `inputs`, in task then input order.
fn captured(pack: &Path, inputs: &[String]) -> Vec<(String, String, PathBuf)> {
    let mut tasks: Vec<PathBuf> = fs::read_dir(pack.join(CAPTURED))
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    tasks.sort();
    let mut files = Vec::new();
    for dir in tasks {
        let Some(task) = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        for input in inputs {
            let path = dir.join(input);
            if path.is_file() {
                files.push((task.clone(), input.clone(), path));
            }
        }
    }
    files
}

fn build(
    plugin: &Loaded,
    inputs: &[String],
    files: &[(String, String, PathBuf)],
) -> Result<View, String> {
    let mut read = Vec::new();
    for (task, input, path) in files {
        let size = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
        if size > SHOWN_BYTES {
            return Err(format!(
                "{task}/{input} is {size} bytes, more than a view reads."
            ));
        }
        let text = fs::read_to_string(path).map_err(|e| format!("reading {task}/{input}: {e}"))?;
        read.push(File {
            path: input.clone(),
            task: task.clone(),
            text,
        });
    }
    run(plugin, inputs, read)
}

/// A local pack's report, built again when its plugin or the files it reads change.
#[derive(Default)]
pub struct Reporter {
    plugin: Option<Mark>,
    author: Option<String>,
    loaded: Option<Result<(Loaded, Vec<String>), String>>,
    read: Vec<Mark>,
    report: Option<Report>,
}

impl Reporter {
    pub fn report(&self) -> Option<&Report> {
        self.report.as_ref()
    }

    /// Whether the report changed.
    pub fn refresh(&mut self, pack: &Path) -> bool {
        let (file, author) = match written_plugin(pack) {
            Some((path, task)) => (Some(path), Some(task)),
            None => (plugin_file(pack), None),
        };
        let plugin = file.and_then(|path| mark(&path));
        let reloaded = plugin != self.plugin;
        self.author = author;
        if reloaded {
            self.loaded = plugin.as_ref().map(|(path, ..)| {
                let bytes =
                    fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
                load(&path.to_string_lossy(), &bytes)
            });
            self.plugin = plugin;
        }
        let (Some((path, ..)), Some(loaded)) = (&self.plugin, &self.loaded) else {
            self.read.clear();
            return self.report.take().is_some();
        };
        let name = name_of(&path.to_string_lossy());
        let view = match loaded {
            Err(error) => {
                if !reloaded {
                    return false;
                }
                Err(error.clone())
            }
            Ok((plugin, inputs)) => {
                let files = captured(pack, inputs);
                let read: Vec<Mark> = files.iter().filter_map(|(.., path)| mark(path)).collect();
                if !reloaded && read == self.read && self.report.is_some() {
                    return false;
                }
                self.read = read;
                build(plugin, inputs, &files)
            }
        };
        let report = Some(Report {
            name,
            author: self.author.clone(),
            view,
        });
        let changed = report != self.report;
        self.report = report;
        changed
    }
}

/// The table a view is showing.
#[derive(Clone, Copy)]
pub struct Table<'a> {
    pub id: &'a str,
    pub columns: &'a [Column],
    pub rows: &'a [Row],
}

/// What the Report pane keeps between frames: the tab picked in each set of tabs, and the shown
/// table's sort and selection.
pub struct Shown {
    tabs: HashMap<String, usize>,
    pub table: TableState,
    order: Option<Order>,
}

/// A table's rows' indices in sorted order.
pub struct Order {
    table: String,
    rows: Vec<usize>,
}

impl Order {
    /// `table` sorted by `column`; `None` when it has no such column.
    pub fn of(table: &Table<'_>, column: &str, dir: SortDir) -> Option<Order> {
        let at = table.columns.iter().position(|c| c.id == column)?;
        let cell = |row: usize| {
            table
                .rows
                .get(row)
                .and_then(|row| row.cells.get(at))
                .map_or("", |cell| cell.text.as_str())
        };
        let mut rows: Vec<usize> = (0..table.rows.len()).collect();
        rows.sort_by(|&a, &b| dir.apply(rune_kit::natural_cmp(cell(a), cell(b))));
        Some(Order {
            table: table.id.to_string(),
            rows,
        })
    }
}

impl Default for Shown {
    fn default() -> Self {
        Shown {
            tabs: HashMap::new(),
            table: fresh_table(),
            order: None,
        }
    }
}

fn fresh_table() -> TableState {
    TableState::new().key("report-table")
}

impl Shown {
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
        self.order = None;
    }

    /// The one table `view` shows with the tabs as picked.
    pub fn table<'a>(&self, view: &'a View) -> Option<Table<'a>> {
        self.table_in(&view.body)
    }

    /// Whether `node` shows a table with the tabs as picked.
    pub fn shows_table(&self, node: &Node) -> bool {
        self.table_in(node).is_some()
    }

    fn table_in<'a>(&self, node: &'a Node) -> Option<Table<'a>> {
        match node {
            Node::Stack { children } => children.iter().find_map(|child| self.table_in(child)),
            Node::Tabs { id, tabs } => self.table_in(&tabs.get(self.tab(id, tabs.len()))?.body),
            Node::Table { id, columns, rows } => Some(Table { id, columns, rows }),
            Node::Stats { .. }
            | Node::Bars { .. }
            | Node::Plot { .. }
            | Node::Canvas { .. }
            | Node::Grid { .. }
            | Node::Text { .. } => None,
        }
    }

    /// Row `i` of `table` as sorted.
    pub fn row<'a>(&self, table: &Table<'a>, i: usize) -> Option<&'a Row> {
        let i = match &self.order {
            Some(order) if order.table == table.id && order.rows.len() == table.rows.len() => {
                *order.rows.get(i)?
            }
            _ => i,
        };
        table.rows.get(i)
    }

    /// Keep `order` for the shown table; `None` leaves the rows as they are.
    pub fn sort(&mut self, order: Option<Order>) {
        if order.is_some() {
            self.order = order;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use crucible_view::{Cell, Column, Node, Row, Tab, View};
    use rune_kit::SortDir;

    use crate::report::{Order, Reporter, Shown};

    fn example() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/report")
    }

    /// A pack directory holding the report example's plugin.
    fn pack() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("ui")).unwrap();
        fs::copy(
            example().join("ui/report.wasm"),
            dir.path().join("ui/report.wasm"),
        )
        .expect("build it with `just example-report-view`");
        dir
    }

    fn capture(pack: &Path, task: &str, lane: &str) {
        let dir = pack.join("state/files").join(task);
        fs::create_dir_all(&dir).unwrap();
        fs::copy(
            example().join(format!("inbox/{lane}.json")),
            dir.join("verdicts.json"),
        )
        .unwrap();
    }

    fn triaged(reporter: &Reporter) -> String {
        let view = reporter.report().unwrap().view.as_ref().unwrap();
        let Node::Stack { children } = &view.body else {
            panic!("the report is a stack");
        };
        let Node::Stats { items } = &children[0] else {
            panic!("the report starts with its stats");
        };
        items[0].value.clone()
    }

    #[test]
    fn a_controller_s_pack_carries_its_plugin_as_base64_text_in_ui() {
        let pack = crate::pack::PackFiles::new(
            "draft demo v1".into(),
            [
                ("ui/zeta.wasm.b64", Some("eg==")),
                ("ui/report.wasm.b64", Some("AA==")),
                ("ui/report/src/lib.rs.wasm.b64", Some("nested")),
                ("ui/alpha.wasm.b64", None),
                ("docs/ui/first.wasm.b64", Some("elsewhere")),
                ("ui/report.wasm", Some("binary as text")),
                ("workflow.star", Some("")),
            ]
            .map(|(path, text)| (path.to_string(), text.map(str::to_string))),
        );
        assert_eq!(
            crate::report::text_plugin(&pack),
            Some(("ui/report.wasm.b64", "AA=="))
        );
        let none = crate::pack::PackFiles::new("x".into(), [("ui/a.wasm".to_string(), None)]);
        assert_eq!(crate::report::text_plugin(&none), None);
        assert_eq!(crate::report::name_of("ui/report.wasm.b64"), "report");
        assert_eq!(crate::report::name_of("ui/report.wasm"), "report");
        assert_eq!(crate::report::name_of("report"), "report");
    }

    #[test]
    fn a_local_pack_s_plugin_may_be_base64_text() {
        use base64::Engine;
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("ui")).unwrap();
        let wasm = fs::read(example().join("ui/report.wasm"))
            .expect("build it with `just example-report-view`");
        fs::write(
            dir.path().join("ui/report.wasm.b64"),
            base64::engine::general_purpose::STANDARD.encode(wasm),
        )
        .unwrap();
        capture(dir.path(), "triage-go", "go");
        let mut reporter = Reporter::default();
        assert!(reporter.refresh(dir.path()));
        assert_eq!(reporter.report().unwrap().name, "report");
        assert_eq!(triaged(&reporter), "34");
    }

    /// The fenced `json` blocks of one of the view plugin skill's files, each with the word
    /// after `json` on its fence.
    fn skill_examples(file: &str) -> Vec<(String, String)> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../skills/write-a-view-plugin")
            .join(file);
        let text = fs::read_to_string(path).unwrap();
        let mut found = Vec::new();
        let mut open: Option<(String, String)> = None;
        for line in text.lines() {
            match (&mut open, line.strip_prefix("```")) {
                (None, Some(info)) if info.starts_with("json") => {
                    open = Some((info["json".len()..].trim().to_string(), String::new()));
                }
                (Some(_), Some(_)) => found.extend(open.take()),
                (Some((_, body)), None) => {
                    body.push_str(line);
                    body.push('\n');
                }
                _ => {}
            }
        }
        assert!(open.is_none(), "{file} ends inside an example");
        found
    }

    fn component_names(node: &Node, names: &mut std::collections::BTreeSet<&'static str>) {
        let name = match node {
            Node::Stack { children } => {
                for child in children {
                    component_names(child, names);
                }
                "stack"
            }
            Node::Tabs { tabs, .. } => {
                for tab in tabs {
                    component_names(&tab.body, names);
                }
                "tabs"
            }
            Node::Stats { .. } => "stats",
            Node::Bars { .. } => "bars",
            Node::Plot { .. } => "plot",
            Node::Canvas { .. } => "canvas",
            Node::Grid { .. } => "grid",
            Node::Table { .. } => "table",
            Node::Text { .. } => "text",
        };
        names.insert(name);
    }

    #[test]
    fn the_skill_s_examples_are_views_the_host_draws() {
        let limits = crucible_view::Limits::DEFAULT;
        let mut names = std::collections::BTreeSet::new();
        for (_, json) in skill_examples("components.md") {
            let body: Node = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}: {json}"));
            component_names(&body, &mut names);
            let view = View {
                title: "T".into(),
                body,
            };
            view.check(&limits)
                .unwrap_or_else(|e| panic!("{e}: {json}"));
        }
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            [
                "bars", "canvas", "grid", "plot", "stack", "stats", "table", "tabs", "text"
            ],
            "every component has an example"
        );

        let mut kinds = Vec::new();
        for (kind, json) in skill_examples("SKILL.md") {
            match kind.as_str() {
                "request" => {
                    let request: crucible_view::Request = serde_json::from_str(&json).unwrap();
                    assert_eq!(request.files.len(), 1);
                }
                "reply" => match serde_json::from_str(&json).unwrap() {
                    crucible_view::Reply::View(view) => view.check(&limits).unwrap(),
                    crucible_view::Reply::Error(error) => assert!(!error.is_empty()),
                },
                other => panic!("an example marked {other:?}"),
            }
            kinds.push(kind);
        }
        assert_eq!(kinds, ["request", "reply", "reply"]);
    }

    #[test]
    fn a_pack_without_a_plugin_has_no_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut reporter = Reporter::default();
        assert!(!reporter.refresh(dir.path()));
        assert!(reporter.report().is_none());
    }

    #[test]
    fn the_report_follows_the_captured_files_and_the_plugin() {
        let dir = pack();
        let mut reporter = Reporter::default();
        assert!(reporter.refresh(dir.path()));
        let report = reporter.report().unwrap();
        assert_eq!(report.name, "report");
        assert_eq!(
            report.view,
            Err("No task has captured verdicts.json yet.".into())
        );
        assert!(!reporter.refresh(dir.path()), "nothing changed");

        capture(dir.path(), "triage-go", "go");
        assert!(reporter.refresh(dir.path()));
        assert_eq!(triaged(&reporter), "34");
        assert!(!reporter.refresh(dir.path()), "nothing changed");

        capture(dir.path(), "triage-python", "python");
        assert!(reporter.refresh(dir.path()));
        assert_eq!(triaged(&reporter), "64");

        fs::write(dir.path().join("ui/report.wasm"), b"not wasm").unwrap();
        assert!(reporter.refresh(dir.path()));
        let error = reporter.report().unwrap().view.clone().unwrap_err();
        assert!(
            error.starts_with("not a plugin the host can load"),
            "{error}"
        );
        assert!(!reporter.refresh(dir.path()), "nothing changed");

        fs::remove_file(dir.path().join("ui/report.wasm")).unwrap();
        assert!(reporter.refresh(dir.path()));
        assert!(reporter.report().is_none());
    }

    #[test]
    fn a_plugin_a_task_captured_is_drawn_instead_of_the_pack_s_and_names_its_task() {
        let dir = pack();
        capture(dir.path(), "triage-go", "go");
        let mut reporter = Reporter::default();
        assert!(reporter.refresh(dir.path()));
        assert_eq!(reporter.report().unwrap().author, None);

        let written = dir.path().join("state/files/explain/out");
        fs::create_dir_all(&written).unwrap();
        fs::copy(
            example().join("ui/report.wasm"),
            written.join("mine.view.wasm"),
        )
        .unwrap();
        assert!(reporter.refresh(dir.path()));
        let report = reporter.report().unwrap();
        assert_eq!(
            (report.name.as_str(), report.author.as_deref()),
            ("mine", Some("explain"))
        );
        assert_eq!(triaged(&reporter), "34");
        assert!(!reporter.refresh(dir.path()), "nothing changed");

        fs::write(written.join("mine.view.wasm"), b"not wasm").unwrap();
        assert!(reporter.refresh(dir.path()));
        let report = reporter.report().unwrap();
        assert_eq!(report.author.as_deref(), Some("explain"));
        assert!(report.view.is_err());

        fs::remove_file(written.join("mine.view.wasm")).unwrap();
        assert!(reporter.refresh(dir.path()));
        assert_eq!(reporter.report().unwrap().author, None);
        assert_eq!(triaged(&reporter), "34");
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn a_javascript_plugin_in_the_pack_or_from_a_task_is_compiled_and_drawn() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("ui")).unwrap();
        let script = |title: &str| {
            format!(
                r#"export const inputs = ["verdicts.json"];
                   export function view({{ files }}) {{
                       const rows = files.flatMap((file) => JSON.parse(file.text));
                       return {{ title: "{title}", body: {{ stats: {{ items: [
                           {{ label: "Rows", value: String(rows.length) }} ] }} }} }};
                   }}"#
            )
        };
        fs::write(dir.path().join("ui/summary.js"), script("From the pack")).unwrap();
        capture(dir.path(), "triage-go", "go");
        let mut reporter = Reporter::default();
        assert!(reporter.refresh(dir.path()));
        let rows = |reporter: &Reporter| {
            let report = reporter.report().unwrap();
            let view = report.view.as_ref().unwrap();
            let Node::Stats { items } = &view.body else {
                panic!("the view is its stats");
            };
            (
                report.name.clone(),
                report.author.clone(),
                view.title.clone(),
                items[0].value.clone(),
            )
        };
        assert_eq!(
            rows(&reporter),
            ("summary".into(), None, "From the pack".into(), "34".into())
        );

        let written = dir.path().join("state/files/explain");
        fs::create_dir_all(&written).unwrap();
        fs::write(written.join("report.view.js"), script("From the task")).unwrap();
        assert!(reporter.refresh(dir.path()));
        assert_eq!(
            rows(&reporter),
            (
                "report".into(),
                Some("explain".into()),
                "From the task".into(),
                "34".into()
            )
        );

        fs::write(written.join("report.view.js"), "export const inputs = [;").unwrap();
        assert!(reporter.refresh(dir.path()));
        let error = reporter.report().unwrap().view.clone().unwrap_err();
        assert!(error.starts_with("the script does not compile:"), "{error}");
    }

    #[test]
    fn a_torn_file_is_the_report_s_error() {
        let dir = pack();
        let captured = dir.path().join("state/files/triage-go");
        fs::create_dir_all(&captured).unwrap();
        fs::write(captured.join("verdicts.json"), "[{").unwrap();
        let mut reporter = Reporter::default();
        assert!(reporter.refresh(dir.path()));
        let error = reporter.report().unwrap().view.clone().unwrap_err();
        assert!(
            error.starts_with("verdicts.json from triage-go:"),
            "{error}"
        );
    }

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
                })
                .collect(),
        }
    }

    fn tabbed() -> View {
        View {
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
        let mut shown = Shown::default();
        assert!(shown.table(&view).is_none(), "Summary holds no table");
        assert!(!shown.shows_table(&view.body));
        shown.pick("by", 1);
        assert_eq!(shown.table(&view).unwrap().id, "all");
        assert!(shown.shows_table(&view.body));
        shown.pick("by", 9);
        assert_eq!(shown.tab("by", 2), 1, "a tab past the end is the last");
    }

    #[test]
    fn a_sort_orders_rows_naturally_until_the_tab_changes() {
        let view = tabbed();
        let mut shown = Shown::default();
        shown.pick("by", 1);
        let table = shown.table(&view).unwrap();
        let ids = |shown: &Shown| -> Vec<&str> {
            (0..3)
                .map(|i| shown.row(&table, i).unwrap().id.as_str())
                .collect()
        };
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
        shown.sort(Order::of(&table, "name", SortDir::Ascending));
        assert_eq!(ids(&shown), ["item1", "item2", "item10"]);
        shown.sort(Order::of(&table, "name", SortDir::Descending));
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
        shown.sort(Order::of(&table, "missing", SortDir::Ascending));
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
        assert!(shown.row(&table, 3).is_none());
        shown.sort(Order::of(&table, "name", SortDir::Ascending));
        shown.pick("by", 1);
        assert_eq!(ids(&shown), ["item10", "item2", "item1"]);
    }
}

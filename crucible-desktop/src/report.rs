//! A pack's own view of a run: a plugin in the pack's `ui/` folder, given the captured files it
//! asks for, and what the Report pane keeps while it shows the result. The plugin is
//! `ui/<name>.wasm`, or `ui/<name>.wasm.b64` holding the same bytes as base64 text, which is the
//! form a controller's packs can carry, or `ui/<name>.js`, JavaScript source the app compiles
//! and runs itself. A task may also write a plugin during the run and capture it as
//! `<name>.view.wasm` or `<name>.view.js`; a local run's newest one is drawn instead of the
//! pack's.

pub mod shown;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crucible_view::{File, Param, Request, View};

use base64::Engine;

use crate::captured::Captured;
use crate::pack::PackFiles;
use crate::plugin::{Limits, Loaded, Module, Script};

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
/// The most bytes of captured files one view reads.
pub const READ_BYTES: u64 = 32 * 1024 * 1024;
/// The most bytes one captured file a view reads may hold.
pub const FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The task that wrote the plugin during the run; `None` for the pack's own.
    pub author: Option<String>,
    /// What the plugin was asked with.
    pub params: BTreeMap<String, Param>,
    pub view: Result<View, String>,
}

/// A live run's report, as the Report pane asks the live worker for it. Asked again when any of
/// it changes.
#[derive(Clone, Debug, PartialEq)]
pub struct Wanted {
    pub run: String,
    /// The plugin's path in the run's pack.
    pub plugin: String,
    /// Each file the run captured, by key and listed size.
    pub listed: Vec<(String, u64)>,
    /// The run has stopped, so a captured file's copy on disk can stand in for it.
    pub settled: bool,
    pub params: BTreeMap<String, Param>,
}

/// What the live worker built for a [`Wanted`].
#[derive(Clone, Debug, PartialEq)]
pub struct Built {
    pub wanted: Wanted,
    pub report: Arc<Report>,
}

impl Built {
    /// The pack's own plugin's `view` for `wanted`.
    pub fn new(wanted: Wanted, view: Result<View, String>) -> Built {
        Built {
            report: Arc::new(Report {
                author: None,
                params: wanted.params.clone(),
                view,
            }),
            wanted,
        }
    }
}

/// How a plugin's file holds it, from the file's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    Wasm,
    Encoded,
    Script,
}

impl Form {
    fn of(name: &str) -> Option<Form> {
        if name.ends_with(ENCODED) {
            Some(Form::Encoded)
        } else if name.ends_with(".wasm") {
            Some(Form::Wasm)
        } else if name.ends_with(SCRIPT) {
            Some(Form::Script)
        } else {
            None
        }
    }
}

/// The form of the plugin a pack keeps at `path`, a path inside the pack.
fn pack_plugin(path: &str) -> Option<Form> {
    path.strip_prefix(PLUGINS)?
        .strip_prefix('/')
        .filter(|name| !name.contains('/'))
        .and_then(Form::of)
}

/// The plugin a controller's pack carries as text, base64 or JavaScript: its path and that
/// text. The first by path.
pub fn text_plugin(pack: &PackFiles) -> Option<(&str, &str)> {
    pack.files
        .iter()
        .filter(|file| pack_plugin(&file.path).is_some_and(|form| form != Form::Wasm))
        .filter_map(|file| Some((file.path.as_str(), file.text.as_deref()?)))
        .min_by_key(|(path, _)| *path)
}

/// A loaded plugin and the captured file paths it reads, or why it did not load.
pub type Plugin = Result<(Loaded, Vec<String>), String>;

/// The plugins a worker has loaded: the JavaScript engine once a script needs it, and the last
/// plugin that loaded, kept while its path and bytes stay the same.
#[derive(Default)]
pub struct Plugins {
    engine: Option<Arc<Module>>,
    held: Option<(String, Vec<u8>, Arc<Plugin>)>,
}

impl Plugins {
    /// The plugin `bytes` hold, in the form `path` names.
    pub fn load(&mut self, path: &str, bytes: &[u8]) -> Arc<Plugin> {
        if let Some((held_path, held_bytes, plugin)) = &self.held
            && held_path == path
            && held_bytes == bytes
        {
            return Arc::clone(plugin);
        }
        let plugin = Arc::new(self.compile(path, bytes));
        if plugin.is_ok() {
            self.held = Some((path.to_string(), bytes.to_vec(), Arc::clone(&plugin)));
        }
        plugin
    }

    /// The plugin held now.
    #[cfg(test)]
    pub fn held(&self) -> Option<&Arc<Plugin>> {
        self.held.as_ref().map(|(_, _, plugin)| plugin)
    }

    fn engine(&mut self) -> Result<Arc<Module>, String> {
        if let Some(engine) = &self.engine {
            return Ok(Arc::clone(engine));
        }
        let engine = Script::engine()?;
        self.engine = Some(Arc::clone(&engine));
        Ok(engine)
    }

    fn compile(&mut self, path: &str, bytes: &[u8]) -> Plugin {
        let plugin = match Form::of(path) {
            Some(Form::Script) => {
                Loaded::Js(Script::new(self.engine()?, &String::from_utf8_lossy(bytes)))
            }
            Some(Form::Encoded) => {
                let packed: String = String::from_utf8_lossy(bytes).split_whitespace().collect();
                let wasm = base64::engine::general_purpose::STANDARD
                    .decode(packed)
                    .map_err(|e| format!("the plugin is not base64: {e}"))?;
                Loaded::Wasm(Module::load(&wasm, Limits::DEFAULT).map_err(|f| f.to_string())?)
            }
            Some(Form::Wasm) | None => {
                Loaded::Wasm(Module::load(bytes, Limits::DEFAULT).map_err(|f| f.to_string())?)
            }
        };
        let inputs = plugin.inputs().map_err(|f| f.to_string())?;
        Ok((plugin, inputs))
    }
}

/// Refuses `files`, each named with its size, when one holds more than [`FILE_BYTES`] or all
/// together more than [`READ_BYTES`].
pub fn admit<'a>(files: impl IntoIterator<Item = (&'a str, u64)>) -> Result<(), String> {
    let mut total = 0_u64;
    for (name, size) in files {
        if size > FILE_BYTES {
            return Err(format!("{name} is {size} bytes, more than a view reads."));
        }
        total = total.saturating_add(size);
    }
    readable(std::iter::once(total))
}

/// Refuses files of `sizes` that together hold more than [`READ_BYTES`].
fn readable(sizes: impl Iterator<Item = u64>) -> Result<(), String> {
    let total = sizes.fold(0_u64, u64::saturating_add);
    if total > READ_BYTES {
        return Err(format!(
            "The files this view reads hold {total} bytes, more than the {READ_BYTES} a view reads."
        ));
    }
    Ok(())
}

/// The view `plugin` builds from `files`, the captured copies of its `inputs`, with the
/// reader's `params`.
pub fn run(
    plugin: &Loaded,
    inputs: &[String],
    files: Vec<File>,
    params: BTreeMap<String, Param>,
) -> Result<View, String> {
    if files.is_empty() && !inputs.is_empty() {
        return Err(format!("No task has captured {} yet.", inputs.join(" or ")));
    }
    readable(files.iter().map(|file| file.text.len() as u64))?;
    plugin
        .view(&Request { files, params })
        .map_err(|fault| fault.to_string())
}

/// The task a run link names, or the fan-out and instance key for `task[key]`; `runs` says
/// whether the run has a task of a name.
pub fn task_named(runs: impl Fn(&str) -> bool, named: &str) -> Option<(&str, Option<&str>)> {
    if runs(named) {
        return Some((named, None));
    }
    let (task, key) = named.strip_suffix(']')?.split_once('[')?;
    runs(task).then_some((task, Some(key)))
}

/// The captured file a run link names as `<task>/<path>`, the task as a plugin is given it.
pub fn file_named<'a>(files: &'a [Captured], named: &str) -> Option<&'a Captured> {
    files.iter().find(|file| {
        named
            .strip_prefix(file.owner().as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            == Some(file.path.as_str())
    })
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
                .is_some_and(|name| Form::of(name).is_some())
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
fn captured(pack: &Path, inputs: &[String]) -> Vec<(String, String, Mark)> {
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
            if let Some(mark) = mark(&dir.join(input)) {
                files.push((task.clone(), input.clone(), mark));
            }
        }
    }
    files
}

fn build(
    plugin: &Loaded,
    inputs: &[String],
    files: &[(String, String, Mark)],
    params: &BTreeMap<String, Param>,
) -> Result<View, String> {
    let sized: Vec<(String, u64)> = files
        .iter()
        .map(|(task, input, (_, size, _))| (format!("{task}/{input}"), *size))
        .collect();
    admit(sized.iter().map(|(name, size)| (name.as_str(), *size)))?;
    let mut read = Vec::new();
    for (task, input, (path, ..)) in files {
        let text = fs::read_to_string(path).map_err(|e| format!("reading {task}/{input}: {e}"))?;
        read.push(File {
            path: input.clone(),
            task: task.clone(),
            text,
        });
    }
    run(plugin, inputs, read, params.clone())
}

/// A local pack's report, built again when its plugin, the files it reads or the reader's
/// params change.
#[derive(Default)]
pub struct Reporter {
    plugin: Option<Mark>,
    loaded: Option<Arc<Plugin>>,
    read: Vec<Mark>,
    report: Option<Arc<Report>>,
}

impl Reporter {
    /// The same allocation until the report changes.
    pub fn report(&self) -> Option<&Arc<Report>> {
        self.report.as_ref()
    }

    /// Builds `pack`'s report with `params`, loading its plugin through `plugins`; whether the
    /// report changed.
    pub fn refresh(
        &mut self,
        pack: &Path,
        params: &BTreeMap<String, Param>,
        plugins: &mut Plugins,
    ) -> bool {
        let (file, author) = match written_plugin(pack) {
            Some((path, task)) => (Some(path), Some(task)),
            None => (plugin_file(pack), None),
        };
        let plugin = file.and_then(|path| mark(&path));
        let reloaded = plugin != self.plugin;
        if reloaded {
            self.loaded = plugin.as_ref().map(|(path, ..)| match fs::read(path) {
                Ok(bytes) => plugins.load(&path.to_string_lossy(), &bytes),
                Err(e) => Arc::new(Err(format!("reading {}: {e}", path.display()))),
            });
            self.plugin = plugin;
        }
        let Some(loaded) = &self.loaded else {
            self.read.clear();
            return self.report.take().is_some();
        };
        let view = match &**loaded {
            Err(error) => {
                if !reloaded {
                    return false;
                }
                Err(error.clone())
            }
            Ok((plugin, inputs)) => {
                let files = captured(pack, inputs);
                let read: Vec<Mark> = files.iter().map(|(.., mark)| mark.clone()).collect();
                let asked = self.report.as_ref().map(|report| &report.params);
                if !reloaded && read == self.read && asked == Some(params) {
                    return false;
                }
                self.read = read;
                build(plugin, inputs, &files, params)
            }
        };
        let report = Report {
            author,
            params: params.clone(),
            view,
        };
        let changed = self.report.as_deref() != Some(&report);
        if changed {
            self.report = Some(Arc::new(report));
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use crucible_view::{Cell, Node, Param, View};

    use std::sync::Arc;

    use crate::captured::Captured;
    use crate::model::Params;
    use crate::report::shown::{Scoped, Shown};
    use crate::report::{
        FILE_BYTES, Plugins, READ_BYTES, Reporter, admit, file_named, readable, task_named,
    };

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

    /// Builds `pack`'s report with no params and nothing loaded before.
    fn refresh(reporter: &mut Reporter, pack: &Path) -> bool {
        reporter.refresh(pack, &Params::new(), &mut Plugins::default())
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
        assert!(refresh(&mut reporter, dir.path()));
        assert_eq!(triaged(&reporter), "34");
    }

    #[test]
    fn a_plugin_is_loaded_again_only_when_its_path_or_bytes_change() {
        let wasm = fs::read(example().join("ui/report.wasm"))
            .expect("build it with `just example-report-view`");
        let mut plugins = Plugins::default();
        let first = plugins.load("ui/report.wasm", &wasm);
        assert!(first.is_ok());
        assert!(Arc::ptr_eq(&first, &plugins.load("ui/report.wasm", &wasm)));
        let moved = plugins.load("ui/other.wasm", &wasm);
        assert!(!Arc::ptr_eq(&first, &moved), "another path");
        assert!(Arc::ptr_eq(&moved, &plugins.load("ui/other.wasm", &wasm)));
        let broken = plugins.load("ui/other.wasm", b"not wasm");
        assert!(broken.is_err());
        assert!(
            !Arc::ptr_eq(&broken, &plugins.load("ui/other.wasm", b"not wasm")),
            "a plugin that did not load is tried again"
        );
        assert!(
            Arc::ptr_eq(&moved, &plugins.load("ui/other.wasm", &wasm)),
            "and does not drop the last that did"
        );
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn the_javascript_engine_is_loaded_once_for_every_script() {
        let script = |title: &str| {
            format!(
                r#"export const inputs = [];
                   export function view() {{
                       return {{ title: "{title}", body: {{ stats: {{ items: [] }} }} }};
                   }}"#
            )
        };
        let mut plugins = Plugins::default();
        assert!(plugins.load("ui/a.js", script("A").as_bytes()).is_ok());
        let engine = Arc::clone(plugins.engine.as_ref().unwrap());
        assert!(plugins.load("ui/b.js", script("B").as_bytes()).is_ok());
        assert!(Arc::ptr_eq(&engine, plugins.engine.as_ref().unwrap()));
    }

    #[test]
    fn a_pack_without_a_plugin_has_no_report() {
        let dir = tempfile::tempdir().unwrap();
        let mut reporter = Reporter::default();
        assert!(!refresh(&mut reporter, dir.path()));
        assert!(reporter.report().is_none());
    }

    #[test]
    fn the_report_follows_the_captured_files_and_the_plugin() {
        let dir = pack();
        let mut reporter = Reporter::default();
        assert!(refresh(&mut reporter, dir.path()));
        let report = reporter.report().unwrap();
        assert_eq!(
            report.view,
            Err("No task has captured verdicts.json yet.".into())
        );
        assert!(!refresh(&mut reporter, dir.path()), "nothing changed");

        capture(dir.path(), "triage-go", "go");
        assert!(refresh(&mut reporter, dir.path()));
        assert_eq!(triaged(&reporter), "34");
        assert!(!refresh(&mut reporter, dir.path()), "nothing changed");

        capture(dir.path(), "triage-python", "python");
        assert!(refresh(&mut reporter, dir.path()));
        assert_eq!(triaged(&reporter), "64");

        fs::write(dir.path().join("ui/report.wasm"), b"not wasm").unwrap();
        assert!(refresh(&mut reporter, dir.path()));
        let error = reporter.report().unwrap().view.clone().unwrap_err();
        assert!(
            error.starts_with("not a plugin the host can load"),
            "{error}"
        );
        assert!(!refresh(&mut reporter, dir.path()), "nothing changed");

        fs::remove_file(dir.path().join("ui/report.wasm")).unwrap();
        assert!(refresh(&mut reporter, dir.path()));
        assert!(reporter.report().is_none());
    }

    #[test]
    fn a_plugin_a_task_captured_is_drawn_instead_of_the_pack_s_and_names_its_task() {
        let dir = pack();
        capture(dir.path(), "triage-go", "go");
        let mut reporter = Reporter::default();
        assert!(refresh(&mut reporter, dir.path()));
        assert_eq!(reporter.report().unwrap().author, None);

        let written = dir.path().join("state/files/explain/out");
        fs::create_dir_all(&written).unwrap();
        fs::copy(
            example().join("ui/report.wasm"),
            written.join("mine.view.wasm"),
        )
        .unwrap();
        assert!(refresh(&mut reporter, dir.path()));
        let report = reporter.report().unwrap();
        assert_eq!(report.author.as_deref(), Some("explain"));
        assert_eq!(triaged(&reporter), "34");
        assert!(!refresh(&mut reporter, dir.path()), "nothing changed");

        fs::write(written.join("mine.view.wasm"), b"not wasm").unwrap();
        assert!(refresh(&mut reporter, dir.path()));
        let report = reporter.report().unwrap();
        assert_eq!(report.author.as_deref(), Some("explain"));
        assert!(report.view.is_err());

        fs::remove_file(written.join("mine.view.wasm")).unwrap();
        assert!(refresh(&mut reporter, dir.path()));
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
        assert!(refresh(&mut reporter, dir.path()));
        let rows = |reporter: &Reporter| {
            let report = reporter.report().unwrap();
            let view = report.view.as_ref().unwrap();
            let Node::Stats { items } = &view.body else {
                panic!("the view is its stats");
            };
            (
                report.author.clone(),
                view.title.clone(),
                items[0].value.clone(),
            )
        };
        assert_eq!(rows(&reporter), (None, "From the pack".into(), "34".into()));

        let written = dir.path().join("state/files/explain");
        fs::create_dir_all(&written).unwrap();
        fs::write(written.join("report.view.js"), script("From the task")).unwrap();
        assert!(refresh(&mut reporter, dir.path()));
        assert_eq!(
            rows(&reporter),
            (Some("explain".into()), "From the task".into(), "34".into())
        );

        fs::write(written.join("report.view.js"), "export const inputs = [;").unwrap();
        assert!(refresh(&mut reporter, dir.path()));
        let error = reporter.report().unwrap().view.clone().unwrap_err();
        assert!(error.starts_with("the script does not compile:"), "{error}");
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn the_typescript_example_s_script_draws_its_view() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/report-ts");
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("ui")).unwrap();
        fs::copy(
            example.join("ui/report.js"),
            dir.path().join("ui/report.js"),
        )
        .unwrap();
        for lane in ["go", "python"] {
            let captured = dir
                .path()
                .join("state/files")
                .join(format!("triage-{lane}"));
            fs::create_dir_all(&captured).unwrap();
            fs::copy(
                example.join(format!("inbox/{lane}.json")),
                captured.join("verdicts.json"),
            )
            .unwrap();
        }
        let mut reporter = Reporter::default();
        assert!(refresh(&mut reporter, dir.path()));
        let report = Arc::clone(reporter.report().unwrap());
        let view = report.view.as_ref().unwrap();
        assert_eq!(
            view.title,
            "Triage: 19 to act on, 22 to spot-check, 23 to close"
        );
        assert_eq!(
            view.body.counts().into_keys().collect::<Vec<_>>(),
            [
                "canvas",
                "columns",
                "flow",
                "histogram",
                "markdown",
                "plot",
                "section",
                "stack",
                "stats",
                "table",
                "tabs",
                "text"
            ]
        );
        let mut shown = Shown::default();
        shown.follow(Some(&report));
        assert!(shown.table().is_none(), "the first tab holds no table");
        let Node::Stack { children } = &view.body else {
            panic!("the view is a stack");
        };
        let Node::Tabs { tabs, .. } = &children[1] else {
            panic!("the view's tabs follow its stats");
        };
        assert!(!shown.fills(&tabs[0].body), "Overview sizes to its content");
        let Node::Stack { children: overview } = &tabs[0].body else {
            panic!("Overview is a stack");
        };
        let [
            Node::Columns { .. },
            Node::Section {
                open: true, body, ..
            },
        ] = overview.as_slice()
        else {
            panic!("Overview is columns over an open section");
        };
        let Node::Canvas {
            width, commands, ..
        } = body.as_ref()
        else {
            panic!("the ticket map is a canvas");
        };
        assert_eq!(*width, 600.0);
        let count = |pick: fn(&crucible_view::Draw) -> bool| {
            commands.iter().filter(|command| pick(command)).count()
        };
        assert_eq!(
            count(|command| matches!(command, crucible_view::Draw::Hint { .. })),
            64,
            "a hint per ticket"
        );
        assert_eq!(
            count(|command| matches!(command, crucible_view::Draw::Translate(_))),
            64,
            "each cell is drawn from its own origin"
        );
        assert_eq!(
            count(|command| matches!(command, crucible_view::Draw::Save)),
            count(|command| matches!(command, crucible_view::Draw::Restore)),
        );
        assert!(commands.iter().any(|command| matches!(
            command,
            crucible_view::Draw::TextBaseline(crucible_view::TextBaseline::Middle)
        )));
        assert!(commands.iter().any(|command| matches!(
            command,
            crucible_view::Draw::FillText { text, max_width: Some(600.0), .. }
                if text.starts_with("One cell per ticket")
        )));
        assert!(commands.iter().any(|command| matches!(
            command,
            crucible_view::Draw::Hint { text, width: 58.0, .. } if text.starts_with("DEMO-")
        )));
        shown.pick("queue", 1);
        assert_eq!(shown.table().unwrap().id, "act");
        shown.toggle("ruler", true);
        assert!(!shown.open("ruler", true));
        assert_eq!(
            shown.table().unwrap().id,
            "act",
            "folding the ruler leaves the table"
        );
        shown.pick("queue", 3);
        let row = &shown.table().unwrap().rows[0];
        let detail = row.detail.as_deref().unwrap();
        assert!(detail.contains("](run:task/triage-"), "{detail}");
        assert!(detail.contains("](run:file/triage-"), "{detail}");
        let lane = row.cells[8].href.as_deref().unwrap();
        assert!(lane.starts_with("run:task/triage-"), "{lane}");
        let every = shown.table().unwrap();
        assert_eq!(every.columns[11].id, "opened");
        for row in every.rows {
            let cvss: f64 = row.cells[2].text.parse().unwrap();
            assert_eq!(row.cells[2].bar, Some(cvss / 10.0), "{}", row.id);
            let days = row.cells[3].value.unwrap() / 86_400.0;
            assert_eq!(row.cells[3], Cell::duration(days * 86_400.0), "{}", row.id);
            assert_eq!(
                row.cells[11],
                Cell::date(1_793_491_200.0 - days * 86_400.0),
                "{}",
                row.id
            );
        }
        let old = every.rows.iter().find(|row| row.id == "DEMO-101").unwrap();
        assert_eq!(
            (old.cells[3].text.as_str(), old.cells[11].text.as_str()),
            ("57 d", "2026-09-05")
        );

        let charts = tabs.iter().find(|tab| tab.label == "Charts").unwrap();
        let Node::Stack { children: charts } = &charts.body else {
            panic!("Charts is a stack");
        };
        let [
            Node::Columns { children: side },
            Node::Plot {
                series, y_scale, ..
            },
        ] = charts.as_slice()
        else {
            panic!("Charts is two charts side by side over a plot");
        };
        assert_eq!(*y_scale, crucible_view::Scale::Log);
        assert_eq!(series.len(), 8, "a series per package");
        assert!(
            series
                .iter()
                .all(|one| one.tone == crucible_view::Tone::Neutral),
            "past the six tones"
        );
        let [
            Node::Plot {
                x_scale: crucible_view::Scale::Time,
                series: lanes,
                ..
            },
            Node::Histogram {
                values,
                bins: Some(10),
                ..
            },
        ] = side.as_slice()
        else {
            panic!("a time plot beside a histogram");
        };
        assert_eq!(values.len(), 64);
        let opened: Vec<f64> = lanes
            .iter()
            .flat_map(|lane| &lane.points)
            .map(|point| point[0])
            .collect();
        assert_eq!(opened.len(), 64);
        let as_of = 1_793_491_200.0;
        assert!(
            opened
                .iter()
                .all(|at| *at < as_of && *at > as_of - 100.0 * 86_400.0),
            "{opened:?}"
        );

        let [
            crucible_view::Control::Select {
                id: lane,
                options,
                value,
                ..
            },
            crucible_view::Control::Range {
                id: cvss,
                min,
                max,
                value: least,
                ..
            },
        ] = view.controls.as_slice()
        else {
            panic!("a lane select and a CVSS range");
        };
        assert_eq!(
            (lane.as_str(), value.as_str(), cvss.as_str()),
            ("lane", "all", "cvss")
        );
        assert_eq!((*min, *max, *least), (0.0, 10.0, 0.0));
        assert_eq!(options.len(), 3, "all and each lane: {options:?}");
        let first_lane = options[1].value.clone();

        let mut shown = Scoped::default();
        let selection = crate::Selection::Local("report-ts".into());
        shown.follow(Some(&selection), Some(&report));
        shown.shown.set("lane", Param::Select(first_lane.clone()));
        let params = shown.shown.params().clone();
        assert_eq!(params["cvss"], Param::Range(0.0));
        let mut plugins = Plugins::default();
        assert!(reporter.refresh(dir.path(), &params, &mut plugins));
        let laned = reporter.report().unwrap();
        assert_eq!(laned.params, params);
        let view = laned.view.as_ref().unwrap();
        let triaged = |view: &View| {
            let Node::Stack { children } = &view.body else {
                panic!("the view is a stack");
            };
            let Node::Stats { items } = &children[0] else {
                panic!("the view starts with its stats");
            };
            items[0].value.parse::<usize>().unwrap()
        };
        let one_lane = triaged(view);
        assert!(one_lane > 0 && one_lane < 64, "{one_lane}");
        assert!(matches!(
            &view.controls[0],
            crucible_view::Control::Select { value, .. } if *value == first_lane
        ));

        shown.follow(Some(&selection), Some(laned));
        shown.shown.set("lane", Param::Select("all".into()));
        shown.shown.set("cvss", Param::Range(9.2));
        let params = shown.shown.params();
        assert_eq!(params["cvss"], Param::Range(9.0), "on the range's step");
        assert!(reporter.refresh(dir.path(), params, &mut plugins));
        let view = reporter.report().unwrap().view.as_ref().unwrap();
        let severe = triaged(view);
        assert!(severe > 0 && severe < 64, "{severe}");
        let Node::Stack { children } = &view.body else {
            panic!("the view is a stack");
        };
        let Node::Tabs { tabs, .. } = &children[1] else {
            panic!("the view's tabs follow its stats");
        };
        let all = tabs.iter().find(|tab| tab.label == "All").unwrap();
        let Node::Table { rows, .. } = &all.body else {
            panic!("All is a table");
        };
        assert!(
            rows.iter()
                .all(|row| row.cells[2].text.parse::<f64>().unwrap() >= 9.0)
        );

        shown.shown.set("cvss", Param::Range(10.0));
        shown.shown.set("lane", Param::Select(first_lane));
        assert!(reporter.refresh(dir.path(), shown.shown.params(), &mut plugins));
        let view = reporter.report().unwrap().view.as_ref().unwrap();
        assert_eq!(
            view.body,
            Node::Note {
                headline: "No ticket matches.".into(),
                detail: "Lower Min CVSS or pick another lane.".into(),
                tone: crucible_view::Tone::Warn
            }
        );
        assert_eq!(view.controls.len(), 2, "the controls stay to undo it");
    }

    #[test]
    fn a_torn_file_is_the_report_s_error() {
        let dir = pack();
        let captured = dir.path().join("state/files/triage-go");
        fs::create_dir_all(&captured).unwrap();
        fs::write(captured.join("verdicts.json"), "[{").unwrap();
        let mut reporter = Reporter::default();
        assert!(refresh(&mut reporter, dir.path()));
        let error = reporter.report().unwrap().view.clone().unwrap_err();
        assert!(
            error.starts_with("verdicts.json from triage-go:"),
            "{error}"
        );
    }

    #[test]
    fn a_view_reads_only_so_many_bytes_in_all() {
        assert_eq!(
            readable([READ_BYTES / 2, READ_BYTES / 2].into_iter()),
            Ok(())
        );
        let over = readable([READ_BYTES, 1].into_iter()).unwrap_err();
        assert!(over.contains("33554433 bytes"), "{over}");
        assert!(readable([u64::MAX, u64::MAX].into_iter()).is_err());
        let sized = |sizes: &[u64]| -> Vec<(String, u64)> {
            sizes
                .iter()
                .enumerate()
                .map(|(i, size)| (format!("t/{i}.json"), *size))
                .collect()
        };
        let admit = |files: Vec<(String, u64)>| {
            admit(files.iter().map(|(name, size)| (name.as_str(), *size)))
        };
        assert_eq!(admit(sized(&[FILE_BYTES, FILE_BYTES])), Ok(()));
        assert_eq!(
            admit(sized(&[1, FILE_BYTES + 1])),
            Err(format!(
                "t/1.json is {} bytes, more than a view reads.",
                FILE_BYTES + 1
            ))
        );
        let over = admit(sized(&[FILE_BYTES; 5])).unwrap_err();
        assert!(over.starts_with("The files this view reads hold"), "{over}");

        let dir = pack();
        for task in ["a", "b", "c", "d", "e"] {
            let captured = dir.path().join("state/files").join(task);
            fs::create_dir_all(&captured).unwrap();
            let file = fs::File::create(captured.join("verdicts.json")).unwrap();
            file.set_len(7 * 1024 * 1024).unwrap();
        }
        let mut reporter = Reporter::default();
        assert!(refresh(&mut reporter, dir.path()));
        let error = reporter.report().unwrap().view.clone().unwrap_err();
        assert!(
            error.starts_with("The files this view reads hold 36700160 bytes"),
            "{error}"
        );
    }

    #[test]
    fn a_run_link_names_a_task_an_instance_or_a_captured_file() {
        let runs = |name: &str| ["triage", "odd[name]"].contains(&name);
        assert_eq!(task_named(runs, "triage"), Some(("triage", None)));
        assert_eq!(task_named(runs, "triage[go]"), Some(("triage", Some("go"))));
        assert_eq!(task_named(runs, "odd[name]"), Some(("odd[name]", None)));
        assert_eq!(
            task_named(runs, "triage[a[b]]"),
            Some(("triage", Some("a[b]")))
        );
        for off in ["tri", "other[go]", "triage]", "triage[go", ""] {
            assert_eq!(task_named(runs, off), None, "{off}");
        }

        let file = |task: &str, instance: Option<&str>, path: &str, key: &str| Captured {
            task: task.into(),
            instance: instance.map(Into::into),
            path: path.into(),
            size: 1,
            key: key.into(),
        };
        let files = [
            file(
                "triage",
                Some("go"),
                "verdicts.json",
                "triage-0/verdicts.json",
            ),
            file(
                "triage",
                Some("py"),
                "evidence/probe.json",
                "triage-1/evidence/probe.json",
            ),
            file("report", None, "out.md", "report/out.md"),
        ];
        let key = |named: &str| file_named(&files, named).map(|file| file.key.as_str());
        assert_eq!(
            key("triage[go]/verdicts.json"),
            Some("triage-0/verdicts.json")
        );
        assert_eq!(
            key("triage[py]/evidence/probe.json"),
            Some("triage-1/evidence/probe.json")
        );
        assert_eq!(key("report/out.md"), Some("report/out.md"));
        for off in [
            "triage/verdicts.json",
            "triage[go]verdicts.json",
            "report/",
            "out.md",
        ] {
            assert_eq!(key(off), None, "{off}");
        }
    }

    #[test]
    fn the_reporter_builds_again_when_the_params_change_and_only_then() {
        let dir = pack();
        capture(dir.path(), "triage-go", "go");
        capture(dir.path(), "triage-python", "python");
        let mut reporter = Reporter::default();
        let mut plugins = Plugins::default();
        let none = Params::new();
        assert!(reporter.refresh(dir.path(), &none, &mut plugins));
        assert_eq!(triaged(&reporter), "64");
        assert!(reporter.report().unwrap().params.is_empty());
        assert!(
            !reporter.refresh(dir.path(), &none, &mut plugins),
            "no params is no change"
        );

        let go: Params = [("task".to_string(), Param::Select("triage-go".into()))].into();
        let before = Arc::clone(reporter.report().unwrap());
        assert!(reporter.refresh(dir.path(), &go, &mut plugins));
        assert_eq!(triaged(&reporter), "34");
        assert_eq!(reporter.report().unwrap().params, go);
        assert_eq!(
            before.params.len(),
            0,
            "the last report stands until the next is built"
        );
        assert!(
            !reporter.refresh(dir.path(), &go, &mut plugins),
            "the same params are no change"
        );

        fs::write(
            dir.path().join("state/files/triage-python/verdicts.json"),
            "[]",
        )
        .unwrap();
        reporter.refresh(dir.path(), &go, &mut plugins);
        assert_eq!(triaged(&reporter), "34");
        assert_eq!(
            reporter.report().unwrap().params,
            go,
            "a refresh keeps the params"
        );

        assert!(reporter.refresh(dir.path(), &none, &mut plugins));
        assert_eq!(
            triaged(&reporter),
            "34",
            "go alone has no python verdicts now"
        );
        assert!(reporter.report().unwrap().params.is_empty());
    }
}

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

use crate::captured::{Captured, SHOWN_BYTES};
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
    /// The plugin's path in the run's pack, and its length there.
    pub plugin: (String, usize),
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

/// The engine JavaScript plugins run in: the module `CRUCIBLE_JS_ENGINE` names, else the one
/// `just desktop-js-engine` fetches into this crate's assets.
pub fn js_engine() -> Result<Arc<Module>, String> {
    let path = std::env::var_os("CRUCIBLE_JS_ENGINE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/js-engine/plugin.wasm")
        });
    let wasm = fs::read(&path)
        .map_err(|e| format!("reading the JavaScript engine {}: {e}", path.display()))?;
    Module::load(&wasm, Limits::DEFAULT)
        .map(Arc::new)
        .map_err(|fault| fault.to_string())
}

/// The plugin `bytes` hold, in the form `path` names, and the captured file paths it reads.
pub fn load(path: &str, bytes: &[u8]) -> Result<(Loaded, Vec<String>), String> {
    let plugin = match Form::of(path) {
        Some(Form::Script) => {
            Loaded::Js(Script::new(js_engine()?, &String::from_utf8_lossy(bytes)))
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

/// Refuses `files`, each named with its size, when one holds more than [`SHOWN_BYTES`] or all
/// together more than [`READ_BYTES`].
pub fn admit(files: &[(String, u64)]) -> Result<(), String> {
    if let Some((name, size)) = files.iter().find(|(_, size)| *size > SHOWN_BYTES) {
        return Err(format!("{name} is {size} bytes, more than a view reads."));
    }
    readable(files.iter().map(|(_, size)| *size))
}

/// Refuses files of `sizes` that together hold more than [`READ_BYTES`].
pub fn readable(sizes: impl Iterator<Item = u64>) -> Result<(), String> {
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
    params: &BTreeMap<String, Param>,
) -> Result<View, String> {
    let sized: Vec<(String, u64)> = files
        .iter()
        .map(|(task, input, path)| {
            let size = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
            (format!("{task}/{input}"), size)
        })
        .collect();
    admit(&sized)?;
    let mut read = Vec::new();
    for (task, input, path) in files {
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
    loaded: Option<Result<(Loaded, Vec<String>), String>>,
    read: Vec<Mark>,
    params: BTreeMap<String, Param>,
    report: Option<Arc<Report>>,
}

impl Reporter {
    /// The same allocation until the report changes.
    pub fn report(&self) -> Option<&Arc<Report>> {
        self.report.as_ref()
    }

    /// What the next [`Reporter::refresh`] asks the plugin with; whether that changed.
    pub fn ask(&mut self, params: BTreeMap<String, Param>) -> bool {
        let changed = params != self.params;
        self.params = params;
        changed
    }

    /// Whether the report changed.
    pub fn refresh(&mut self, pack: &Path) -> bool {
        let (file, author) = match written_plugin(pack) {
            Some((path, task)) => (Some(path), Some(task)),
            None => (plugin_file(pack), None),
        };
        let plugin = file.and_then(|path| mark(&path));
        let reloaded = plugin != self.plugin;
        if reloaded {
            self.loaded = plugin.as_ref().map(|(path, ..)| {
                let bytes =
                    fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
                load(&path.to_string_lossy(), &bytes)
            });
            self.plugin = plugin;
        }
        let Some(loaded) = &self.loaded else {
            self.read.clear();
            return self.report.take().is_some();
        };
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
                let asked = self.report.as_ref().map(|report| &report.params);
                if !reloaded && read == self.read && asked == Some(&self.params) {
                    return false;
                }
                self.read = read;
                build(plugin, inputs, &files, &self.params)
            }
        };
        let report = Report {
            author,
            params: self.params.clone(),
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

    use crate::captured::{Captured, SHOWN_BYTES};
    use crate::report::shown::{Scoped, Shown};
    use crate::report::{READ_BYTES, Reporter, admit, file_named, readable, task_named};

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

    /// The name `value` is written under.
    fn tag(value: &impl serde::Serialize) -> String {
        let written = serde_json::to_value(value).unwrap();
        let mut keys = written.as_object().unwrap().keys();
        let name = keys.next().unwrap().clone();
        assert_eq!(keys.next(), None);
        name
    }

    fn component_names(node: &Node, names: &mut std::collections::BTreeSet<&'static str>) {
        assert_eq!(tag(node), node.name());
        names.insert(node.name());
        for child in node.children() {
            component_names(child, names);
        }
    }

    #[test]
    fn the_skill_s_examples_are_views_the_host_draws() {
        let limits = crucible_view::Limits::DEFAULT;
        let mut names = std::collections::BTreeSet::new();
        let mut kinds = std::collections::BTreeSet::new();
        for (word, json) in skill_examples("components.md") {
            if word == "controls" {
                let controls: Vec<crucible_view::Control> =
                    serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}: {json}"));
                for control in &controls {
                    assert_eq!(tag(control), control.kind(), "{json}");
                }
                kinds.extend(controls.iter().map(crucible_view::Control::kind));
                let view = View {
                    title: "T".into(),
                    body: Node::Stats { items: vec![] },
                    controls,
                };
                view.check(&limits)
                    .unwrap_or_else(|e| panic!("{e}: {json}"));
                continue;
            }
            let body: Node = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{e}: {json}"));
            component_names(&body, &mut names);
            let view = View {
                controls: vec![],
                title: "T".into(),
                body,
            };
            view.check(&limits)
                .unwrap_or_else(|e| panic!("{e}: {json}"));
        }
        let typed = include_str!("../../crucible-view/js/crucible-view.d.ts");
        for name in names.iter().chain(&kinds) {
            assert!(
                typed.contains(&format!("{name}: {{")),
                "the types name {name}"
            );
        }
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            [
                "bars",
                "canvas",
                "columns",
                "flow",
                "grid",
                "heatmap",
                "histogram",
                "markdown",
                "mermaid",
                "meter",
                "note",
                "plot",
                "section",
                "stack",
                "stats",
                "table",
                "tabs",
                "text"
            ],
            "every component has an example"
        );
        assert_eq!(
            kinds.into_iter().collect::<Vec<_>>(),
            ["range", "select", "toggle"],
            "every control has an example"
        );

        let mut kinds = Vec::new();
        for (kind, json) in skill_examples("SKILL.md") {
            match kind.as_str() {
                "request" => {
                    let request: crucible_view::Request = serde_json::from_str(&json).unwrap();
                    assert_eq!(request.files.len(), 1);
                    assert_eq!(request.select("lane"), Some("go"));
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
        assert_eq!(report.author.as_deref(), Some("explain"));
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
                report.author.clone(),
                view.title.clone(),
                items[0].value.clone(),
            )
        };
        assert_eq!(rows(&reporter), (None, "From the pack".into(), "34".into()));

        let written = dir.path().join("state/files/explain");
        fs::create_dir_all(&written).unwrap();
        fs::write(written.join("report.view.js"), script("From the task")).unwrap();
        assert!(reporter.refresh(dir.path()));
        assert_eq!(
            rows(&reporter),
            (Some("explain".into()), "From the task".into(), "34".into())
        );

        fs::write(written.join("report.view.js"), "export const inputs = [;").unwrap();
        assert!(reporter.refresh(dir.path()));
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
        assert!(reporter.refresh(dir.path()));
        let report = Arc::clone(reporter.report().unwrap());
        let view = report.view.as_ref().unwrap();
        assert_eq!(
            view.title,
            "Triage: 19 to act on, 22 to spot-check, 23 to close"
        );
        let mut names = std::collections::BTreeSet::new();
        component_names(&view.body, &mut names);
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
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
        let params = shown.params(Some(&selection));
        assert_eq!(params["cvss"], Param::Range(0.0));
        assert!(reporter.ask(params.clone()));
        assert!(reporter.refresh(dir.path()));
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
        let params = shown.params(Some(&selection));
        assert_eq!(params["cvss"], Param::Range(9.0), "on the range's step");
        assert!(reporter.ask(params));
        assert!(reporter.refresh(dir.path()));
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
        assert!(reporter.ask(shown.params(Some(&selection))));
        assert!(reporter.refresh(dir.path()));
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
        assert!(reporter.refresh(dir.path()));
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
        assert_eq!(admit(&sized(&[SHOWN_BYTES, SHOWN_BYTES])), Ok(()));
        assert_eq!(
            admit(&sized(&[1, SHOWN_BYTES + 1])),
            Err(format!(
                "t/1.json is {} bytes, more than a view reads.",
                SHOWN_BYTES + 1
            ))
        );
        let over = admit(&sized(&[SHOWN_BYTES; 5])).unwrap_err();
        assert!(over.starts_with("The files this view reads hold"), "{over}");

        let dir = pack();
        for task in ["a", "b", "c", "d", "e"] {
            let captured = dir.path().join("state/files").join(task);
            fs::create_dir_all(&captured).unwrap();
            let file = fs::File::create(captured.join("verdicts.json")).unwrap();
            file.set_len(7 * 1024 * 1024).unwrap();
        }
        let mut reporter = Reporter::default();
        assert!(reporter.refresh(dir.path()));
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
        assert!(reporter.refresh(dir.path()));
        assert_eq!(triaged(&reporter), "64");
        assert!(reporter.report().unwrap().params.is_empty());
        assert!(!reporter.ask(Default::default()), "no params is no change");
        assert!(!reporter.refresh(dir.path()));

        let go: std::collections::BTreeMap<String, Param> =
            [("task".to_string(), Param::Select("triage-go".into()))].into();
        assert!(reporter.ask(go.clone()));
        let before = Arc::clone(reporter.report().unwrap());
        assert!(reporter.refresh(dir.path()));
        assert_eq!(triaged(&reporter), "34");
        assert_eq!(reporter.report().unwrap().params, go);
        assert_eq!(
            before.params.len(),
            0,
            "the last report stands until the next is built"
        );
        assert!(!reporter.ask(go.clone()), "the same params are no change");
        assert!(!reporter.refresh(dir.path()), "nothing changed");

        fs::write(
            dir.path().join("state/files/triage-python/verdicts.json"),
            "[]",
        )
        .unwrap();
        reporter.refresh(dir.path());
        assert_eq!(triaged(&reporter), "34");
        assert_eq!(
            reporter.report().unwrap().params,
            go,
            "a refresh keeps the params"
        );

        assert!(reporter.ask(Default::default()));
        assert!(reporter.refresh(dir.path()));
        assert_eq!(
            triaged(&reporter),
            "34",
            "go alone has no python verdicts now"
        );
        assert!(reporter.report().unwrap().params.is_empty());
    }
}

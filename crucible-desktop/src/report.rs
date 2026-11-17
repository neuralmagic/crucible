//! A pack's own view of a run: a plugin in the pack's `ui/` folder, given the captured files it
//! asks for, and what the Report pane keeps while it shows the result. The plugin is
//! `ui/<name>.wasm`, or `ui/<name>.wasm.b64` holding the same bytes as base64 text, which is the
//! form a controller's packs can carry, or `ui/<name>.js`, JavaScript source the app compiles
//! and runs itself. A task may also write a plugin during the run and capture it as
//! `<name>.view.wasm` or `<name>.view.js`; a local run's newest one is drawn instead of the
//! pack's.
//!
//! A plugin may also draw the launches of its playbook on the runs page, from what each launch's
//! newest run captured.

pub mod play;
pub mod shown;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use crucible_view::{Event, File, Param, Request, Run, RunsRequest, Status, View};

use base64::Engine;

use crate::captured::Captured;
use crate::live::{LaunchStatus, LivePlaybook};
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
/// The most launches one runs view reads.
pub const RUNS: usize = 50;

/// What a plugin is asked with besides its files: the reader's params and, for a live view,
/// the state its newest view returned and the event that asks. Two are the same ask when their
/// params and turn are.
#[derive(Clone, Debug, Default)]
pub struct Asked {
    pub params: BTreeMap<String, Param>,
    /// How many events have asked; each one is a new ask.
    pub turn: u64,
    pub state: Option<String>,
    pub event: Option<Event>,
}

impl PartialEq for Asked {
    fn eq(&self, other: &Asked) -> bool {
        self.params == other.params && self.turn == other.turn
    }
}

impl Asked {
    /// The request for one run's `files`.
    pub fn request(&self, files: Vec<File>) -> Request {
        Request {
            files,
            params: self.params.clone(),
            state: self.state.clone(),
            event: self.event.clone(),
        }
    }

    /// The request for `runs`, of `total` launches the filters keep.
    pub fn runs_request(&self, runs: Vec<Run>, total: usize) -> RunsRequest {
        RunsRequest {
            runs,
            total,
            params: self.params.clone(),
            state: self.state.clone(),
            event: self.event.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// The task that wrote the plugin during the run; `None` for the pack's own.
    pub author: Option<String>,
    pub asked: Asked,
    pub view: Result<View, String>,
    /// The launches a runs view was asked to read and could not.
    pub unread: Vec<Unread>,
}

impl Report {
    /// The pack's own plugin's view, built for `asked`.
    pub fn pack(asked: Asked, view: Result<View, String>) -> Report {
        Report {
            author: None,
            asked,
            view,
            unread: Vec::new(),
        }
    }
}

/// A launch a runs view could not read, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Unread {
    pub launch: String,
    pub why: String,
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
    pub asked: Asked,
}

impl Wanted {
    /// Whether a report for `other` reads the same files: all but what it is asked with is
    /// the same.
    pub fn reads_as(&self, other: &Wanted) -> bool {
        (&self.run, &self.plugin, &self.listed, self.settled)
            == (&other.run, &other.plugin, &other.listed, other.settled)
    }
}

/// A playbook's runs view, as the runs page asks the live worker for it. Asked again when any
/// of it changes.
#[derive(Clone, Debug, PartialEq)]
pub struct WantedRuns {
    pub playbook: String,
    /// The plugin's path in the newest launch's pack.
    pub plugin: String,
    /// The launches to read, newest first, at most [`RUNS`].
    pub launches: Vec<Launched>,
    /// How many launches the reader's filters keep.
    pub total: usize,
    pub asked: Asked,
}

impl WantedRuns {
    /// The runs view of `playbook`'s `launches`, newest first, drawn by the plugin at `plugin`:
    /// the newest [`RUNS`] of them.
    pub fn of(
        playbook: &str,
        plugin: &str,
        launches: &[&LivePlaybook],
        asked: Asked,
    ) -> WantedRuns {
        WantedRuns {
            playbook: playbook.to_string(),
            plugin: plugin.to_string(),
            total: launches.len(),
            launches: launches
                .iter()
                .take(RUNS)
                .map(|launch| Launched::of(launch))
                .collect(),
            asked,
        }
    }
}

/// The runs view last asked of the live worker, and the one it last built for that ask.
#[derive(Default)]
pub struct RunsAsk {
    sent: Option<WantedRuns>,
    built: Option<Arc<Built<WantedRuns>>>,
}

impl RunsAsk {
    /// Notes `wanted` as what is asked now, and gives it back when the worker is to be asked:
    /// when it is not what was asked last.
    pub fn ask(&mut self, wanted: Option<WantedRuns>) -> Option<&WantedRuns> {
        if wanted == self.sent {
            return None;
        }
        self.sent = wanted;
        self.sent.as_ref()
    }

    /// Keeps `built` when it answers what was asked last.
    pub fn answer(&mut self, built: Arc<Built<WantedRuns>>) {
        if Some(&built.wanted) == self.sent.as_ref() {
            self.built = Some(built);
        }
    }

    /// The view built for `playbook`, when its plugin draws one.
    pub fn view(&self, playbook: &str) -> Option<&Arc<Report>> {
        self.built
            .as_deref()
            .filter(|built| built.wanted.playbook == playbook)
            .and_then(|built| built.report.as_ref())
    }
}

/// One launch a runs view is asked with, as the launch list has it.
#[derive(Clone, Debug, PartialEq)]
pub struct Launched {
    pub key: String,
    pub status: Status,
    /// How many runs it has dispatched.
    pub runs: i64,
    /// When it was launched, in Unix seconds.
    pub started: i64,
    /// What it cost, once it has stopped. Left out while it runs, so that a rising cost does
    /// not make every poll a new ask and read every running launch again.
    pub cost: Option<f64>,
    pub origin: String,
}

impl Launched {
    pub fn of(launch: &LivePlaybook) -> Launched {
        let status = match launch.status {
            LaunchStatus::Running => Status::Running,
            LaunchStatus::Done => Status::Done,
            LaunchStatus::Error => Status::Error,
            LaunchStatus::Cancelled => Status::Cancelled,
            LaunchStatus::New | LaunchStatus::Other(_) => Status::Unfinished,
        };
        Launched {
            key: launch.key.clone(),
            status,
            runs: launch.runs,
            started: launch.landed,
            cost: launch.cost.filter(|_| status != Status::Running),
            origin: launch.origin.clone(),
        }
    }

    /// Whether it has stopped, so what its runs captured no longer changes.
    pub fn settled(&self) -> bool {
        self.status != Status::Running
    }
}

/// What each launch of the last runs view was read as, kept so a launch that has stopped and
/// not changed is not read again, and none is for an event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gathered {
    /// The paths each launch was read for.
    pub inputs: Vec<String>,
    /// The launches the view was asked to read.
    pub launches: Vec<Launched>,
    /// Those read, which leaves out the ones past the bytes a view reads.
    pub runs: Vec<(Launched, Run)>,
    /// Those that could not be read.
    pub unread: Vec<Unread>,
}

impl Gathered {
    /// Whether this is what reading `launches` for `inputs` gave, as they were then.
    pub fn read(&self, inputs: &[String], launches: &[Launched]) -> bool {
        self.inputs == inputs && self.launches == launches
    }

    /// What `launch` was read as for `inputs`, when it has stopped and is as it was then.
    pub fn kept(&self, inputs: &[String], launch: &Launched) -> Option<&Run> {
        if self.inputs != inputs || !launch.settled() {
            return None;
        }
        self.runs
            .iter()
            .find_map(|(read, run)| (read == launch).then_some(run))
    }
}

/// `held` bytes of launches' files with `run`'s added, when that is at most [`READ_BYTES`].
pub fn with_run(held: u64, run: &Run) -> Option<u64> {
    let total = run
        .files
        .iter()
        .map(|file| file.text.len() as u64)
        .fold(held, u64::saturating_add);
    (total <= READ_BYTES).then_some(total)
}

/// What the live worker built for a [`Wanted`] or a [`WantedRuns`].
#[derive(Clone, Debug, PartialEq)]
pub struct Built<W> {
    pub wanted: W,
    /// `None` when the plugin draws no view of that kind.
    pub report: Option<Arc<Report>>,
}

impl<W> Built<W> {
    pub fn new(wanted: W, report: Option<Report>) -> Built<W> {
        Built {
            wanted,
            report: report.map(Arc::new),
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

/// The plugin file at `path` holding `bytes`, as the path and text a controller's pack would
/// carry it under: a `.wasm` module as base64 text, anything else as UTF-8. `None` for a file
/// that is no plugin or not text.
pub fn stand_in(path: &str, bytes: Vec<u8>) -> Option<(String, String)> {
    match Form::of(path)? {
        Form::Wasm => Some((
            format!("{path}.b64"),
            base64::engine::general_purpose::STANDARD.encode(bytes),
        )),
        Form::Encoded | Form::Script => Some((path.to_string(), String::from_utf8(bytes).ok()?)),
    }
}

/// A plugin that loaded, and the captured file paths each of its views reads.
pub struct Ready {
    pub loaded: Loaded,
    /// What its view of one run reads.
    pub inputs: Vec<String>,
    /// What its view of the playbook's launches reads from each; `None` when it draws none.
    pub runs_inputs: Option<Vec<String>>,
}

/// A loaded plugin, or why it did not load.
pub type Plugin = Result<Ready, String>;

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
        let runs_inputs = plugin.runs_inputs().map_err(|f| f.to_string())?;
        Ok(Ready {
            loaded: plugin,
            inputs,
            runs_inputs,
        })
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

/// The view `plugin` builds from `files`, the captured copies of its `inputs`, for `asked`.
pub fn run(
    plugin: &Loaded,
    inputs: &[String],
    files: Vec<File>,
    asked: &Asked,
) -> Result<View, String> {
    if files.is_empty() && !inputs.is_empty() {
        return Err(format!("No task has captured {} yet.", inputs.join(" or ")));
    }
    readable(files.iter().map(|file| file.text.len() as u64))?;
    plugin
        .view(&asked.request(files))
        .map_err(|fault| fault.to_string())
}

/// The view `plugin` builds from the launches in `request`.
pub fn run_runs(plugin: &Loaded, request: &RunsRequest) -> Result<View, String> {
    plugin.runs_view(request).map_err(|fault| fault.to_string())
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
    asked: &Asked,
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
    run(plugin, inputs, read, asked)
}

/// A local pack's report, built again when its plugin, the files it reads or the ask change.
#[derive(Default)]
pub struct Reporter {
    plugin: Option<Mark>,
    loaded: Option<Arc<Plugin>>,
    read: Vec<Mark>,
    /// What the next build asks with: the last ask, with the state its newest view returned
    /// and no event, so a build for a changed file does not answer the event again.
    asked: Option<Asked>,
    report: Option<Arc<Report>>,
}

impl Reporter {
    /// The same allocation until the report changes.
    pub fn report(&self) -> Option<&Arc<Report>> {
        self.report.as_ref()
    }

    /// Builds `pack`'s report for `asked`, loading its plugin through `plugins`; whether the
    /// report changed.
    pub fn refresh(&mut self, pack: &Path, asked: &Asked, plugins: &mut Plugins) -> bool {
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
        let fresh = self.asked.as_ref() != Some(asked);
        let held = match &mut self.asked {
            Some(held) if !fresh => held,
            held => held.insert(asked.clone()),
        };
        let Some(loaded) = &self.loaded else {
            self.read.clear();
            return self.report.take().is_some();
        };
        let view = match &**loaded {
            Err(error) => {
                if !reloaded && !fresh {
                    return false;
                }
                Err(error.clone())
            }
            Ok(Ready {
                loaded: plugin,
                inputs,
                ..
            }) => {
                let files = captured(pack, inputs);
                let read: Vec<Mark> = files.iter().map(|(.., mark)| mark.clone()).collect();
                if !reloaded && !fresh && read == self.read {
                    return false;
                }
                self.read = read;
                build(plugin, inputs, &files, held)
            }
        };
        let report = Report {
            author,
            asked: held.clone(),
            view,
            unread: Vec::new(),
        };
        held.event = None;
        if let Ok(view) = &report.view {
            held.state.clone_from(&view.live.state);
        }
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

    use crucible_view::{Run, Status};

    use crate::captured::Captured;
    use crate::live::{LaunchStatus, LivePlaybook};
    use crate::report::shown::{Scoped, Shown};
    use crate::report::{
        Asked, Built, FILE_BYTES, Gathered, Launched, Plugins, READ_BYTES, RUNS, Report, Reporter,
        RunsAsk, Wanted, WantedRuns, admit, file_named, readable, run_runs, stand_in, task_named,
        with_run,
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
        reporter.refresh(pack, &Asked::default(), &mut Plugins::default())
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
        let asked = shown.shown.asked();
        assert_eq!(asked.params["cvss"], Param::Range(0.0));
        let mut plugins = Plugins::default();
        assert!(reporter.refresh(dir.path(), &asked, &mut plugins));
        let laned = reporter.report().unwrap();
        assert_eq!(laned.asked, asked);
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
        let asked = shown.shown.asked();
        assert_eq!(
            asked.params["cvss"],
            Param::Range(9.0),
            "on the range's step"
        );
        assert!(reporter.refresh(dir.path(), &asked, &mut plugins));
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
        assert!(reporter.refresh(dir.path(), &shown.shown.asked(), &mut plugins));
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

    /// Three launches of `example`, newest first, built from its inbox: one with no run yet,
    /// one of both lanes with DEMO-101 found affected, and one of the go lane alone.
    fn example_launches(example: &Path) -> crucible_view::RunsRequest {
        let lane = |lane: &str| crucible_view::File {
            path: "verdicts.json".into(),
            task: format!("triage-{lane}"),
            text: fs::read_to_string(example.join(format!("inbox/{lane}.json"))).unwrap(),
        };
        let mut records: Vec<serde_json::Value> = serde_json::from_str(&lane("go").text).unwrap();
        let ticket = records
            .iter_mut()
            .find(|record| record["ticket_key"] == "DEMO-101")
            .unwrap();
        ticket["verdict_property"]["verdict"] = "affected".into();
        ticket["verdict_property"]["proposed_actions"]["lane"] = "present".into();
        ticket["verdict_property"]["rationale"] = "Reachable.".into();
        let regressed = crucible_view::File {
            text: serde_json::to_string(&records).unwrap(),
            ..lane("go")
        };
        let launch = |id: &str, status: Status, files: Vec<crucible_view::File>| Run {
            files,
            id: id.into(),
            status,
            ..run(id, 0)
        };
        crucible_view::RunsRequest {
            runs: vec![
                launch("triage:3", Status::Running, vec![]),
                launch("triage:2", Status::Done, vec![regressed, lane("python")]),
                launch("triage:1", Status::Done, vec![lane("go")]),
            ],
            total: 4,
            ..Default::default()
        }
    }

    /// The stats, the moved tickets' cells and the launch links of an example's runs view.
    fn history(view: &View) -> (Vec<String>, Vec<Vec<String>>, Vec<String>) {
        let Node::Stack { children } = &view.body else {
            panic!("the runs view is a stack");
        };
        let [
            Node::Stats { items },
            Node::Bars { .. },
            Node::Tabs { tabs, .. },
        ] = children.as_slice()
        else {
            panic!("the runs view is its stats over its bars over its tabs");
        };
        let rows = |at: usize| {
            let Node::Table { rows, .. } = &tabs[at].body else {
                panic!("a table");
            };
            rows.clone()
        };
        let moved = rows(0)
            .iter()
            .map(|row| row.cells.iter().map(|cell| cell.text.clone()).collect())
            .collect();
        let links = rows(1)
            .iter()
            .filter_map(|row| row.cells[0].href.clone())
            .collect();
        (
            items.iter().map(|item| item.value.clone()).collect(),
            moved,
            links,
        )
    }

    #[test]
    fn the_report_example_s_runs_view_follows_each_ticket_across_launches() {
        let wasm = fs::read(example().join("ui/report.wasm"))
            .expect("build it with `just example-report-view`");
        let plugin = Plugins::default().load("ui/report.wasm", &wasm);
        let ready = (*plugin).as_ref().unwrap();
        assert_eq!(
            ready.runs_inputs.as_deref(),
            Some(&["verdicts.json".to_string()][..])
        );
        let view = run_runs(&ready.loaded, &example_launches(&example())).unwrap();
        assert_eq!(view.title, "Triage history");
        let (stats, moved, links) = history(&view);
        assert_eq!(stats[..3], ["3 of 4", "64", "1"]);
        assert_eq!(
            moved,
            [[
                "DEMO-101",
                "affected",
                "sbom_inaccurate",
                "triage:2",
                "1",
                "sbom_inaccurate \u{2192} affected"
            ]]
        );
        assert_eq!(
            links,
            ["launch:triage:3", "launch:triage:2", "launch:triage:1"]
        );
    }

    #[test]
    #[ignore = "needs assets/js-engine/plugin.wasm from `just desktop-js-engine`"]
    fn the_typescript_example_s_runs_view_follows_each_ticket_across_launches() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/report-ts");
        let script = fs::read(example.join("ui/report.js")).unwrap();
        let plugin = Plugins::default().load("ui/report.js", &script);
        let ready = (*plugin).as_ref().unwrap();
        assert_eq!(
            ready.runs_inputs.as_deref(),
            Some(&["verdicts.json".to_string()][..])
        );
        let view = run_runs(&ready.loaded, &example_launches(&example)).unwrap();
        assert_eq!(view.title, "Triage history");
        let (stats, moved, links) = history(&view);
        assert_eq!(stats[..3], ["3 of 4", "64", "1"]);
        assert_eq!(
            moved,
            [[
                "DEMO-101",
                "Fix",
                "Close",
                "triage:2",
                "1",
                "Close \u{2192} Fix"
            ]]
        );
        assert_eq!(
            links,
            ["launch:triage:3", "launch:triage:2", "launch:triage:1"]
        );
        let empty = crucible_view::RunsRequest {
            total: 1,
            ..Default::default()
        };
        assert!(matches!(
            run_runs(&ready.loaded, &empty).unwrap().body,
            Node::Note { .. }
        ));
        let mut torn = example_launches(&example);
        torn.runs[2].files[0].text = "[{".into();
        let error = run_runs(&ready.loaded, &torn).unwrap_err();
        assert!(error.contains(", in triage:1"), "{error}");
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

    fn launch(key: &str, status: LaunchStatus) -> LivePlaybook {
        LivePlaybook {
            id: "triage".into(),
            key: key.into(),
            status,
            cost: Some(1.25),
            runs: 1,
            landed: 1_790_000_000,
            origin: "schedule".into(),
            ..LivePlaybook::default()
        }
    }

    #[test]
    fn a_launch_is_asked_with_its_status_and_its_cost_once_it_has_stopped() {
        for (status, read) in [
            (LaunchStatus::Running, Status::Running),
            (LaunchStatus::Done, Status::Done),
            (LaunchStatus::Error, Status::Error),
            (LaunchStatus::Cancelled, Status::Cancelled),
            (LaunchStatus::New, Status::Unfinished),
            (LaunchStatus::Other("parked".into()), Status::Unfinished),
        ] {
            let asked = Launched::of(&launch("PB-1", status));
            assert_eq!(asked.status, read);
            assert_eq!(asked.settled(), read != Status::Running);
            let cost = (read != Status::Running).then_some(1.25);
            assert_eq!(asked.cost, cost, "{read:?}");
            assert_eq!(
                (
                    asked.key.as_str(),
                    asked.runs,
                    asked.started,
                    asked.origin.as_str()
                ),
                ("PB-1", 1, 1_790_000_000, "schedule")
            );
        }
        let running = |cost: f64| {
            Launched::of(&LivePlaybook {
                cost: Some(cost),
                ..launch("PB-1", LaunchStatus::Running)
            })
        };
        assert_eq!(running(0.5), running(3.0), "a rising cost is the same ask");
        let more = LivePlaybook {
            runs: 2,
            ..launch("PB-1", LaunchStatus::Running)
        };
        assert_ne!(
            Launched::of(&more),
            running(0.5),
            "another run is a new ask"
        );
    }

    #[test]
    fn a_runs_view_is_asked_with_the_newest_launches_and_how_many_there_are() {
        let keys: Vec<String> = (0..RUNS + 3).map(|n| format!("PB-{n}")).collect();
        let listed: Vec<LivePlaybook> = keys
            .iter()
            .map(|key| launch(key, LaunchStatus::Done))
            .collect();
        let launches: Vec<&LivePlaybook> = listed.iter().collect();
        let wanted = WantedRuns::of("triage", "ui/report.js", &launches, Asked::default());
        assert_eq!(
            (wanted.playbook.as_str(), wanted.plugin.as_str()),
            ("triage", "ui/report.js")
        );
        assert_eq!(wanted.total, RUNS + 3);
        assert_eq!(
            wanted
                .launches
                .iter()
                .map(|launch| launch.key.as_str())
                .collect::<Vec<_>>(),
            keys[..RUNS]
        );
        assert_eq!(wanted.launches[0], Launched::of(&listed[0]));
    }

    #[test]
    fn the_runs_page_asks_once_and_keeps_only_the_answer_to_its_newest_ask() {
        let done = launch("PB-1", LaunchStatus::Done);
        let running = launch("PB-2", LaunchStatus::Running);
        let wanted = |playbook: &str, launches: &[&LivePlaybook]| {
            WantedRuns::of(playbook, "ui/report.js", launches, Asked::default())
        };
        let built = |wanted: &WantedRuns, title: &str| {
            let view = View {
                title: title.into(),
                body: Node::Stats { items: vec![] },
                controls: vec![],
                live: Default::default(),
            };
            let report = Report::pack(wanted.asked.clone(), Ok(view));
            Arc::new(Built::new(wanted.clone(), Some(report)))
        };
        let title = |ask: &RunsAsk, playbook: &str| {
            let view = ask.view(playbook)?.view.as_ref().ok()?;
            Some(view.title.clone())
        };

        let mut ask = RunsAsk::default();
        assert_eq!(ask.ask(None), None, "nothing to ask for");
        let first = wanted("triage", &[&running, &done]);
        assert_eq!(ask.ask(Some(first.clone())), Some(&first));
        assert_eq!(
            ask.ask(Some(first.clone())),
            None,
            "the same ask is sent once"
        );
        assert_eq!(title(&ask, "triage"), None, "not built yet");

        ask.answer(built(&first, "one"));
        assert_eq!(title(&ask, "triage").as_deref(), Some("one"));
        assert_eq!(
            title(&ask, "other"),
            None,
            "another playbook's is not shown"
        );

        let second = wanted("triage", &[&done]);
        assert_eq!(ask.ask(Some(second.clone())), Some(&second));
        assert_eq!(
            title(&ask, "triage").as_deref(),
            Some("one"),
            "the last view stays while the next is built"
        );
        ask.answer(built(&first, "late"));
        assert_eq!(
            title(&ask, "triage").as_deref(),
            Some("one"),
            "an answer to an older ask is dropped"
        );
        ask.answer(built(&second, "two"));
        assert_eq!(title(&ask, "triage").as_deref(), Some("two"));

        ask.answer(Arc::new(Built::new(second.clone(), None)));
        assert_eq!(title(&ask, "triage"), None, "the plugin draws no runs view");

        assert_eq!(ask.ask(None), None, "the page is left");
        assert_eq!(
            ask.ask(Some(second.clone())),
            Some(&second),
            "and asked on return"
        );
    }

    fn run(id: &str, bytes: usize) -> Run {
        Run {
            id: id.into(),
            status: Status::Done,
            started: 0.0,
            cost_usd: None,
            origin: "manual".into(),
            pack_version: None,
            params: Default::default(),
            files: vec![crucible_view::File {
                path: "verdicts.json".into(),
                task: "triage".into(),
                text: "x".repeat(bytes),
            }],
        }
    }

    #[test]
    fn a_stopped_unchanged_launch_is_kept_for_the_same_inputs_only() {
        let inputs = vec!["verdicts.json".to_string()];
        let done = Launched::of(&launch("PB-1", LaunchStatus::Done));
        let running = Launched::of(&launch("PB-2", LaunchStatus::Running));
        let cut = Launched::of(&launch("PB-0", LaunchStatus::Done));
        let asked = [done.clone(), running.clone(), cut.clone()];
        let gathered = Gathered {
            inputs: inputs.clone(),
            launches: asked.to_vec(),
            runs: vec![
                (done.clone(), run("PB-1", 3)),
                (running.clone(), run("PB-2", 3)),
            ],
            unread: Vec::new(),
        };
        assert_eq!(gathered.kept(&inputs, &done), Some(&run("PB-1", 3)));
        assert_eq!(
            gathered.kept(&inputs, &running),
            None,
            "a running launch is read again"
        );
        let rerun = Launched {
            runs: 2,
            ..done.clone()
        };
        assert_eq!(gathered.kept(&inputs, &rerun), None, "it changed since");
        let other = ["verdicts.json".to_string(), "notes.json".to_string()];
        assert_eq!(
            gathered.kept(&other, &done),
            None,
            "the plugin reads other files now"
        );
        let unknown = Launched::of(&launch("PB-9", LaunchStatus::Done));
        assert_eq!(gathered.kept(&inputs, &unknown), None);
        assert_eq!(gathered.kept(&inputs, &cut), None, "asked for, not read");
        assert_eq!(Gathered::default().kept(&[], &done), None);

        assert!(gathered.read(&inputs, &asked), "running and cut ones too");
        assert!(!gathered.read(&other, &asked));
        assert!(!gathered.read(&inputs, &asked[..2]));
        assert!(!gathered.read(&inputs, &[done.clone(), running, rerun]));
        assert!(!gathered.read(&inputs, &[]));
        assert!(Gathered::default().read(&[], &[]), "nothing to read");
        assert!(!Gathered::default().read(&[], &[done]));
    }

    #[test]
    fn a_report_reads_the_same_files_whatever_it_is_asked_with() {
        let wanted = Wanted {
            run: "r1".into(),
            plugin: "ui/report.js".into(),
            listed: vec![("triage/verdicts.json".into(), 8)],
            settled: false,
            asked: Asked::default(),
        };
        let asked = Wanted {
            asked: Asked {
                params: [("lane".to_string(), Param::Select("go".into()))].into(),
                turn: 3,
                state: Some("s".into()),
                event: Some(crucible_view::Event::Tick),
            },
            ..wanted.clone()
        };
        assert_ne!(wanted, asked);
        assert!(wanted.reads_as(&asked) && asked.reads_as(&wanted));
        for other in [
            Wanted {
                run: "r2".into(),
                ..wanted.clone()
            },
            Wanted {
                plugin: "ui/other.js".into(),
                ..wanted.clone()
            },
            Wanted {
                listed: vec![("triage/verdicts.json".into(), 9)],
                ..wanted.clone()
            },
            Wanted {
                listed: vec![],
                ..wanted.clone()
            },
            Wanted {
                settled: true,
                ..wanted.clone()
            },
        ] {
            assert!(!wanted.reads_as(&other), "{other:?}");
        }
    }

    #[test]
    fn launches_are_read_while_their_files_fit_the_bytes_a_view_reads() {
        let whole = READ_BYTES as usize;
        assert_eq!(with_run(0, &run("a", 0)), Some(0));
        assert_eq!(with_run(0, &run("a", whole)), Some(READ_BYTES));
        assert_eq!(with_run(0, &run("a", whole + 1)), None);
        assert_eq!(with_run(READ_BYTES - 3, &run("a", 3)), Some(READ_BYTES));
        assert_eq!(with_run(READ_BYTES - 3, &run("a", 4)), None);
        assert_eq!(with_run(u64::MAX, &run("a", 1)), None);
        let two = Run {
            files: [run("a", 5).files, run("a", 7).files].concat(),
            ..run("a", 0)
        };
        assert_eq!(with_run(1, &two), Some(13));
    }

    #[test]
    fn a_stand_in_plugin_is_carried_as_text() {
        use base64::Engine;
        assert_eq!(
            stand_in("/tmp/view.js", b"export const inputs = [];".to_vec()),
            Some(("/tmp/view.js".into(), "export const inputs = [];".into()))
        );
        let (path, text) = stand_in("/tmp/report.wasm", vec![0, 97, 115, 109]).unwrap();
        assert_eq!(path, "/tmp/report.wasm.b64");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .unwrap(),
            [0, 97, 115, 109]
        );
        assert_eq!(
            stand_in("ui/report.wasm.b64", b"AA==".to_vec()),
            Some(("ui/report.wasm.b64".into(), "AA==".into()))
        );
        assert_eq!(stand_in("/tmp/view.js", vec![0xff, 0xfe]), None, "not text");
        assert_eq!(
            stand_in("/tmp/notes.md", b"# hi".to_vec()),
            None,
            "no plugin"
        );
        let wasm = fs::read(example().join("ui/report.wasm"))
            .expect("build it with `just example-report-view`");
        let (path, text) = stand_in("report.wasm", wasm).unwrap();
        assert!(Plugins::default().load(&path, text.as_bytes()).is_ok());
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
        let none = Asked::default();
        assert!(reporter.refresh(dir.path(), &none, &mut plugins));
        assert_eq!(triaged(&reporter), "64");
        assert!(reporter.report().unwrap().asked.params.is_empty());
        assert!(
            !reporter.refresh(dir.path(), &none, &mut plugins),
            "no params is no change"
        );

        let go = Asked {
            params: [("task".to_string(), Param::Select("triage-go".into()))].into(),
            ..Asked::default()
        };
        let before = Arc::clone(reporter.report().unwrap());
        assert!(reporter.refresh(dir.path(), &go, &mut plugins));
        assert_eq!(triaged(&reporter), "34");
        assert_eq!(reporter.report().unwrap().asked, go);
        assert_eq!(
            before.asked.params.len(),
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
            reporter.report().unwrap().asked,
            go,
            "a refresh keeps the params"
        );

        assert!(reporter.refresh(dir.path(), &none, &mut plugins));
        assert_eq!(
            triaged(&reporter),
            "34",
            "go alone has no python verdicts now"
        );
        assert!(reporter.report().unwrap().asked.params.is_empty());
    }

    /// A pack directory holding the stack example's plugin.
    fn stack_pack() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("ui")).unwrap();
        fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/stack/ui/stack.wasm"),
            dir.path().join("ui/stack.wasm"),
        )
        .expect("build it with `just example-stack-view`");
        dir
    }

    #[test]
    fn the_stack_example_plays_through_the_reporter() {
        use crucible_view::{Draw, Event, Key};
        let dir = stack_pack();
        let mut reporter = Reporter::default();
        let mut plugins = Plugins::default();
        let a = crate::Selection::Local("stack".into());
        let mut shown = Scoped::default();
        assert!(reporter.refresh(dir.path(), &shown.shown.asked(), &mut plugins));
        shown.follow(Some(&a), reporter.report());
        let view = |reporter: &Reporter| reporter.report().unwrap().view.clone().unwrap();
        let first = view(&reporter);
        assert_eq!(first.title, "Stack");
        assert_eq!(first.live.tick_ms, Some(600));
        assert_eq!(first.live.keys.len(), 6);
        let state = first.live.state.clone().unwrap();
        assert!(state.starts_with(&"0".repeat(180)), "an empty board");
        let Node::Stack { children } = &first.body else {
            panic!("the view is a stack");
        };
        let Node::Canvas { commands, .. } = &children[1] else {
            panic!("the board is a canvas");
        };
        let hits: Vec<&str> = commands
            .iter()
            .filter_map(|command| match command {
                Draw::Hit { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(hits, ["left", "right", "turn", "down", "drop", "new"]);

        let mut play = |shown: &mut Scoped<crate::Selection>, event: Event| {
            shown.shown.play.fire(event);
            assert!(reporter.refresh(dir.path(), &shown.shown.asked(), &mut plugins));
            shown.follow(Some(&a), reporter.report());
            assert_eq!(shown.shown.asked().event, None, "answered");
            view(&reporter)
        };
        let fell = play(&mut shown, Event::Tick);
        assert_ne!(fell.live.state, first.live.state, "the piece fell a row");
        let moved = play(&mut shown, Event::Key(Key::Left));
        assert_ne!(moved.live.state, fell.live.state, "and moved left");
        let dropped = play(&mut shown, Event::Key(Key::Space));
        let board = dropped.live.state.clone().unwrap();
        let filled = board[..180].bytes().filter(|cell| *cell != b'0').count();
        assert_eq!(filled, 4, "the piece set down: {board}");
        assert!(
            board[..140].bytes().all(|cell| cell == b'0'),
            "on the floor: {board}"
        );
        for _ in 0..40 {
            play(&mut shown, Event::Hit("drop".into()));
        }
        let over = play(&mut shown, Event::Tick);
        assert_eq!(over.live.tick_ms, None, "a full board stops the clock");
        let again = play(&mut shown, Event::Key(Key::Enter));
        assert_eq!(again.live.tick_ms, Some(600));
        assert!(again.live.state.unwrap().starts_with(&"0".repeat(180)));
    }

    #[test]
    fn a_build_for_a_changed_plugin_continues_from_the_newest_state_without_the_event() {
        use crucible_view::Event;
        let dir = stack_pack();
        let mut reporter = Reporter::default();
        let mut plugins = Plugins::default();
        let state = |reporter: &Reporter| {
            reporter
                .report()
                .unwrap()
                .view
                .as_ref()
                .unwrap()
                .live
                .state
                .clone()
        };
        assert!(reporter.refresh(dir.path(), &Asked::default(), &mut plugins));
        let ticked = Asked {
            turn: 1,
            state: state(&reporter),
            event: Some(Event::Tick),
            ..Asked::default()
        };
        assert!(reporter.refresh(dir.path(), &ticked, &mut plugins));
        let fell = state(&reporter);
        assert_ne!(fell, ticked.state, "the tick moved the piece");
        assert_eq!(reporter.report().unwrap().asked.event, Some(Event::Tick));
        assert!(
            !reporter.refresh(dir.path(), &ticked, &mut plugins),
            "the same ask builds nothing"
        );

        let wasm = fs::read(dir.path().join("ui/stack.wasm")).unwrap();
        fs::remove_file(dir.path().join("ui/stack.wasm")).unwrap();
        fs::write(dir.path().join("ui/stack.wasm"), wasm).unwrap();
        assert!(
            !reporter.refresh(dir.path(), &ticked, &mut plugins),
            "built again from the newest state, the view is the same"
        );
        assert_eq!(state(&reporter), fell, "the tick is not answered twice");

        let other = Asked {
            turn: 2,
            state: fell.clone(),
            event: Some(Event::Hit("nothing".into())),
            ..Asked::default()
        };
        reporter.refresh(dir.path(), &other, &mut plugins);
        assert_eq!(state(&reporter), fell, "a hit the game has no button for");
        let built = &reporter.report().unwrap().asked;
        assert_eq!((built.turn, &built.event), (2, &other.event));
    }
}

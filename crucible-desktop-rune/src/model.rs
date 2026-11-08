//! Pack discovery, the session-log fold, and the background client the window talks to.
//!
//! Every read of `state/session.jsonl`, every `try_wait`, and every spawn happens on the client
//! thread. The UI only ever takes a finished snapshot, so a slow disk or a slow command never
//! costs a frame.
//!
//! The session log folds through [`crate::session::RunFold`]; the engine's console output, kept in
//! `state/desktop-run.log` for a run this app started, parses through [`crate::logline`].

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use rune::Waker;

use crate::logline::LogLine;
use crate::pack::PackFiles;
use crate::preview::Preview;
use crate::session::{RunFold, TaskView};

const LOG: &str = "state/session.jsonl";
const RUN_START: &str = "state/playbook-run.json";
const STDERR: &str = "state/desktop-run.log";
/// How often a live run republishes its elapsed time.
const LIVE_TICK: Duration = Duration::from_millis(500);
/// How often an idle client looks for a run started from the CLI.
const IDLE_TICK: Duration = Duration::from_secs(2);
/// Console lines kept per pack.
const LOG_LINES: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    NotRun,
    Running,
    Unfinished,
    Valid,
    Invalid,
    Error,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::NotRun => "Not run",
            Verdict::Running => "Running",
            Verdict::Unfinished => "Unfinished",
            Verdict::Valid => "Valid",
            Verdict::Invalid => "Invalid",
            Verdict::Error => "Error",
        }
    }

    pub fn is_bad(self) -> bool {
        matches!(self, Verdict::Invalid | Verdict::Error)
    }
}

/// What the window draws. Owned, immutable, cheap to clone.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub packs: Vec<PackView>,
    pub error: Option<String>,
}

impl Snapshot {
    fn elapsed(&self) -> Vec<Option<String>> {
        self.packs.iter().map(|pack| pack.elapsed.clone()).collect()
    }
}

#[derive(Clone)]
pub struct PackView {
    pub name: String,
    pub workflow: String,
    pub manifest: String,
    pub goal: String,
    pub budget_usd: f64,
    pub tasks: Vec<TaskView>,
    pub spent: f64,
    pub passed: usize,
    pub failed: usize,
    pub verdict: Verdict,
    pub elapsed: Option<String>,
    pub started_at: String,
    pub detail: Option<String>,
    pub log: Vec<LogLine>,
    pub transcript: Vec<LogLine>,
    pub agent_events: Vec<crate::session::AgentStep>,
    pub pack: PackFiles,
    pub preview: Preview,
}

impl PackView {
    /// A case-insensitive match on the fields a reader can see in the row.
    pub fn matches(&self, query: &str) -> bool {
        let fields = [
            self.name.as_str(),
            self.goal.as_str(),
            self.workflow.as_str(),
            self.verdict.label(),
        ];
        fields
            .iter()
            .any(|field| field.to_ascii_lowercase().contains(query))
    }
}

#[derive(Clone, Debug)]
pub enum Request {
    Launch { pack: String, max_time: String },
}

/// The UI-side handle. Dropping it stops the client thread.
pub struct Client {
    tx: Sender<Request>,
    shared: Arc<Mutex<Shared>>,
}

struct Shared {
    snapshot: Snapshot,
    version: u64,
    waker: Option<Waker>,
}

impl Client {
    pub fn spawn(repo: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            snapshot: Snapshot::default(),
            version: 0,
            waker: None,
        }));
        let worker = Arc::clone(&shared);
        if let Err(error) = std::thread::Builder::new()
            .name("crucible-playbooks".into())
            .spawn(move || run_client(repo, rx, worker))
        {
            publish(
                &shared,
                Snapshot {
                    packs: Vec::new(),
                    error: Some(format!("starting the local worker: {error}")),
                },
            );
        }
        Client { tx, shared }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.waker = Some(waker);
        }
    }

    pub fn launch(&self, pack: &str, max_time: &str) {
        let _ = self.tx.send(Request::Launch {
            pack: pack.to_string(),
            max_time: max_time.to_string(),
        });
    }

    /// The newest snapshot, unless the UI has already taken this version.
    pub fn take(&self, seen: u64) -> Option<(u64, Snapshot)> {
        let shared = self.shared.lock().ok()?;
        (shared.version > seen).then(|| (shared.version, shared.snapshot.clone()))
    }
}

/// Publishes and returns the elapsed labels the window will show, so a live tick only draws when
/// a second's text actually changed.
fn publish(shared: &Mutex<Shared>, snapshot: Snapshot) -> Vec<Option<String>> {
    let elapsed = snapshot.elapsed();
    let waker = match shared.lock() {
        Ok(mut shared) => {
            shared.snapshot = snapshot;
            shared.version += 1;
            shared.waker.clone()
        }
        Err(_) => return elapsed,
    };
    if let Some(waker) = waker {
        waker();
    }
    elapsed
}

fn run_client(repo: PathBuf, rx: Receiver<Request>, shared: Arc<Mutex<Shared>>) {
    let crucible = crucible_binary(&repo);
    let mut error: Option<String> = None;
    let mut packs = load_packs(&repo).unwrap_or_else(|message| {
        error = Some(message);
        Vec::new()
    });
    let mut next_local = Instant::now();
    let mut shown = publish(&shared, snapshot(&packs, error.clone()));

    loop {
        let now = Instant::now();
        let wait = next_local
            .saturating_duration_since(now)
            .min(Duration::from_millis(500));
        match rx.recv_timeout(wait) {
            Ok(Request::Launch { pack, max_time }) => {
                error = None;
                match packs.iter_mut().find(|candidate| candidate.name == pack) {
                    Some(pack) => {
                        pack.tick();
                        if let Err(message) = pack.launch(&crucible, &max_time) {
                            error = Some(message);
                        }
                    }
                    None => error = Some(format!("no playbook named {pack}")),
                }
                shown = publish(&shared, snapshot(&packs, error.clone()));
                next_local = Instant::now() + LIVE_TICK;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        let now = Instant::now();
        let mut changed = false;
        if now >= next_local {
            for pack in &mut packs {
                changed |= pack.tick();
            }
            next_local = now
                + if packs.iter().any(Pack::running) {
                    LIVE_TICK
                } else {
                    IDLE_TICK
                };
        }
        let next = snapshot(&packs, error.clone());
        if changed || next.elapsed() != shown {
            shown = publish(&shared, next);
        }
    }
}

fn snapshot(packs: &[Pack], error: Option<String>) -> Snapshot {
    Snapshot {
        packs: packs.iter().map(Pack::view).collect(),
        error,
    }
}

struct Pack {
    name: String,
    workflow: String,
    manifest: String,
    dir: PathBuf,
    plan: Option<PathBuf>,
    goal: String,
    budget_usd: f64,
    run: Run,
    files: PackFiles,
    /// The pack as it compiles now, redone whenever its files change.
    preview: Preview,
    started: bool,
    since: Option<Instant>,
    stderr: PathBuf,
}

impl Pack {
    /// Only `[workflow] type = "playbook"` manifests are tracked; scored loops are not.
    fn load(dir: PathBuf, repo: &Path) -> Option<Self> {
        let name = dir.file_name()?.to_string_lossy().into_owned();
        let manifest_path = dir.join("crucible.toml");
        let text = fs::read_to_string(&manifest_path).ok()?;
        let doc: toml::Value = toml::from_str(&text).ok()?;
        let workflow = doc
            .get("workflow")
            .and_then(|w| w.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if workflow != "playbook" {
            return None;
        }
        let goal = doc
            .get("agent")
            .and_then(|a| a.get("goal"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let plan_file = dir.join("plan.toml");
        let plan = plan_file.is_file().then_some(plan_file);
        let budget_usd = plan
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
            .and_then(|d| {
                d.get("budget")
                    .and_then(|b| b.get("usd"))
                    .and_then(|v| v.as_float())
            })
            .unwrap_or(1.0);

        let mut run = Run {
            session: LogReader {
                path: dir.join(LOG),
                ..LogReader::default()
            },
            console: LogReader {
                path: dir.join(STDERR),
                ..LogReader::default()
            },
            ..Run::default()
        };
        run.started_at = read_start(&dir);
        run.read_new();
        let started = dir.join(RUN_START).is_file();
        let files = PackFiles::read_dir(&dir);
        Some(Pack {
            workflow: workflow.clone(),
            manifest: manifest_path
                .strip_prefix(repo)
                .unwrap_or(&manifest_path)
                .display()
                .to_string(),
            stderr: dir.join(STDERR),
            name,
            dir: dir.clone(),
            plan,
            goal,
            budget_usd,
            run,
            preview: crate::preview::compile(&dir),
            files,
            started,
            since: None,
        })
    }

    fn running(&self) -> bool {
        self.run.running(self.started)
    }

    /// Fold new log lines, notice an external run, and reap a child this app started.
    fn tick(&mut self) -> bool {
        let started = self.dir.join(RUN_START).is_file();
        let start_changed = started != self.started;
        self.started = started;
        if started {
            let started_at = read_start(&self.dir);
            if started_at != self.run.started_at {
                self.run.started_at = started_at;
            }
        }

        let mut changed = self.run.read_new() | start_changed;
        let files = PackFiles::read_dir(&self.dir);
        if files != self.files {
            self.files = files;
            self.preview = crate::preview::compile(&self.dir);
            changed = true;
        }
        if let Some(mut child) = self.run.child.take() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.run.exit(status, &self.stderr);
                    changed = true;
                }
                Ok(None) => self.run.child = Some(child),
                Err(error) => {
                    self.run.error = Some(error.to_string());
                    changed = true;
                }
            }
        }
        if !self.running() && self.since.take().is_some() {
            changed = true;
        }
        changed
    }

    fn launch(&mut self, crucible: &Path, max_time: &str) -> Result<(), String> {
        if self.running() {
            return Err(format!("{} is already running", self.name));
        }
        self.run.follow();
        self.started = false;
        self.since = Some(Instant::now());

        let mut command = Command::new(crucible);
        command.current_dir(&self.dir);
        command.arg("plan").arg("run");
        if let Some(plan) = &self.plan {
            command.arg("--file").arg(plan);
        }
        command
            .arg("--manifest")
            .arg(self.dir.join("crucible.toml"));
        command
            .arg("--max-cost")
            .arg(format!("{:.4}", self.budget_usd.max(0.01)));
        command.arg("--max-time").arg(max_time);
        if let Some(parent) = self.stderr.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let stderr = File::create(&self.stderr)
            .map_err(|e| format!("opening {}: {e}", self.stderr.display()))?;
        let stdout = stderr
            .try_clone()
            .map_err(|e| format!("sharing {}: {e}", self.stderr.display()))?;
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
        let child = command
            .spawn()
            .map_err(|e| format!("starting {}: {e}", crucible.display()))?;
        self.run.child = Some(child);
        Ok(())
    }

    fn view(&self) -> PackView {
        PackView {
            name: self.name.clone(),
            workflow: self.workflow.clone(),
            manifest: self.manifest.clone(),
            goal: self.goal.clone(),
            budget_usd: self.budget_usd,
            tasks: self.run.fold.tasks.clone(),
            spent: self.run.fold.spent(),
            passed: self.run.fold.passed(),
            failed: self.run.fold.failed(),
            verdict: self.run.verdict(self.started),
            elapsed: self.since.map(|since| shown_elapsed(since.elapsed())),
            started_at: self.run.started_at.clone(),
            detail: self.run.detail(),
            log: self.run.log.clone(),
            transcript: self.run.fold.transcript.clone(),
            agent_events: self.run.fold.agent_events.clone(),
            pack: self.files.clone(),
            preview: self.preview.clone(),
        }
    }
}

fn load_packs(repo: &Path) -> Result<Vec<Pack>, String> {
    let examples = repo.join("examples");
    let entries =
        fs::read_dir(&examples).map_err(|e| format!("reading {}: {e}", examples.display()))?;
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|dir| dir.join("crucible.toml").is_file())
        .collect();
    dirs.sort();
    Ok(dirs
        .into_iter()
        .filter_map(|dir| Pack::load(dir, repo))
        .collect())
}

#[derive(Default)]
struct LogReader {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
}

impl LogReader {
    fn skip_to_end(&mut self) {
        self.offset = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        self.pending.clear();
    }

    /// New complete lines since the last read. A torn tail stays pending until its newline lands.
    fn read_new(&mut self) -> (Vec<String>, bool) {
        let Ok(mut file) = File::open(&self.path) else {
            return (Vec::new(), false);
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.offset = 0;
            self.pending.clear();
        }
        if len == self.offset {
            return (Vec::new(), false);
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return (Vec::new(), false);
        }
        let mut bytes = Vec::new();
        if file
            .take(len - self.offset)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return (Vec::new(), false);
        }
        self.offset = len;
        self.pending.extend_from_slice(&bytes);

        let mut lines = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
        }
        (lines, true)
    }
}

#[derive(Default)]
struct Run {
    fold: RunFold,
    log: Vec<LogLine>,
    started_at: String,
    error: Option<String>,
    child: Option<Child>,
    session: LogReader,
    console: LogReader,
}

impl Run {
    fn read_new(&mut self) -> bool {
        let (events, read) = self.session.read_new();
        let mut changed = read;
        for line in events {
            changed |= self.fold.apply_line(&line);
        }
        let (lines, read) = self.console.read_new();
        changed |= read && !lines.is_empty();
        self.log
            .extend(lines.iter().map(|line| LogLine::parse(line)));
        if self.log.len() > LOG_LINES {
            self.log.drain(..self.log.len() - LOG_LINES);
        }
        changed
    }

    fn follow(&mut self) {
        self.session.skip_to_end();
        self.console = LogReader {
            path: std::mem::take(&mut self.console.path),
            ..LogReader::default()
        };
        self.fold = RunFold::default();
        self.log.clear();
        self.error = None;
        self.started_at.clear();
        self.child = None;
    }

    fn running(&self, started: bool) -> bool {
        self.child.is_some()
            || (started && self.fold.shutdown.is_none() && !self.fold.tasks.is_empty())
    }

    fn verdict(&self, started: bool) -> Verdict {
        if self.running(started) {
            return Verdict::Running;
        }
        if let Some(shutdown) = &self.fold.shutdown {
            return if shutdown.reason.starts_with("completed") {
                Verdict::Valid
            } else {
                Verdict::Invalid
            };
        }
        if !self.fold.tasks.is_empty() && self.fold.required_settled() {
            return if self.fold.required_passed() {
                Verdict::Valid
            } else {
                Verdict::Invalid
            };
        }
        if self.fold.tasks.is_empty() {
            Verdict::NotRun
        } else {
            Verdict::Unfinished
        }
    }

    fn detail(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(error.clone());
        }
        if let Some(shutdown) = &self.fold.shutdown {
            return Some(shutdown.reason.clone());
        }
        if let Some(note) = self.fold.notes.last() {
            return Some(note.clone());
        }
        if !self.fold.tasks.is_empty() && self.fold.required_settled() {
            return Some(if self.fold.required_passed() {
                "all required tasks settled; the log carries no shutdown record".to_string()
            } else {
                "required tasks did not all pass; the log carries no shutdown record".to_string()
            });
        }
        None
    }

    fn exit(&mut self, status: std::process::ExitStatus, stderr: &Path) {
        self.read_new();
        self.child = None;
        if self.fold.shutdown.is_none() {
            let tail = tail_of(stderr, 2);
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            self.error = Some(match tail {
                Some(tail) => format!("exited with {code}: {tail}"),
                None => format!("exited with {code} before writing a shutdown event"),
            });
        }
    }
}

fn tail_of(path: &Path, lines: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let tail: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(lines)
        .collect();
    if tail.is_empty() {
        return None;
    }
    Some(tail.into_iter().rev().collect::<Vec<_>>().join(" · "))
}

fn shown_elapsed(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    if total >= 3600 {
        format!("{}h {:02}m", total / 3600, total % 3600 / 60)
    } else if total >= 60 {
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        format!("{total}s")
    }
}

#[derive(Deserialize)]
struct StartRecord {
    #[serde(default)]
    started_at: String,
}

fn read_start(dir: &Path) -> String {
    let Ok(text) = fs::read_to_string(dir.join(RUN_START)) else {
        return String::new();
    };
    let Ok(record) = serde_json::from_str::<StartRecord>(&text) else {
        return String::new();
    };
    let time: String = record
        .started_at
        .split_once('T')
        .map(|(_, rest)| rest.chars().take(8).collect())
        .unwrap_or_default();
    time
}

/// The binary the Run button spawns: an override beats the checkout's build, which beats `$PATH`.
pub fn crucible_binary(repo: &Path) -> PathBuf {
    if let Ok(bin) = std::env::var("CRUCIBLE_BIN") {
        return PathBuf::from(bin);
    }
    for candidate in ["target/debug/crucible", "target/release/crucible"] {
        let path = repo.join(candidate);
        if path.is_file() {
            return path;
        }
    }
    PathBuf::from("crucible")
}

fn find_repo(start: PathBuf, levels: usize) -> Option<PathBuf> {
    let mut dir = start;
    for _ in 0..levels {
        if dir.join("examples").is_dir()
            && dir.join("Cargo.toml").is_file()
            && dir.join("crucible").is_dir()
        {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

pub fn default_repo() -> PathBuf {
    if let Ok(repo) = std::env::var("CRUCIBLE_REPO") {
        return PathBuf::from(repo);
    }
    if let Some(dir) = find_repo(std::env::current_dir().unwrap_or_default(), 4) {
        return dir;
    }
    // A bundled .app launches with cwd /; its executable still lives under the repo.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = find_repo(exe, 8)
    {
        return dir;
    }
    std::env::current_dir().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use crate::model::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// `CRUCIBLE_BIN` is process-wide; the client threads read it as they start.
    static ENV: Mutex<()> = Mutex::new(());

    fn wait_for(
        client: &Client,
        seen: &mut u64,
        what: &str,
        pred: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some((version, snapshot)) = client.take(*seen) {
                *seen = version;
                if pred(&snapshot) {
                    return snapshot;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn fake_repo(dir: &Path) -> PathBuf {
        let pack = dir.join("examples/demo");
        fs::create_dir_all(pack.join("state")).unwrap();
        fs::write(
            pack.join("crucible.toml"),
            "[agent]\ngoal = \"prove the tracker\"\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n",
        )
        .unwrap();
        fs::write(
            pack.join("plan.toml"),
            "version = 1\n\n[budget]\nusd = 2.0\n",
        )
        .unwrap();
        dir.to_path_buf()
    }

    fn append(pack: &Path, lines: &[&str]) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(pack.join("state/session.jsonl"))
            .unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    const ADMITTED: &str = r#"{"v":1,"kind":"plan_admitted","plan_version":1,"reason":"","budget_usd":2.0,"tasks":[{"name":"first","kind":"agent","depends_on":[],"required":true},{"name":"second","kind":"command","depends_on":["first"],"required":false}]}"#;
    const FIRST: &str = r#"{"v":1,"kind":"task_result","task":"first","status":"pass","attempts":1,"cost_usd":0.25,"output":{"entries":1}}"#;
    const SECOND: &str = r#"{"v":1,"kind":"task_result","task":"second","status":"fail","attempts":2,"cost_usd":0.5,"note":"wrote nothing"}"#;
    const SHUTDOWN: &str = r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"completed"}"#;

    #[test]
    fn the_checked_in_playbook_log_folds_to_a_valid_verdict() {
        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let packs: Vec<PackView> = load_packs(&repo).unwrap().iter().map(Pack::view).collect();
        assert!(
            packs.iter().all(|pack| pack.workflow == "playbook"),
            "the local backend lists playbooks and nothing else"
        );
        let playbook = packs
            .iter()
            .find(|pack| pack.name == "playbook")
            .expect("playbook pack");
        assert_eq!(playbook.verdict, Verdict::Valid);
        assert_eq!(playbook.tasks.len(), 7);
        assert_eq!(playbook.passed, 6);
        assert_eq!(playbook.failed, 1);
        assert_eq!(playbook.spent, 0.0);
        let freshness = playbook
            .tasks
            .iter()
            .find(|task| task.name == "audit-freshness")
            .unwrap();
        assert_eq!(
            freshness.result.as_ref().unwrap().status,
            crucible_contract::session::TaskStatus::Fail
        );
        assert!(!freshness.required);
    }

    #[test]
    fn a_run_written_by_something_else_reads_as_live_until_its_shutdown_lands() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fake_repo(dir.path());
        let pack = repo.join("examples/demo");
        append(&pack, &[ADMITTED, FIRST]);
        fs::write(
            pack.join("state/playbook-run.json"),
            r#"{"run":"r1","started_at":"2026-01-01T10:00:00Z","base":"","plan":""}"#,
        )
        .unwrap();

        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let client = Client::spawn(repo);
        let mut seen = 0;
        let live = wait_for(&client, &mut seen, "the live run", |snapshot| {
            snapshot
                .packs
                .first()
                .is_some_and(|pack| pack.verdict == Verdict::Running)
        });
        let demo = &live.packs[0];
        assert_eq!(demo.tasks.len(), 2);
        assert_eq!(demo.passed, 1);
        assert_eq!(demo.started_at, "10:00:00");

        append(&pack, &[SECOND, SHUTDOWN]);
        let done = wait_for(&client, &mut seen, "the shutdown", |snapshot| {
            snapshot
                .packs
                .first()
                .is_some_and(|pack| pack.verdict == Verdict::Valid)
        });
        let demo = &done.packs[0];
        assert_eq!(demo.passed, 1);
        assert_eq!(demo.failed, 1);
        assert!((demo.spent - 0.75).abs() < 1e-9);
        assert_eq!(demo.detail.as_deref(), Some("completed"));
    }

    #[test]
    fn launching_through_the_client_tracks_the_child_to_its_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fake_repo(dir.path());
        let stub = dir.path().join("stub-crucible");
        fs::write(
            &stub,
            format!(
                r#"#!/bin/sh
d=$(pwd)
printf '%s\n' '{ADMITTED}' >> "$d/state/session.jsonl"
sleep 0.1
printf '%s\n' '{FIRST}' >> "$d/state/session.jsonl"
sleep 0.1
printf '%s\n' '{SECOND}' >> "$d/state/session.jsonl"
printf '%s\n' '{SHUTDOWN}' >> "$d/state/session.jsonl"
"#
            ),
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&stub)
            .status()
            .unwrap();
        // SAFETY: the test binary is single-threaded at this point and nothing else reads the
        // variable; the client thread inherits it.
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CRUCIBLE_BIN", &stub) };

        let client = Client::spawn(repo);
        let mut seen = 0;
        wait_for(&client, &mut seen, "discovery", |snapshot| {
            !snapshot.packs.is_empty()
        });
        client.launch("demo", "1m");

        let running = wait_for(&client, &mut seen, "the launch", |snapshot| {
            snapshot
                .packs
                .first()
                .is_some_and(|pack| pack.verdict == Verdict::Running)
        });
        assert!(running.packs[0].elapsed.is_some());

        let done = wait_for(&client, &mut seen, "the verdict", |snapshot| {
            snapshot
                .packs
                .first()
                .is_some_and(|pack| pack.verdict == Verdict::Valid)
        });
        let demo = &done.packs[0];
        assert_eq!(demo.passed, 1);
        assert_eq!(demo.failed, 1);
        assert_eq!(demo.detail.as_deref(), Some("completed"));
    }

    #[test]
    fn a_pack_file_saved_elsewhere_shows_its_new_text() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fake_repo(dir.path());
        let workflow = repo.join("examples/demo/workflow.star");
        fs::write(&workflow, "x = 1\n").unwrap();
        let client = Client::spawn(repo);
        let mut seen = 0;
        let text = |snapshot: &Snapshot| {
            snapshot
                .packs
                .first()
                .and_then(|pack| pack.pack.file("workflow.star"))
                .and_then(|file| file.text.clone())
        };
        wait_for(&client, &mut seen, "discovery", |snapshot| {
            text(snapshot).as_deref() == Some("x = 1\n")
        });
        fs::write(&workflow, "x = 2\n").unwrap();
        wait_for(&client, &mut seen, "the saved text", |snapshot| {
            text(snapshot).as_deref() == Some("x = 2\n")
        });
    }

    #[test]
    fn a_child_that_dies_without_a_shutdown_shows_its_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fake_repo(dir.path());
        let stub = dir.path().join("stub-crucible");
        fs::write(
            &stub,
            "#!/bin/sh\nprintf '%s\n' 'missing --max-time' >&2\nexit 2\n",
        )
        .unwrap();
        std::process::Command::new("chmod")
            .arg("+x")
            .arg(&stub)
            .status()
            .unwrap();
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CRUCIBLE_BIN", &stub) };

        let client = Client::spawn(repo);
        let mut seen = 0;
        wait_for(&client, &mut seen, "discovery", |snapshot| {
            !snapshot.packs.is_empty()
        });
        client.launch("demo", "1m");
        let failed = wait_for(&client, &mut seen, "the failure", |snapshot| {
            snapshot
                .packs
                .first()
                .and_then(|pack| pack.detail.clone())
                .is_some_and(|d| d.contains("exited with 2"))
        });
        assert!(
            failed.packs[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("missing --max-time")
        );
    }
}

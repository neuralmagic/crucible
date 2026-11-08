//! A client for `crucible-lsp`, run as a child process speaking JSON-RPC over stdio. The worker
//! thread owns the process; the UI sends it the files it shows and reads back a [`View`] of
//! each file's diagnostics.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rune::Waker;
use serde_json::{Value, json};

/// A line and a UTF-16 column, as LSP counts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub start: Pos,
    pub end: Pos,
    pub severity: Severity,
    pub message: String,
    /// Code the server says can be deleted.
    pub unnecessary: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Status {
    #[default]
    Idle,
    Starting,
    Ready,
    Unavailable(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct View {
    pub status: Status,
    pub problems: HashMap<PathBuf, Vec<Problem>>,
}

/// How long the server runs with nothing new to check before it is stopped. The next file
/// shown starts it again.
const IDLE: Duration = Duration::from_secs(300);

enum Cmd {
    Show {
        path: PathBuf,
        text: String,
    },
    /// Stop the server and forget what it reported.
    Stop,
    Incoming(Value),
    Exited(String),
}

struct Shared {
    view: View,
    version: u64,
    waker: Option<Waker>,
}

pub struct Lsp {
    tx: Sender<Cmd>,
    shared: Arc<Mutex<Shared>>,
}

impl Lsp {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared {
            view: View::default(),
            version: 0,
            waker: None,
        }));
        let worker = Arc::clone(&shared);
        let wake = tx.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("crucible-playbooks-lsp".into())
            .spawn(move || run(rx, wake, worker))
        {
            update(&shared, |view| {
                view.status = Status::Unavailable(format!("starting the LSP worker: {error}"));
            });
        }
        Lsp { tx, shared }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.waker = Some(waker);
        }
    }

    /// Have the server check `path` as `text`: open it, or send the new text when it changed.
    pub fn show(&self, path: &Path, text: &str) {
        let _ = self.tx.send(Cmd::Show {
            path: path.to_path_buf(),
            text: text.to_string(),
        });
    }

    /// Stop the server, if one runs, and clear its problems.
    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn take(&self, seen: u64) -> Option<(u64, View)> {
        let shared = self.shared.lock().ok()?;
        (shared.version != seen).then(|| (shared.version, shared.view.clone()))
    }
}

fn update(shared: &Mutex<Shared>, f: impl FnOnce(&mut View)) {
    let Ok(mut shared) = shared.lock() else {
        return;
    };
    f(&mut shared.view);
    shared.version += 1;
    if let Some(waker) = &shared.waker {
        waker();
    }
}

/// The `crucible-lsp` to run: `$CRUCIBLE_LSP`, the one beside this executable (inside the app
/// bundle), or the first on `PATH`.
pub fn binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CRUCIBLE_LSP") {
        return Some(PathBuf::from(path));
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("crucible-lsp")))
        .filter(|path| path.is_file());
    beside.or_else(|| {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join("crucible-lsp"))
            .find(|path| path.is_file())
    })
}

/// The id of the one request the client makes.
const INITIALIZE: i64 = 1;

struct Doc {
    version: i64,
    text: String,
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    /// Whether the server has answered `initialize`.
    ready: bool,
    /// Commands that arrived before the server answered `initialize`.
    backlog: Vec<Cmd>,
}

impl Server {
    fn start(wake: &Sender<Cmd>) -> Result<Server, String> {
        let binary = binary().ok_or("crucible-lsp is not installed")?;
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("starting {}: {e}", binary.display()))?;
        let stdin = child.stdin.take().ok_or("crucible-lsp has no stdin")?;
        let stdout = child.stdout.take().ok_or("crucible-lsp has no stdout")?;
        let wake = wake.clone();
        std::thread::Builder::new()
            .name("crucible-playbooks-lsp-read".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let why = loop {
                    match read_message(&mut reader) {
                        Ok(Some(message)) => {
                            if wake.send(Cmd::Incoming(message)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break "crucible-lsp exited".to_string(),
                        Err(error) => break format!("reading from crucible-lsp: {error}"),
                    }
                };
                let _ = wake.send(Cmd::Exited(why));
            })
            .map_err(|e| format!("starting the LSP reader: {e}"))?;
        let mut server = Server {
            child,
            stdin,
            ready: false,
            backlog: Vec::new(),
        };
        server.send(&json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": {"textDocument": {"publishDiagnostics": {}}}
            }
        }))?;
        Ok(server)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    fn send(&mut self, message: &Value) -> Result<(), String> {
        self.stdin
            .write_all(&frame(message))
            .and_then(|()| self.stdin.flush())
            .map_err(|e| format!("writing to crucible-lsp: {e}"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(rx: Receiver<Cmd>, wake: Sender<Cmd>, shared: Arc<Mutex<Shared>>) {
    let mut server: Option<Server> = None;
    let mut docs: HashMap<PathBuf, Doc> = HashMap::new();
    let mut failed = false;
    loop {
        let cmd = match rx.recv_timeout(IDLE) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                if server.take().is_some() {
                    docs.clear();
                    update(&shared, |view| view.status = Status::Idle);
                }
                continue;
            }
        };
        if let Cmd::Stop = cmd {
            server = None;
            docs.clear();
            failed = false;
            update(&shared, |view| *view = View::default());
            continue;
        }
        if let Cmd::Exited(why) = cmd {
            server = None;
            failed = true;
            update(&shared, |view| view.status = Status::Unavailable(why));
            continue;
        }
        if let Cmd::Incoming(message) = cmd {
            let Some(live) = server.as_mut() else {
                continue;
            };
            if let Err(why) = receive(live, &message, &shared) {
                update(&shared, |view| view.status = Status::Unavailable(why));
            }
            if live.ready {
                for cmd in std::mem::take(&mut live.backlog) {
                    let _ = handle(live, cmd, &mut docs);
                }
            }
            continue;
        }
        if server.is_none() && !failed {
            update(&shared, |view| view.status = Status::Starting);
            match Server::start(&wake) {
                Ok(started) => server = Some(started),
                Err(why) => {
                    failed = true;
                    update(&shared, |view| view.status = Status::Unavailable(why));
                }
            }
        }
        let Some(live) = server.as_mut() else {
            continue;
        };
        if !live.ready {
            live.backlog.push(cmd);
            continue;
        }
        if let Err(why) = handle(live, cmd, &mut docs) {
            update(&shared, |view| view.status = Status::Unavailable(why));
        }
    }
}

fn handle(server: &mut Server, cmd: Cmd, docs: &mut HashMap<PathBuf, Doc>) -> Result<(), String> {
    let Cmd::Show { path, text } = cmd else {
        return Ok(());
    };
    let uri = uri(&path);
    match docs.get_mut(&path) {
        Some(doc) if doc.text == text => Ok(()),
        Some(doc) => {
            doc.version += 1;
            doc.text = text.clone();
            server.notify(
                "textDocument/didChange",
                json!({"textDocument": {"uri": uri, "version": doc.version},
                       "contentChanges": [{"text": text}]}),
            )
        }
        None => {
            server.notify(
                "textDocument/didOpen",
                json!({"textDocument": {"uri": uri, "languageId": language(&path),
                                        "version": 1, "text": text}}),
            )?;
            docs.insert(path, Doc { version: 1, text });
            Ok(())
        }
    }
}

fn receive(server: &mut Server, message: &Value, shared: &Mutex<Shared>) -> Result<(), String> {
    if let Some(method) = message.get("method").and_then(Value::as_str) {
        if method == "textDocument/publishDiagnostics"
            && let Some(params) = message.get("params")
            && let Some(path) = params.get("uri").and_then(Value::as_str).and_then(path_of)
        {
            let found = problems(params);
            update(shared, |view| {
                if found.is_empty() {
                    view.problems.remove(&path);
                } else {
                    view.problems.insert(path, found);
                }
            });
        }
        if let Some(id) = message.get("id") {
            server.send(&json!({"jsonrpc": "2.0", "id": id, "result": null}))?;
        }
        return Ok(());
    }
    if message.get("id").and_then(Value::as_i64) == Some(INITIALIZE) && !server.ready {
        server.ready = true;
        server.notify("initialized", json!({}))?;
        update(shared, |view| view.status = Status::Ready);
    }
    Ok(())
}

fn language(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("toml") => "toml",
        _ => "starlark",
    }
}

/// A `file://` URI for an absolute path, percent-encoding what a URI path cannot carry.
pub fn uri(path: &Path) -> String {
    format!(
        "file://{}",
        crate::link::encode(&path.to_string_lossy(), b"/")
    )
}

pub fn path_of(uri: &str) -> Option<PathBuf> {
    crate::link::decode(uri.strip_prefix("file://")?).map(PathBuf::from)
}

/// One message framed with its `Content-Length` header.
pub fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// The next message, or `None` at the end of the stream.
pub fn read_message(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            if length.is_some() {
                break;
            }
            continue;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0; length.unwrap_or_default()];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// The byte offset of `pos` in `text`, clamped to its line's end and the text's end.
pub fn offset(text: &str, pos: Pos) -> usize {
    let mut start = 0;
    for _ in 0..pos.line {
        match text[start..].find('\n') {
            Some(at) => start += at + 1,
            None => return text.len(),
        }
    }
    let line = text[start..].split('\n').next().unwrap_or_default();
    let mut units = 0;
    for (at, c) in line.char_indices() {
        if units >= pos.character as usize {
            return start + at;
        }
        units += c.len_utf16();
    }
    start + line.trim_end_matches('\r').len().min(line.len())
}

fn pos(value: &Value) -> Pos {
    let field = |name: &str| {
        value
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or_default()
    };
    Pos {
        line: field("line"),
        character: field("character"),
    }
}

fn problems(params: &Value) -> Vec<Problem> {
    let Some(list) = params.get("diagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<Problem> = list
        .iter()
        .filter_map(|d| {
            let range = d.get("range")?;
            Some(Problem {
                start: pos(range.get("start")?),
                end: pos(range.get("end")?),
                severity: match d.get("severity").and_then(Value::as_u64) {
                    Some(2) => Severity::Warning,
                    Some(3) => Severity::Info,
                    Some(4) => Severity::Hint,
                    _ => Severity::Error,
                },
                message: d.get("message")?.as_str()?.to_string(),
                unnecessary: d
                    .get("tags")
                    .and_then(Value::as_array)
                    .is_some_and(|tags| tags.iter().any(|t| t.as_u64() == Some(1))),
            })
        })
        .collect();
    out.sort_by_key(|p| (p.start.line, p.start.character, p.severity));
    out
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::lsp::{
        Lsp, Pos, Severity, Status, View, binary, frame, offset, path_of, problems, read_message,
        uri,
    };

    #[test]
    fn messages_round_trip_through_their_framing() {
        let a = json!({"jsonrpc": "2.0", "id": 1, "result": {"ok": "é"}});
        let b = json!({"jsonrpc": "2.0", "method": "x"});
        let mut bytes = frame(&a);
        let body = b.to_string();
        bytes.extend(format!("content-length: {}\r\nContent-Type: x\r\n\r\n", body.len()).bytes());
        bytes.extend(body.bytes());
        let mut reader = BufReader::new(&bytes[..]);
        assert_eq!(read_message(&mut reader).unwrap(), Some(a));
        assert_eq!(read_message(&mut reader).unwrap(), Some(b));
        assert_eq!(read_message(&mut reader).unwrap(), None);
        let mut truncated = BufReader::new(&b"Content-Length: 10\r\n\r\n{}"[..]);
        assert!(read_message(&mut truncated).is_err());
    }

    #[test]
    fn positions_count_utf16_columns() {
        let text = "ab\nü😀x\r\nend";
        let at = |line, character| offset(text, Pos { line, character });
        assert_eq!(at(0, 0), 0);
        assert_eq!(at(1, 0), 3);
        assert_eq!(at(1, 1), 5, "past ü");
        assert_eq!(at(1, 3), 9, "past the surrogate pair");
        assert_eq!(at(1, 99), 10, "the line's end, before \\r");
        assert_eq!(at(2, 3), text.len());
        assert_eq!(at(9, 0), text.len());
    }

    #[test]
    fn uris_carry_any_path() {
        let path = Path::new("/tmp/a b/ü#?.star");
        assert_eq!(uri(path), "file:///tmp/a%20b/%C3%BC%23%3F.star");
        assert_eq!(path_of(&uri(path)), Some(path.to_path_buf()));
        assert_eq!(path_of("https://x"), None);
        assert_eq!(path_of("file:///bad%2"), None);
    }

    #[test]
    fn diagnostics_read_from_the_wire_in_order() {
        let found = problems(&json!({"diagnostics": [
            {"range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 4}},
             "severity": 2, "message": "later", "tags": [1]},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}},
             "message": "first"},
            {"message": "no range"}
        ]}));
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].message.as_str(), found[0].severity),
            ("first", Severity::Error)
        );
        assert!(found[1].unnecessary && found[1].severity == Severity::Warning);
        assert!(problems(&json!({})).is_empty());
    }

    fn wait(lsp: &Lsp, until: impl Fn(&View) -> bool) -> View {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = View::default();
        while Instant::now() < deadline {
            if let Some((_, view)) = lsp.take(u64::MAX) {
                if until(&view) {
                    return view;
                }
                last = view;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out; last view {last:?}");
    }

    fn errors(view: &View, path: &Path) -> usize {
        view.problems.get(path).map_or(0, |found| {
            found
                .iter()
                .filter(|p| p.severity == Severity::Error)
                .count()
        })
    }

    /// Against the real server: needs `crucible-lsp` installed (`cargo install --path crucible-lsp`).
    #[test]
    fn the_real_server_checks_a_pack_as_it_changes() {
        assert!(binary().is_some(), "crucible-lsp is not installed");
        let dir = tempfile::tempdir().unwrap();
        let pack: PathBuf = dir.path().canonicalize().unwrap();
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/playbook");
        copy(&example, &pack);
        let workflow = pack.join("workflow.star");
        let good = std::fs::read_to_string(&workflow).unwrap();
        let broken = good.replacen("draft = agent(", "draft = agent((", 1);

        let lsp = Lsp::spawn();
        lsp.show(&workflow, &broken);
        let view = wait(&lsp, |v| errors(v, &workflow) > 0);
        assert_eq!(view.status, Status::Ready);
        let error = view.problems[&workflow]
            .iter()
            .find(|p| p.severity == Severity::Error)
            .unwrap();
        let line = broken.lines().position(|l| l.contains("agent((")).unwrap();
        assert!(error.start.line as usize >= line, "{error:?}");

        lsp.show(&workflow, &good);
        let view = wait(&lsp, |v| errors(v, &workflow) == 0);
        assert_eq!(view.status, Status::Ready);

        let unknown = good.replacen("name = \"draft\"", "nmae = \"draft\"", 1);
        lsp.show(&workflow, &unknown);
        let view = wait(&lsp, |v| errors(v, &workflow) > 0);
        let typo = unknown.lines().position(|l| l.contains("nmae")).unwrap() as u32;
        assert!(
            view.problems[&workflow].iter().any(|p| {
                p.severity == Severity::Error
                    && p.message.contains("name")
                    && (p.start.line..=p.end.line).contains(&typo)
            }),
            "{view:?}"
        );
    }

    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }
}

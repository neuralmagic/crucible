//! The built binary over stdio, as an editor drives it.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

struct Client {
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    pending: VecDeque<Value>,
    next_id: i64,
}

impl Client {
    fn spawn() -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crucible-lsp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn crucible-lsp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (send, incoming) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let line = line.trim_end();
                    if line.is_empty() {
                        break;
                    }
                    if let Some(n) = line.strip_prefix("Content-Length: ") {
                        length = n.parse().expect("length");
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                let message: Value = serde_json::from_slice(&body).expect("json");
                if send.send(message).is_err() {
                    return;
                }
            }
        });
        Client {
            child,
            stdin,
            incoming,
            pending: VecDeque::new(),
            next_id: 0,
        }
    }

    fn send(&mut self, message: Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("write");
        self.stdin.flush().expect("flush");
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let message = self
                .next(deadline)
                .unwrap_or_else(|| panic!("no response to {method}"));
            if message.get("id") == Some(&json!(id)) {
                assert!(message.get("error").is_none(), "{method}: {message}");
                return message["result"].clone();
            }
            self.pending.push_back(message);
        }
    }

    fn next(&mut self, deadline: Instant) -> Option<Value> {
        let wait = deadline.checked_duration_since(Instant::now())?;
        self.incoming.recv_timeout(wait).ok()
    }

    /// The first diagnostics published for `uri` that satisfy `done`.
    fn diagnostics(&mut self, uri: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + TIMEOUT;
        let mut last = None;
        loop {
            let message = match self.pending.pop_front() {
                Some(message) => message,
                None => self
                    .next(deadline)
                    .unwrap_or_else(|| panic!("no diagnostics for {uri}; last {last:?}")),
            };
            if message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == uri
            {
                let found = message["params"]["diagnostics"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if done(&found) {
                    return found;
                }
                last = Some(found);
            }
        }
    }

    fn shutdown(mut self) {
        self.request("shutdown", Value::Null);
        self.notify("exit", Value::Null);
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                assert!(status.success(), "{status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        panic!("crucible-lsp did not exit");
    }
}

fn uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn codes(diagnostics: &[Value]) -> Vec<String> {
    let mut codes: Vec<String> = diagnostics
        .iter()
        .map(|d| d["code"].as_str().unwrap_or_default().to_owned())
        .collect();
    codes.sort();
    codes
}

/// Position of the first `needle` in `source`, `n` characters in. ASCII sources only.
fn position(source: &str, needle: &str, n: usize) -> Value {
    let offset = source.find(needle).expect("needle") + n;
    let line = source[..offset].matches('\n').count();
    let character = offset - source[..offset].rfind('\n').map_or(0, |at| at + 1);
    json!({"line": line, "character": character})
}

fn pack() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().canonicalize().expect("canonical");
    std::fs::create_dir_all(dir.join("prompts")).expect("prompts");
    std::fs::write(
        dir.join("crucible.toml"),
        "[agent.sandbox.rust]\nimage = \"x\"\n",
    )
    .expect("manifest");
    std::fs::write(dir.join("prompts/review.md"), "Review it.\n").expect("prompt");
    (root, dir)
}

const BROKEN: &str = "params = {\"repo\": {\"type\": \"string\", \"required\": True}}\n\
review = agent(name = \"review\", prompt = prompt_file(\"prompts/review.md\"), sandbox = \"go\")\n\
extra = command(name = \"extra\", run = \"true\")\n\
workflow(type = \"playbook\", tasks = [review])\n";

const FIXED: &str = "params = {\"repo\": {\"type\": \"string\", \"required\": True}}\n\
review = agent(name = \"review\", prompt = prompt_file(\"prompts/review.md\") + param(\"repo\"), sandbox = \"rust\")\n\
extra = command(name = \"extra\", run = \"true\", depends_on = [review])\n\
workflow(type = \"playbook\", tasks = [review, extra])\n";

fn initialize(client: &mut Client, dir: &Path) {
    let result = client.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": uri(dir),
            "capabilities": {"textDocument": {"completion": {"completionItem": {"snippetSupport": true}}}},
        }),
    );
    assert_eq!(result["serverInfo"]["name"], "crucible-lsp");
    assert!(result["capabilities"]["completionProvider"].is_object());
    client.notify("initialized", json!({}));
}

#[test]
fn an_edit_session_reports_fixes_and_answers_requests() {
    let (_root, dir) = pack();
    let entry = dir.join("workflow.star");
    std::fs::write(&entry, BROKEN).expect("entry");
    let entry_uri = uri(&entry);
    let mut client = Client::spawn();
    initialize(&mut client, &dir);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": entry_uri, "languageId": "starlark", "version": 1, "text": BROKEN}}),
    );

    let found = client.diagnostics(&entry_uri, |d| codes(d).contains(&"compile".to_owned()));
    assert_eq!(
        codes(&found),
        ["compile", "undeclared-sandbox"],
        "no unused-param: the command task may read `repo` from its inputs. {found:#?}"
    );
    let compile = found
        .iter()
        .find(|d| d["code"] == "compile")
        .expect("compile");
    assert_eq!(
        compile["range"]["start"]["line"], 2,
        "the dropped task's own line"
    );
    assert_eq!(compile["source"], "crucible");

    client.notify(
        "textDocument/didChange",
        json!({"textDocument": {"uri": entry_uri, "version": 2}, "contentChanges": [{"text": FIXED}]}),
    );
    let found = client.diagnostics(&entry_uri, <[Value]>::is_empty);
    assert!(found.is_empty());

    let at = |needle, n| json!({"textDocument": {"uri": entry_uri}, "position": position(FIXED, needle, n)});
    let completion = client.request("textDocument/completion", at("agent(", 6));
    let labels: Vec<&str> = completion
        .as_array()
        .expect("items")
        .iter()
        .filter_map(|i| i["label"].as_str())
        .collect();
    assert!(
        labels.contains(&"harness") && !labels.contains(&"prompt"),
        "{labels:?}"
    );

    let hover = client.request("textDocument/hover", at("agent(", 1));
    assert!(
        hover["contents"]["value"]
            .as_str()
            .is_some_and(|v| v.contains("An agent turn")),
        "{hover}"
    );

    let signature = client.request("textDocument/signatureHelp", at("sandbox =", 2));
    let label = signature["signatures"][0]["label"].as_str().expect("label");
    assert!(label.starts_with("agent(name: str"), "{label}");
    assert!(signature["activeParameter"].is_number(), "{signature}");

    let definition = client.request("textDocument/definition", at("prompts/review.md", 3));
    assert_eq!(definition["uri"], uri(&dir.join("prompts/review.md")));

    let symbols = client.request(
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": entry_uri}}),
    );
    let names: Vec<&str> = symbols
        .as_array()
        .expect("symbols")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert_eq!(names, ["repo", "review", "extra", "workflow"]);

    let references = client.request(
        "textDocument/references",
        json!({
            "textDocument": {"uri": entry_uri},
            "position": position(FIXED, "[review", 1),
            "context": {"includeDeclaration": true},
        }),
    );
    let lines: Vec<u64> = references
        .as_array()
        .expect("locations")
        .iter()
        .filter_map(|l| l["range"]["start"]["line"].as_u64())
        .collect();
    assert_eq!(lines, [1, 2, 3], "the binding, `depends_on`, and `tasks`");

    client.shutdown();
}

#[test]
fn an_error_in_a_loaded_module_is_published_against_that_module() {
    let (_root, dir) = pack();
    let entry = "load(\"lib.star\", \"make\")\nworkflow(type = \"playbook\", tasks = [make()])\n";
    let lib = "def make():\n    return command(name = \"m\", run = \"true\", bogus = 1)\n";
    std::fs::write(dir.join("workflow.star"), entry).expect("entry");
    std::fs::write(dir.join("lib.star"), lib).expect("lib");
    let mut client = Client::spawn();
    initialize(&mut client, &dir);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&dir.join("workflow.star")), "languageId": "starlark", "version": 1, "text": entry}}),
    );
    let lib_uri = uri(&dir.join("lib.star"));
    let found = client.diagnostics(&lib_uri, |d| !d.is_empty());
    assert_eq!(codes(&found), ["compile"]);
    assert!(
        found[0]["message"]
            .as_str()
            .is_some_and(|m| m.contains("bogus")),
        "{found:#?}"
    );
    assert_eq!(found[0]["range"]["start"]["line"], 1);

    client.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": lib_uri, "languageId": "starlark", "version": 1, "text": lib}}),
    );
    let found = client.diagnostics(&lib_uri, |d| !d.is_empty());
    assert_eq!(
        codes(&found),
        ["compile"],
        "the unknown-argument lint repeats it, so it is dropped: {found:#?}"
    );
    client.shutdown();
}

#[test]
fn a_parse_error_is_reported_without_a_compile() {
    let (_root, dir) = pack();
    let path = dir.join("workflow.star");
    let mut client = Client::spawn();
    initialize(&mut client, &dir);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&path), "languageId": "starlark", "version": 1, "text": "x = agent(\n"}}),
    );
    let found = client.diagnostics(&uri(&path), |d| !d.is_empty());
    assert_eq!(codes(&found), ["parse"]);
    client.shutdown();
}

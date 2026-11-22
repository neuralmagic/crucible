//! A client for crucible-kueue's MCP endpoint. A run's command and evaluate tasks get
//! the server's URL and a token file from the engine in `CRUCIBLE_TASK_SERVERS`; each call is one
//! `tools/call` request carrying the token the file holds at that moment.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crucible_contract::task_servers::{ENV_TASK_SERVERS, TaskServer};
use serde_json::{Map, Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

/// The name the controller grants crucible-kueue under.
pub const DEFAULT_SERVER: &str = "kueue";

/// The reason Kueue gives a workload that finished successfully.
const SUCCEEDED: &str = "Succeeded";

fn failure(reason: &str) -> String {
    match reason {
        "" => "ended before the point waited for".to_string(),
        reason => format!("did not succeed: {reason}"),
    }
}

/// The longest `workload_status` waits on the server for one call.
pub const MAX_SERVER_WAIT_SECS: u64 = 50;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(MAX_SERVER_WAIT_SECS + 30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{ENV_TASK_SERVERS} is not set: the pack's [workflow] servers must name {0:?}")]
    NoServers(String),
    #[error("{ENV_TASK_SERVERS} is not a list of servers: {0}")]
    Servers(#[source] serde_json::Error),
    #[error("{ENV_TASK_SERVERS} names {granted:?}, not {wanted:?}")]
    NotGranted {
        wanted: String,
        granted: Vec<String>,
    },
    #[error("reading the token file {}: {source}", path.display())]
    Token {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("building the HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("calling {tool}: {source}")]
    Http {
        tool: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("{tool} was refused with HTTP {status}: {body}")]
    Refused {
        tool: String,
        status: u16,
        body: String,
    },
    #[error("{tool} answered something that is no tool result: {reason}")]
    Protocol { tool: String, reason: String },
    #[error("{tool} failed with JSON-RPC error {code}: {message}")]
    Rpc {
        tool: String,
        code: i64,
        message: String,
    },
    #[error("{tool} answered {status}: {body}")]
    Tool {
        tool: String,
        /// The result's `status`: `refused`, `rejected`, `not_found`, `error` or `failed`.
        status: String,
        /// Whether the server says the same call may succeed later.
        retryable: bool,
        body: Box<Value>,
    },
    #[error("{workload} did not reach {until} within {waited:?}")]
    WaitTimeout {
        /// `<kind>/<name>`.
        workload: String,
        until: String,
        waited: Duration,
        /// The last status report.
        body: Box<Value>,
    },
    #[error("{workload} cannot start: a pod is stuck")]
    Stuck {
        /// `<kind>/<name>`.
        workload: String,
        /// The last status report; `problem_pods` says why.
        body: Box<Value>,
    },
    #[error("{workload} {}", failure(.reason))]
    Failed {
        /// `<kind>/<name>`.
        workload: String,
        /// The reason Kueue gives for how it ended; empty for one that was stopped.
        reason: String,
        /// The last status report.
        body: Box<Value>,
    },
    #[error("{workload} has no collected results ({state}){}", detail(.body))]
    NoResults {
        /// `<kind>/<name>`.
        workload: String,
        /// `pending`, `withheld` or `failed`.
        state: String,
        /// What `workload_results` answered.
        body: Box<Value>,
    },
    #[error("the results name {path:?}, which is no file below the destination")]
    ResultsPath { path: String },
    #[error("writing {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The `error` or `note` a `workload_results` reply explains itself with.
fn detail(body: &Value) -> String {
    ["error", "note"]
        .iter()
        .find_map(|key| body.get(key).and_then(Value::as_str))
        .map(|text| format!(": {text}"))
        .unwrap_or_default()
}

/// Writes each file of a `workload_results` reply's `contents` below `dest` and returns their
/// paths relative to it.
async fn write_contents(contents: &[Value], dest: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut written = Vec::new();
    for file in contents {
        let path = file.get("path").and_then(Value::as_str).unwrap_or_default();
        let refused = || Error::ResultsPath {
            path: path.to_string(),
        };
        let relative = Path::new(path);
        let below = relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
        if path.is_empty() || !below {
            return Err(refused());
        }
        let malformed = |reason: &str| Error::Protocol {
            tool: "workload_results".into(),
            reason: format!("{path}: {reason}"),
        };
        let data = match (file.get("text"), file.get("base64")) {
            (Some(Value::String(text)), _) => text.clone().into_bytes(),
            (None, Some(Value::String(encoded))) => STANDARD
                .decode(encoded)
                .map_err(|_| malformed("its base64 does not decode"))?,
            _ => return Err(malformed("neither text nor base64")),
        };
        let full = dest.join(relative);
        let write = |source| Error::Write {
            path: full.clone(),
            source,
        };
        if let Some(parent) = full.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(write)?;
        }
        tokio::fs::write(&full, data).await.map_err(write)?;
        written.push(relative.to_path_buf());
    }
    Ok(written)
}

/// Where each request's bearer token comes from.
#[derive(Debug, Clone)]
pub enum Token {
    /// A file the engine rewrites before the token expires; read for every request.
    File(PathBuf),
    Static(String),
}

#[derive(Debug, Clone)]
pub struct Client {
    url: String,
    token: Token,
    http: reqwest::Client,
}

impl Client {
    /// The client for the server the engine granted this task under `name`.
    pub fn from_env(name: &str) -> Result<Self, Error> {
        let raw = std::env::var(ENV_TASK_SERVERS).unwrap_or_default();
        Self::from_servers(&raw, name)
    }

    /// The client for `name` among the servers `raw` lists, as [`ENV_TASK_SERVERS`] carries them.
    pub fn from_servers(raw: &str, name: &str) -> Result<Self, Error> {
        let servers: Vec<TaskServer> =
            crucible_contract::task_servers::decode(raw).map_err(Error::Servers)?;
        if servers.is_empty() {
            return Err(Error::NoServers(name.to_string()));
        }
        let granted = servers.iter().map(|s| s.name.to_string()).collect();
        let server = servers
            .into_iter()
            .find(|s| s.name.as_str() == name)
            .ok_or_else(|| Error::NotGranted {
                wanted: name.to_string(),
                granted,
            })?;
        Self::new(server.url, Token::File(server.token_file))
    }

    pub fn new(url: impl Into<String>, token: Token) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(Error::Client)?;
        Ok(Self {
            url: url.into(),
            token,
            http,
        })
    }

    async fn bearer(&self) -> Result<String, Error> {
        match &self.token {
            Token::Static(token) => Ok(token.clone()),
            Token::File(path) => tokio::fs::read_to_string(path)
                .await
                .map(|t| t.trim().to_string())
                .map_err(|source| Error::Token {
                    path: path.clone(),
                    source,
                }),
        }
    }

    /// Call `tool` with `arguments` and return its JSON result. Any `status` but `ok` is
    /// [`Error::Tool`].
    pub async fn call(&self, tool: &str, arguments: Map<String, Value>) -> Result<Value, Error> {
        let http = |source| Error::Http {
            tool: tool.to_string(),
            source,
        };
        let protocol = |reason: String| Error::Protocol {
            tool: tool.to_string(),
            reason,
        };
        let response = self
            .http
            .post(&self.url)
            .bearer_auth(self.bearer().await?)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": tool, "arguments": arguments},
            }))
            .send()
            .await
            .map_err(http)?;
        let status = response.status();
        let event_stream = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.starts_with("text/event-stream"));
        let body = response.text().await.map_err(http)?;
        if !status.is_success() {
            return Err(Error::Refused {
                tool: tool.to_string(),
                status: status.as_u16(),
                body: body.trim().to_string(),
            });
        }
        let message = match event_stream {
            true => last_event(&body).ok_or_else(|| protocol("an empty event stream".into()))?,
            false => body,
        };
        let message: Value =
            serde_json::from_str(&message).map_err(|e| protocol(format!("not JSON: {e}")))?;
        if let Some(error) = message.get("error") {
            return Err(Error::Rpc {
                tool: tool.to_string(),
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        let result = message
            .get("result")
            .ok_or_else(|| protocol("no result".into()))?;
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("no text content".into()))?;
        let value: Value = serde_json::from_str(text)
            .map_err(|e| protocol(format!("content is not JSON: {e}")))?;
        let failed = result.get("isError").and_then(Value::as_bool) == Some(true);
        let answered = value.get("status").and_then(Value::as_str).unwrap_or("ok");
        if failed || answered != "ok" {
            return Err(Error::Tool {
                tool: tool.to_string(),
                status: answered.to_string(),
                retryable: value.get("retryable").and_then(Value::as_bool) == Some(true),
                body: Box::new(value),
            });
        }
        Ok(value)
    }

    /// Write the files a Job planned with `results` left in its results directory below `dest`,
    /// from its newest pod or the one named, and return their paths relative to `dest`. A Job
    /// whose files are not collected is [`Error::NoResults`]: its containers have not exited,
    /// its files were too large or too many, or the collector failed.
    pub async fn results(
        &self,
        kind: &str,
        name: &str,
        dest: &Path,
        namespace: Option<&str>,
        pod: Option<&str>,
    ) -> Result<Vec<PathBuf>, Error> {
        let mut arguments = Map::new();
        arguments.insert("kind".into(), json!(kind));
        arguments.insert("name".into(), json!(name));
        arguments.insert("all".into(), json!(true));
        arguments.insert("max_bytes".into(), json!(u64::MAX));
        for (key, value) in [("namespace", namespace), ("pod", pod)] {
            if let Some(value) = value {
                arguments.insert(key.into(), json!(value));
            }
        }
        let reply = self.call("workload_results", arguments).await?;
        let state = reply
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let complete = reply.get("truncated").and_then(Value::as_bool) == Some(false);
        let contents = reply.get("contents").and_then(Value::as_array);
        match (state, complete, contents) {
            ("collected", true, Some(contents)) => write_contents(contents, dest).await,
            _ => Err(Error::NoResults {
                workload: format!("{kind}/{name}"),
                state: state.to_string(),
                body: Box::new(reply),
            }),
        }
    }

    /// Dropping the future stops the wait and leaves the workload as it is.
    ///
    /// Poll `workload_status` until the workload reaches `until` (`admitted`, `running`, `ready`
    /// or `finished`) and return that report. One that finishes without succeeding, or ends or is
    /// stopped short of `until`, is [`Error::Failed`]; one with a pod that cannot start is
    /// [`Error::Stuck`]; `timeout` passing is [`Error::WaitTimeout`].
    pub async fn wait(
        &self,
        kind: &str,
        name: &str,
        until: &str,
        namespace: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, Error> {
        let started = Instant::now();
        loop {
            let left = timeout.saturating_sub(started.elapsed());
            let mut arguments = Map::new();
            arguments.insert("kind".into(), json!(kind));
            arguments.insert("name".into(), json!(name));
            arguments.insert("until".into(), json!(until));
            arguments.insert(
                "timeout_seconds".into(),
                json!(left.as_secs().clamp(1, MAX_SERVER_WAIT_SECS)),
            );
            if let Some(namespace) = namespace {
                arguments.insert("namespace".into(), json!(namespace));
            }
            let report = self.call("workload_status", arguments).await?;
            let ended = report
                .pointer("/wait/ended")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Protocol {
                    tool: "workload_status".into(),
                    reason: "a waited status carries no wait.ended".into(),
                })?
                .to_string();
            let workload = format!("{kind}/{name}");
            let reason = || {
                report
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let finished = report.get("phase").and_then(Value::as_str) == Some("finished");
            match ended.as_str() {
                "reached" if finished && reason() != SUCCEEDED => {
                    return Err(Error::Failed {
                        workload,
                        reason: reason(),
                        body: Box::new(report),
                    });
                }
                "reached" => return Ok(report),
                "stuck" => {
                    return Err(Error::Stuck {
                        workload,
                        body: Box::new(report),
                    });
                }
                "timeout" if started.elapsed() < timeout => {}
                "timeout" => {
                    return Err(Error::WaitTimeout {
                        workload,
                        until: until.to_string(),
                        waited: started.elapsed(),
                        body: Box::new(report),
                    });
                }
                _ => {
                    return Err(Error::Failed {
                        workload,
                        reason: reason(),
                        body: Box::new(report),
                    });
                }
            }
        }
    }
}

/// The `data` of the last event in a server-sent event stream.
fn last_event(stream: &str) -> Option<String> {
    stream
        .split("\n\n")
        .filter_map(|event| {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|data| data.strip_prefix(' ').unwrap_or(data))
                .collect();
            (!data.is_empty()).then(|| data.join("\n"))
        })
        .filter(|data| !data.trim().is_empty())
        .last()
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn the_last_event_with_data_is_the_message() {
        let stream = "id: 0\nretry: 3000\ndata:\n\nid: 1\ndata: {\"a\":\ndata: 1}\n\n";
        assert_eq!(last_event(stream).as_deref(), Some("{\"a\":\n1}"));
        assert_eq!(
            last_event("data: {\"a\":1}\n\n").as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(last_event(": keep-alive\n\n"), None);
    }

    #[tokio::test]
    async fn results_are_written_below_the_destination_as_the_bytes_collected() {
        let dir = tempfile::tempdir().unwrap();
        let contents = [
            json!({"path": "result.json", "bytes": 9, "text": "{\"p50\":1}"}),
            json!({"path": "runs/a b/trace.bin", "bytes": 3, "base64": "//4A"}),
            json!({"path": "runs/empty", "bytes": 0, "text": ""}),
        ];
        let written = write_contents(&contents, dir.path()).await.unwrap();
        assert_eq!(
            written,
            ["result.json", "runs/a b/trace.bin", "runs/empty"].map(PathBuf::from)
        );
        let read = |path: &str| std::fs::read(dir.path().join(path)).unwrap();
        assert_eq!(read("result.json"), b"{\"p50\":1}");
        assert_eq!(read("runs/a b/trace.bin"), [0xff, 0xfe, 0x00]);
        assert_eq!(read("runs/empty"), b"");
    }

    #[tokio::test]
    async fn a_result_naming_a_path_outside_the_destination_is_refused_and_not_written() {
        let outer = tempfile::tempdir().unwrap();
        let dest = outer.path().join("dest");
        let absolute = outer.path().join("absolute").display().to_string();
        for path in [
            "../escaped",
            "a/../../escaped",
            absolute.as_str(),
            "",
            "./a",
        ] {
            let contents = [json!({"path": path, "bytes": 1, "text": "x"})];
            let refused = write_contents(&contents, &dest).await.unwrap_err();
            assert!(
                matches!(&refused, Error::ResultsPath { path: got } if got == path),
                "{path}: {refused}"
            );
        }
        for malformed in [
            json!({"path": "a", "bytes": 1, "base64": "!"}),
            json!({"path": "a", "bytes": 1}),
        ] {
            assert!(matches!(
                write_contents(&[malformed], &dest).await,
                Err(Error::Protocol { .. })
            ));
        }
        assert_eq!(std::fs::read_dir(outer.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_job_without_collected_results_says_why() {
        let error = |state: &str, body: Value| {
            Error::NoResults {
                workload: "Job/bench".into(),
                state: state.into(),
                body: Box::new(body),
            }
            .to_string()
        };
        assert_eq!(
            error("withheld", json!({"note": "its files total 9 bytes"})),
            "Job/bench has no collected results (withheld): its files total 9 bytes"
        );
        assert_eq!(
            error("failed", json!({"error": "digest", "note": "n"})),
            "Job/bench has no collected results (failed): digest"
        );
        assert_eq!(
            error("pending", json!({})),
            "Job/bench has no collected results (pending)"
        );
    }

    #[test]
    fn the_client_is_the_named_server_of_the_task_env() {
        let raw = r#"[{"name":"kueue","url":"http://k/mcp","token_file":"/run/t/kueue"}]"#;
        let client = Client::from_servers(raw, "kueue").unwrap();
        assert_eq!(client.url, "http://k/mcp");
        assert!(matches!(&client.token, Token::File(p) if p.as_os_str() == "/run/t/kueue"));
        assert_eq!(
            Client::from_servers(raw, "other").unwrap_err().to_string(),
            "CRUCIBLE_TASK_SERVERS names [\"kueue\"], not \"other\""
        );
        assert!(matches!(
            Client::from_servers("", "kueue"),
            Err(Error::NoServers(_))
        ));
        assert!(matches!(
            Client::from_servers("{", "kueue"),
            Err(Error::Servers(_))
        ));
    }
}

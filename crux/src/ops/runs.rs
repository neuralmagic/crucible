//! Runs: the leaderboard, one run's graph, log, and captured files, and the deployed build.

use crate::client::Client;
use crate::dto;
use crate::ops::{Operation, pretty};
use crate::render;
use anyhow::{Context, Result};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Runs {
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Status, e.g. `running`, `finished`, `incomplete`. Omit for all.")]
    pub status: Option<String>,
    /// Repository, owner/repo.
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Repository, `owner/repo`.")]
    pub repo: Option<String>,
    /// Only runs dispatched to this cluster target.
    #[arg(long)]
    #[serde(default)]
    pub dispatch_target: Option<String>,
    #[arg(long)]
    #[serde(default)]
    #[schemars(description = "Max rows. The controller's own default is 50.")]
    pub limit: Option<i64>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Runs {
    type Output = Vec<dto::RunRow>;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.runs(
            self.status.as_deref(),
            self.repo.as_deref(),
            self.dispatch_target.as_deref(),
            self.limit,
        )
        .await
    }

    fn render(&self, _c: &Client, list: Self::Output) -> Result<String> {
        Ok(render::runs(&list))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// One run with its candidates. Always JSON.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Run {
    #[schemars(description = "Run id, as `crucible_runs` lists it.")]
    pub run_id: String,
}

impl Operation for Run {
    type Output = Value;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.run(&self.run_id).await
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        pretty(&out)
    }

    fn json(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct RunLog {
    pub run_id: String,
    /// Byte offset to resume from, as the previous window's `cursor=` line reported it.
    #[arg(long, default_value = "0")]
    #[serde(default)]
    #[schemars(
        description = "Byte offset to resume from, as a previous window's `cursor=` line reported it. Omit to\nstart at the first line of the log."
    )]
    pub cursor: Option<u64>,
    /// Lines in this window. Capped at 200, which is also the default.
    #[arg(long, conflicts_with = "follow")]
    #[serde(default)]
    pub limit: Option<usize>,
    /// Keep printing new lines until the run is no longer running.
    #[arg(long, short = 'f', conflicts_with = "json")]
    #[serde(skip)]
    pub follow: bool,
    /// Seconds between polls under --follow.
    #[arg(long, default_value_t = default_interval(), requires = "follow")]
    #[serde(skip, default = "default_interval")]
    pub interval: u64,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

fn default_interval() -> u64 {
    5
}

impl Operation for RunLog {
    type Output = dto::RunLog;

    async fn call(&self, c: &Client) -> Result<Value> {
        let limit = self
            .limit
            .unwrap_or(render::LOG_WINDOW_LINES)
            .clamp(1, render::LOG_WINDOW_LINES);
        c.run_log_from(
            &self.run_id,
            self.cursor.unwrap_or(0),
            Some(limit),
            Some(render::LOG_WINDOW_BYTES),
        )
        .await
    }

    fn render(&self, _c: &Client, log: Self::Output) -> Result<String> {
        render::run_log(&self.run_id, &log, self.cursor.unwrap_or(0))
    }

    fn json(&self) -> bool {
        self.json
    }
}

impl RunLog {
    /// Follow the log until the run leaves `running`, printing each new line as it appears.
    pub async fn follow(
        &self,
        c: &Client,
        interval: Duration,
        out: &mut (impl std::io::Write + ?Sized),
    ) -> Result<String> {
        let mut at = self.cursor.unwrap_or(0);
        let mut lines = 0usize;
        let mut headed = false;
        let mut mid_line = false;
        loop {
            let detail: dto::RunDetail = c.run(&self.run_id).await?;
            loop {
                let log: dto::RunLog = c.run_log_from(&self.run_id, at, None, None).await?;
                if log.total_bytes < at {
                    if std::mem::take(&mut mid_line) {
                        writeln!(out)?;
                    }
                    writeln!(
                        out,
                        "-- the log is shorter than byte {at}; following it from its start"
                    )?;
                    at = 0;
                    continue;
                }
                if let Some(text) = log.text.as_deref().filter(|t| !t.is_empty()) {
                    if !headed {
                        writeln!(
                            out,
                            "run {} [{}] following from byte {at}",
                            self.run_id, log.dispatch
                        )?;
                        headed = true;
                    }
                    out.write_all(text.as_bytes())?;
                    lines += text.matches('\n').count();
                    mid_line = !text.ends_with('\n');
                }
                at = log.resume()?;
                if log.next.is_none() {
                    break;
                }
            }
            out.flush()?;
            if !matches!(detail.run.status.as_str(), "running" | "pending") {
                if mid_line {
                    writeln!(out)?;
                }
                return Ok(format!(
                    "-- run {} is {}, {lines} lines seen\n",
                    self.run_id, detail.run.status
                ));
            }
            tokio::time::sleep(interval).await;
        }
    }
}

/// A run's engine session, one JSON event per line exactly as the engine wrote it.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct RunSession {
    pub run_id: String,
    /// Stream a running run's events as they land, until it stops. A run that is not running
    /// prints its stored session instead.
    #[arg(long, short = 'f')]
    pub follow: bool,
}

impl Operation for RunSession {
    type Output = String;

    async fn call(&self, c: &Client) -> Result<Value> {
        Ok(Value::String(c.run_session(&self.run_id).await?))
    }

    fn render(&self, _c: &Client, text: Self::Output) -> Result<String> {
        Ok(text)
    }

    fn json(&self) -> bool {
        false
    }
}

impl RunSession {
    /// Relay the live session lines to `out` until the run stops.
    pub async fn follow(&self, c: &Client, out: &mut (dyn std::io::Write + Send)) -> Result<()> {
        c.follow_session(&self.run_id, &mut |line| {
            writeln!(out, "{line}")?;
            out.flush()?;
            Ok(())
        })
        .await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct RunFiles {
    pub run_id: String,
    #[arg(long)]
    pub json: bool,
}

impl Operation for RunFiles {
    type Output = dto::RunFiles;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.run_files(&self.run_id).await
    }

    fn render(&self, _c: &Client, listing: Self::Output) -> Result<String> {
        Ok(render::run_files(&self.run_id, &listing))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Fetch one captured file. With `out`, write it there and say so; without, print it, which is
/// only honest for text: a file that is not UTF-8 says so and names `--out` rather than spraying
/// bytes at a terminal.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct RunFile {
    pub run_id: String,
    /// `<task>/<declared path>`, fan-out brackets included.
    pub key: String,
    /// Write the content here instead of to stdout. Required for a file that is not text.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

impl Operation for RunFile {
    type Output = String;

    async fn call(&self, c: &Client) -> Result<Value> {
        let bytes = c.run_file(&self.run_id, &self.key).await?;
        let text = match &self.out {
            Some(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                std::fs::write(path, &bytes)
                    .with_context(|| format!("writing {}", path.display()))?;
                format!("wrote {} bytes to {}\n", bytes.len(), path.display())
            }
            None => match String::from_utf8(bytes) {
                Ok(text) => text,
                Err(e) => format!(
                    "{} is {} bytes of non-text content; fetch it with --out <path>\n",
                    self.key,
                    e.as_bytes().len()
                ),
            },
        };
        Ok(Value::String(text))
    }

    fn render(&self, _c: &Client, text: Self::Output) -> Result<String> {
        Ok(text)
    }

    fn json(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct Graph {
    pub run_id: String,
    /// Emit a mermaid flowchart instead of the ASCII table.
    #[arg(long)]
    #[serde(default)]
    pub mermaid: bool,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for Graph {
    type Output = dto::RunGraph;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.graph(&self.run_id).await
    }

    fn render(&self, _c: &Client, g: Self::Output) -> Result<String> {
        Ok(if self.mermaid {
            render::graph_mermaid(&g)
        } else {
            render::graph(&self.run_id, &g)
        })
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// What build the controller is running, and whether it is the commit the caller expected.
///
/// A mismatch is an error, not a line of prose: the whole point is to be a gate a script can
/// stand on, and a zero exit beside the word STALE would be read by exactly nobody.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct Deployed {
    /// The commit you expect to be running. Compared by prefix, so a short sha works.
    #[arg(long)]
    pub expect: Option<String>,
    #[arg(long)]
    pub json: bool,
}

impl Operation for Deployed {
    type Output = dto::Version;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.version().await
    }

    fn render(&self, c: &Client, v: Self::Output) -> Result<String> {
        let report = render::deployed(c.base(), &v, self.expect.as_deref());
        match self.expect.as_deref() {
            Some(want) if !render::same_commit(&v.git_sha, want) => {
                anyhow::bail!(
                    "{report}\nthe controller is not running {}; it is running {}",
                    want.trim(),
                    if v.git_sha.is_empty() {
                        "unknown"
                    } else {
                        &v.git_sha
                    }
                )
            }
            _ => Ok(report),
        }
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[cfg(test)]
mod tests {
    use crate::ops::runs::*;

    use crate::ops::test_client;

    /// A log that grows on each status read, served from the byte asked in windows of at most
    /// `window` bytes.
    #[derive(Clone)]
    struct Growing {
        log: std::sync::Arc<str>,
        window: usize,
        status_reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        log_reads: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    }

    impl Growing {
        const SETTLES_AFTER: usize = 3;

        fn visible(&self) -> usize {
            let reads = self.status_reads.load(std::sync::atomic::Ordering::SeqCst);
            (self.log.len() * reads / Self::SETTLES_AFTER).min(self.log.len())
        }
    }

    async fn serve(growing: Growing) -> String {
        use axum::extract::{Query, State};
        use axum::routing::get;

        async fn log(
            State(growing): State<Growing>,
            Query(query): Query<std::collections::HashMap<String, String>>,
        ) -> axum::Json<Value> {
            let from: usize = query
                .get("from")
                .expect("a follow reads from an offset")
                .parse()
                .expect("a byte offset");
            growing.log_reads.lock().expect("lock").push(from as u64);
            let visible = &growing.log[..growing.visible()];
            let start = from.min(visible.len());
            let end = (start + growing.window).min(visible.len());
            let end = if end == visible.len() {
                end
            } else {
                visible[start..end]
                    .rfind('\n')
                    .map_or(end, |at| start + at + 1)
            };
            axum::Json(serde_json::json!({
                "run_id": "r1",
                "dispatch": "pod",
                "text": &visible[start..end],
                "truncated": start > 0 || end < visible.len(),
                "start": start,
                "next": (end < visible.len()).then_some(end),
                "total_bytes": visible.len(),
                "source": "live_pod",
            }))
        }
        async fn run(State(growing): State<Growing>) -> axum::Json<Value> {
            let reads = growing
                .status_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            let status = if reads >= Growing::SETTLES_AFTER {
                "finished"
            } else {
                "running"
            };
            axum::Json(
                serde_json::json!({ "run": { "run_id": "r1", "status": status }, "candidates": [] }),
            )
        }

        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/runs/{id}/log", get(log))
            .route("/api/runs/{id}", get(run))
            .with_state(growing);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        url
    }

    fn following(cursor: u64) -> RunLog {
        RunLog {
            run_id: "r1".into(),
            cursor: Some(cursor),
            limit: None,
            follow: true,
            interval: 1,
            json: false,
        }
    }

    #[tokio::test]
    async fn follow_prints_a_long_repetitive_log_once_and_whole() {
        let log: String = (0..300)
            .map(|i| format!("[task] {{\n[task]   \"n\": {i}\n[task] }}\n"))
            .collect();
        let growing = Growing {
            log: log.as_str().into(),
            window: 512,
            status_reads: Default::default(),
            log_reads: Default::default(),
        };
        let client = test_client(&serve(growing.clone()).await);

        let mut out: Vec<u8> = Vec::new();
        let trailer = following(0)
            .follow(&client, Duration::from_millis(10), &mut out)
            .await
            .expect("follow");
        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(
            printed,
            format!("run r1 [pod] following from byte 0\n{log}"),
            "each line exactly once"
        );
        assert_eq!(trailer, "-- run r1 is finished, 900 lines seen\n");
        let reads = growing.log_reads.lock().expect("lock").clone();
        assert!(reads.len() > 3, "the log spans many windows: {reads:?}");
        assert!(
            reads.windows(2).all(|pair| pair[0] <= pair[1]),
            "no read goes back over what was printed: {reads:?}"
        );
    }

    #[tokio::test]
    async fn follow_restarts_a_log_shorter_than_its_cursor() {
        let growing = Growing {
            log: "one\ntwo".into(),
            window: 512,
            status_reads: std::sync::Arc::new(Growing::SETTLES_AFTER.into()),
            log_reads: Default::default(),
        };
        let client = test_client(&serve(growing).await);

        let mut out: Vec<u8> = Vec::new();
        following(4096)
            .follow(&client, Duration::from_millis(10), &mut out)
            .await
            .expect("follow");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "-- the log is shorter than byte 4096; following it from its start\n\
             run r1 [pod] following from byte 0\none\ntwo\n"
        );
    }
}

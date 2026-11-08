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
    /// Line to resume from, as the previous window's `cursor=` line reported it.
    #[arg(long, default_value = "0")]
    #[serde(default)]
    #[schemars(
        description = "Line to resume from, as a previous window's `cursor=` line reported it. Omit to start at\nthe first line the controller served."
    )]
    pub cursor: Option<usize>,
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
        c.run_log(&self.run_id).await
    }

    fn render(&self, _c: &Client, log: Self::Output) -> Result<String> {
        let limit = self
            .limit
            .unwrap_or(render::LOG_WINDOW_LINES)
            .clamp(1, render::LOG_WINDOW_LINES);
        Ok(render::run_log(
            &self.run_id,
            &log,
            self.cursor.unwrap_or(0),
            limit,
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

impl RunLog {
    /// Follow the log until the run leaves `running`, printing each new line as it appears.
    ///
    /// The controller serves the whole log on every read, tail-capped, so this polls and prints
    /// only what is past `cursor`. When the cap drops the head, line numbers stop meaning
    /// anything; the last printed line is then used to find where to resume, and if it is gone
    /// too, the whole window is printed with a marker rather than silently skipped.
    pub async fn follow(
        &self,
        c: &Client,
        interval: Duration,
        out: &mut (impl std::io::Write + ?Sized),
    ) -> Result<String> {
        let mut follow = Follow {
            seen: self.cursor.unwrap_or(0),
            last_line: None,
            headed: false,
        };
        loop {
            let log: dto::RunLog = c.run_log(&self.run_id).await?;
            follow.emit(&self.run_id, &log, out)?;
            let detail: dto::RunDetail = c.run(&self.run_id).await?;
            if !matches!(detail.run.status.as_str(), "running" | "pending") {
                return Ok(format!(
                    "-- run {} is {}, {} lines seen\n",
                    self.run_id, detail.run.status, follow.seen
                ));
            }
            tokio::time::sleep(interval).await;
        }
    }
}

/// The index after the last printed line inside a window whose head the controller dropped, or
/// `None` when that line is gone too.
fn resume_after(all: &[&str], last_line: Option<&str>) -> Option<usize> {
    let last = last_line?;
    all.iter().rposition(|x| *x == last).map(|pos| pos + 1)
}

/// What one `run-log -f` has printed so far: the line to resume from, the last line printed (to
/// find that point again once the controller drops the head), and whether the header is out.
struct Follow {
    seen: usize,
    last_line: Option<String>,
    headed: bool,
}

impl Follow {
    /// Print what is new in this window.
    fn emit(
        &mut self,
        run_id: &str,
        log: &dto::RunLog,
        out: &mut (impl std::io::Write + ?Sized),
    ) -> Result<()> {
        let Some(text) = log.text.as_deref().filter(|t| !t.is_empty()) else {
            return Ok(());
        };
        if !self.headed {
            writeln!(
                out,
                "run {run_id} [{}] following from line {}",
                log.dispatch, self.seen
            )?;
            self.headed = true;
        }
        let all: Vec<&str> = text.lines().collect();
        let start = if log.truncated {
            match resume_after(&all, self.last_line.as_deref()) {
                Some(pos) => pos,
                None => {
                    if self.last_line.is_some() {
                        writeln!(
                            out,
                            "-- head dropped by the controller's cap; resuming from what it kept"
                        )?;
                    }
                    0
                }
            }
        } else {
            self.seen.min(all.len())
        };
        for line in &all[start..] {
            writeln!(out, "{line}")?;
            self.last_line = Some((*line).to_string());
        }
        self.seen = if log.truncated {
            self.seen + all.len().saturating_sub(start)
        } else {
            all.len()
        };
        out.flush()?;
        Ok(())
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

    /// Following prints only what is new on each poll and stops the moment the run is no
    /// longer running, with the final status in the trailer.
    #[tokio::test]
    async fn follow_prints_new_lines_and_stops_when_the_run_settles() {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::get;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Each poll sees one more line; the run settles on the third.
        async fn log(State(polls): State<Arc<AtomicUsize>>) -> (StatusCode, String) {
            let n = polls.fetch_add(1, Ordering::SeqCst) + 1;
            let text = (1..=n)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            (
                StatusCode::OK,
                serde_json::json!({ "run_id": "r1", "dispatch": "pod", "text": text, "truncated": false })
                    .to_string(),
            )
        }
        async fn run(State(polls): State<Arc<AtomicUsize>>) -> (StatusCode, String) {
            let status = if polls.load(Ordering::SeqCst) >= 3 {
                "finished"
            } else {
                "running"
            };
            (
                StatusCode::OK,
                serde_json::json!({ "run": { "run_id": "r1", "status": status }, "candidates": [] })
                    .to_string(),
            )
        }

        let polls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/runs/{id}/log", get(log))
            .route("/api/runs/{id}", get(run))
            .with_state(polls);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let mut out: Vec<u8> = Vec::new();
        let op = RunLog {
            run_id: "r1".into(),
            cursor: Some(0),
            limit: None,
            follow: true,
            interval: 1,
            json: false,
        };
        let trailer = op
            .follow(&client, Duration::from_millis(10), &mut out)
            .await
            .expect("follow");
        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(
            printed, "run r1 [pod] following from line 0\nline 1\nline 2\nline 3\n",
            "each line exactly once"
        );
        assert_eq!(trailer, "-- run r1 is finished, 3 lines seen\n");
    }
}

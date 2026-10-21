//! A run's evidence over HTTP, the engine side (RFC-0004:C-RUN-EVIDENCE): a run pod uploads its
//! session log, captured files, git memory, and otel log to the controller as the run's own
//! evidence, and a rerun downloads its parent's into a directory laid out like a state directory.
//!
//! Uploads are best-effort and never fail the run: a failed upload is logged, and the controller
//! falls back to scraping the session from the pod log.

use crate::args::Paths;
use crucible::plan::rerun::GIT_MEMORY_DIR;
use crucible_contract::ArtifactKind;
use crucible_contract::artifact::{gzip, tar_gz_dir, untar_gz};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many times one upload is attempted before it is given up.
const MAX_ATTEMPTS: u32 = 3;

/// Base backoff between attempts (attempt N waits `BACKOFF * N`).
const BACKOFF: Duration = Duration::from_millis(300);

/// Per-request timeout. A session log is up to 128 MiB, so this is generous.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// The human credential a person's `crucible plan run --rerun-of <url>` presents when it holds no
/// run token.
const HUMAN_TOKEN_ENV: &str = "CONTROLLER_API_TOKEN";

/// Where a run uploads its evidence: the controller, the run's id, and the run's token.
struct Upload {
    base: String,
    run: String,
    token: String,
}

impl Upload {
    /// Present only when the controller injected all three: its URL, the run token, and the run's
    /// id. Anything less is a pod with nowhere to upload to.
    fn from_env() -> Option<Self> {
        Some(Upload {
            base: non_empty_env(crucible_contract::ENV_CONTROLLER_URL)?
                .trim_end_matches('/')
                .to_string(),
            run: non_empty_env(crucible_contract::ENV_RUN_NAME)?,
            token: non_empty_env(crucible_contract::ENV_RUN_TOKEN)?,
        })
    }

    fn url(&self, kind: ArtifactKind) -> String {
        format!(
            "{}/api/runs/{}/evidence/{}",
            self.base,
            crucible_contract::artifact::path_segment(&self.run),
            kind.as_str()
        )
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// Upload the finished run's evidence when the pod carries a controller and a run token: the
/// captured files, the git memory, and the otel log when the run produced them, and the session
/// log last, since the controller completes the run off it.
pub(crate) fn deliver_run_evidence(p: &Paths) {
    let Some(upload) = Upload::from_env() else {
        return;
    };
    deliver_dir(&upload, ArtifactKind::RunFiles, &p.state.join("files"));
    deliver_dir(
        &upload,
        ArtifactKind::RunCommits,
        &p.state.join(GIT_MEMORY_DIR),
    );
    let otel = p.state.join("otel.jsonl");
    if otel.exists() {
        deliver_file(&upload, ArtifactKind::OtelLog, &otel);
    }
    deliver_file(&upload, ArtifactKind::RunSession, &p.session_log);
}

/// Upload a state directory as one gzipped tar. A run that produced none uploads nothing.
fn deliver_dir(upload: &Upload, kind: ArtifactKind, dir: &Path) {
    if !dir.is_dir() {
        return;
    }
    match tar_gz_dir(dir) {
        Ok(gz) => send(upload, kind, gz),
        Err(e) => eprintln!("[crucible] packing {} as {kind} failed: {e}", dir.display()),
    }
}

fn deliver_file(upload: &Upload, kind: ArtifactKind, path: &Path) {
    match std::fs::read(path).and_then(|raw| gzip(&raw)) {
        Ok(gz) => send(upload, kind, gz),
        Err(e) => eprintln!(
            "[crucible] reading {} as {kind} failed: {e}",
            path.display()
        ),
    }
}

/// PUT one artifact, retrying anything but an oversize refusal.
fn send(upload: &Upload, kind: ArtifactKind, gz: Vec<u8>) {
    let client = match reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[crucible] building the evidence client failed: {e}");
            return;
        }
    };
    let url = upload.url(kind);
    let bytes = gz.len();
    for attempt in 1..=MAX_ATTEMPTS {
        let resp = client
            .put(&url)
            .bearer_auth(&upload.token)
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .body(gz.clone())
            .send();
        match resp {
            Ok(r) if r.status().is_success() => {
                eprintln!("[crucible] uploaded {kind} ({bytes} gzipped bytes)");
                return;
            }
            Ok(r) if r.status() == reqwest::StatusCode::PAYLOAD_TOO_LARGE => {
                eprintln!(
                    "[crucible] the controller refused {kind} as oversize: {}",
                    r.text().unwrap_or_default().trim()
                );
                return;
            }
            Ok(r) => eprintln!(
                "[crucible] uploading {kind}, attempt {attempt}/{MAX_ATTEMPTS}: HTTP {}: {}",
                r.status(),
                r.text().unwrap_or_default().trim()
            ),
            Err(e) => {
                eprintln!("[crucible] uploading {kind}, attempt {attempt}/{MAX_ATTEMPTS}: {e}")
            }
        }
        if attempt < MAX_ATTEMPTS {
            std::thread::sleep(BACKOFF * attempt);
        }
    }
    eprintln!("[crucible] gave up uploading {kind} after {MAX_ATTEMPTS} attempts");
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("building the evidence client: {0}")]
    Client(reqwest::Error),
    #[error("fetching {url}: {source}")]
    Request { url: String, source: reqwest::Error },
    #[error("fetching {url}: HTTP {status}: {body}")]
    Status {
        url: String,
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("writing the parent's {what} under {}: {source}", .dir.display())]
    Write {
        what: &'static str,
        dir: PathBuf,
        source: std::io::Error,
    },
}

/// Download the evidence of the run at `parent` (`<controller>/api/runs/<id>`) into `dest`, laid
/// out like that run's state directory: `session.jsonl`, `files/`, and `git-memory/`. Evidence
/// the controller does not hold is left absent, for the rerun's own read of the parent to name.
/// The bearer is the run token when this process holds one, else a person's controller token.
pub fn fetch_parent(parent: &url::Url, dest: &Path) -> Result<(), FetchError> {
    let token =
        non_empty_env(crucible_contract::ENV_RUN_TOKEN).or_else(|| non_empty_env(HUMAN_TOKEN_ENV));
    let client = reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(FetchError::Client)?;
    let write = |what: &'static str| {
        move |source| FetchError::Write {
            what,
            dir: dest.to_path_buf(),
            source,
        }
    };
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(write("evidence"))?;
    }
    std::fs::create_dir_all(dest).map_err(write("evidence"))?;
    let base = parent.as_str().trim_end_matches('/');
    for kind in [
        ArtifactKind::RunSession,
        ArtifactKind::RunFiles,
        ArtifactKind::RunCommits,
    ] {
        let url = format!("{base}/evidence/{}", kind.as_str());
        let mut req = client.get(&url);
        if let Some(token) = &token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().map_err(|source| FetchError::Request {
            url: url.clone(),
            source,
        })?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        if !resp.status().is_success() {
            return Err(FetchError::Status {
                url,
                status: resp.status(),
                body: resp.text().unwrap_or_default().trim().to_string(),
            });
        }
        let gz = resp
            .bytes()
            .map_err(|source| FetchError::Request { url, source })?;
        match kind {
            ArtifactKind::RunSession => {
                crucible_contract::artifact::gunzip(&gz)
                    .and_then(|raw| std::fs::write(dest.join("session.jsonl"), raw))
                    .map_err(write("session log"))?;
            }
            ArtifactKind::RunFiles => {
                untar_gz(&gz, &dest.join("files")).map_err(write("captured files"))?
            }
            _ => untar_gz(&gz, &dest.join(GIT_MEMORY_DIR)).map_err(write("git memory"))?,
        }
    }
    Ok(())
}

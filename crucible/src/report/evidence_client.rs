//! A pod's evidence over HTTP, the engine side (RFC-0004:C-RUN-EVIDENCE): a run pod uploads its
//! session log, captured files, git memory, and otel log to the controller as the run's own
//! evidence, a scope turn's pod uploads its pack and transcript, and a rerun downloads its
//! parent's evidence into a directory laid out like a state directory.
//!
//! Uploads are best-effort and never fail the pod: a failed upload is logged, a run whose session
//! did not upload leaves [`crucible_contract::RUN_SESSION_UNDELIVERED`] so its wrapper prints the
//! session for the controller to scrape from the pod log, and a turn's manifest marks the artifact
//! undelivered.

use crate::args::Paths;
use crucible::plan::rerun::GIT_MEMORY_DIR;
use crucible_contract::ArtifactKind;
use crucible_contract::artifact::{gzip, tar_gz_dir, untar_gz};
#[cfg(feature = "autoresearch")]
use crucible_contract::{ArtifactRef, content_digest};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The waits between attempts at an upload that failed transiently: a connection error, a timeout,
/// 408, 429, or a 5xx. About three minutes in all, so a router or controller restart does not
/// lose a finished run's evidence.
const RETRY_WAITS: [Duration; 8] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(32),
    Duration::from_secs(60),
    Duration::from_secs(60),
];

/// Per-request timeout. A session log is up to 128 MiB, so this is generous.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// The human credential a person's `crucible plan run --rerun-of <url>` presents when it holds no
/// run token.
const HUMAN_TOKEN_ENV: &str = "CONTROLLER_API_TOKEN";

/// What a pod uploads evidence for.
enum Target {
    Run(String),
    #[cfg(feature = "autoresearch")]
    Turn(String),
}

/// Where a pod uploads its evidence: the controller, what the evidence belongs to, and the pod's
/// token.
pub(crate) struct Upload {
    base: String,
    target: Target,
    token: String,
    waits: &'static [Duration],
}

impl Upload {
    /// A run pod's upload, present only when the controller injected its URL, the pod token, and
    /// the run's id. Anything less is a pod with nowhere to upload to.
    fn run_from_env() -> Option<Self> {
        Self::from_env(Target::Run(non_empty_env(crucible_contract::ENV_RUN_NAME)?))
    }

    /// A scope turn pod's upload, keyed by the pod's own name.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn turn_from_env() -> Option<Self> {
        Self::from_env(Target::Turn(non_empty_env(
            crucible_contract::ENV_POD_NAME,
        )?))
    }

    fn from_env(target: Target) -> Option<Self> {
        Some(Upload {
            base: non_empty_env(crucible_contract::ENV_CONTROLLER_URL)?
                .trim_end_matches('/')
                .to_string(),
            target,
            token: non_empty_env(crucible_contract::ENV_POD_TOKEN)?,
            waits: &RETRY_WAITS,
        })
    }

    fn url(&self, kind: ArtifactKind) -> String {
        let (collection, id) = match &self.target {
            Target::Run(run) => ("runs", run),
            #[cfg(feature = "autoresearch")]
            Target::Turn(pod) => ("turns", pod),
        };
        format!(
            "{}/api/evidence/{collection}/{}/{}",
            self.base,
            crucible_contract::artifact::path_segment(id),
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
    let Some(upload) = Upload::run_from_env() else {
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
    let delivered = deliver_file(&upload, ArtifactKind::RunSession, &p.session_log);
    if let Err(e) = mark_session(&p.state, delivered) {
        eprintln!(
            "[crucible] marking the session in {} failed: {e}",
            p.state.display()
        );
    }
}

/// Leave [`crucible_contract::RUN_SESSION_UNDELIVERED`] in `state` when the session did not
/// upload, and clear one an earlier start left when it did.
fn mark_session(state: &Path, delivered: bool) -> std::io::Result<()> {
    let marker = state.join(crucible_contract::RUN_SESSION_UNDELIVERED);
    if !delivered {
        return std::fs::write(&marker, b"");
    }
    match std::fs::remove_file(&marker) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Upload a state directory as one gzipped tar. A run that produced none uploads nothing.
fn deliver_dir(upload: &Upload, kind: ArtifactKind, dir: &Path) {
    if !dir.is_dir() {
        return;
    }
    match tar_gz_dir(dir) {
        Ok(gz) => {
            send(upload, kind, &gz);
        }
        Err(e) => eprintln!("[crucible] packing {} as {kind} failed: {e}", dir.display()),
    }
}

/// Upload one file gzipped. Whether the controller stored it.
fn deliver_file(upload: &Upload, kind: ArtifactKind, path: &Path) -> bool {
    match std::fs::read(path).and_then(|raw| gzip(&raw)) {
        Ok(gz) => send(upload, kind, &gz),
        Err(e) => {
            eprintln!(
                "[crucible] reading {} as {kind} failed: {e}",
                path.display()
            );
            false
        }
    }
}

/// Upload one of a scope turn's gzipped artifacts. Always returns the [`ArtifactRef`] the turn's
/// manifest carries, with the digest computed here and whether the controller stored it.
#[cfg(feature = "autoresearch")]
pub(crate) fn upload_turn_artifact(upload: &Upload, kind: ArtifactKind, gz: &[u8]) -> ArtifactRef {
    ArtifactRef {
        kind,
        digest: content_digest(gz),
        bytes: gz.len() as u64,
        delivered: send(upload, kind, gz),
    }
}

fn transient(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

/// PUT one artifact, retrying transient failures over the upload's waits. Returns whether it was
/// stored.
fn send(upload: &Upload, kind: ArtifactKind, gz: &[u8]) -> bool {
    let waits = upload.waits;
    let client = match reqwest::blocking::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[crucible] building the evidence client failed: {e}");
            return false;
        }
    };
    let url = upload.url(kind);
    let bytes = gz.len();
    let attempts = waits.len() + 1;
    for attempt in 1..=attempts {
        let resp = client
            .put(&url)
            .bearer_auth(&upload.token)
            .header(reqwest::header::CONTENT_TYPE, "application/gzip")
            .body(gz.to_vec())
            .send();
        match resp {
            Ok(r) if r.status().is_success() => {
                eprintln!("[crucible] uploaded {kind} ({bytes} gzipped bytes)");
                return true;
            }
            Ok(r) if r.status() == reqwest::StatusCode::PAYLOAD_TOO_LARGE => {
                eprintln!(
                    "[crucible] the controller refused {kind} as oversize: {}",
                    first_line(&r.text().unwrap_or_default())
                );
                return false;
            }
            Ok(r) if !transient(r.status()) => {
                eprintln!(
                    "[crucible] the controller refused {kind}: HTTP {}: {}",
                    r.status(),
                    first_line(&r.text().unwrap_or_default())
                );
                return false;
            }
            Ok(r) => eprintln!(
                "[crucible] uploading {kind}, attempt {attempt}/{attempts}: HTTP {}: {}",
                r.status(),
                first_line(&r.text().unwrap_or_default())
            ),
            Err(e) => eprintln!("[crucible] uploading {kind}, attempt {attempt}/{attempts}: {e}"),
        }
        if let Some(wait) = waits.get(attempt - 1) {
            std::thread::sleep(*wait);
        }
    }
    eprintln!("[crucible] gave up uploading {kind} after {attempts} attempts");
    false
}

/// The first non-blank line of an error body, at most 200 characters: a router's HTML error page
/// is not worth a pod log screenful.
fn first_line(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
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

/// Download the evidence of the run at `parent` (`<controller>/api/evidence/runs/<id>`) into `dest`, laid
/// out like that run's state directory: `session.jsonl`, `files/`, and `git-memory/`. Evidence
/// the controller does not hold is left absent, for the rerun's own read of the parent to name.
/// The bearer is the run token when this process holds one, else a person's controller token.
pub fn fetch_parent(parent: &url::Url, dest: &Path) -> Result<(), FetchError> {
    let token =
        non_empty_env(crucible_contract::ENV_POD_TOKEN).or_else(|| non_empty_env(HUMAN_TOKEN_ENV));
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
        let url = format!("{base}/{}", kind.as_str());
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

#[cfg(all(test, feature = "autoresearch"))]
mod tests {
    use crate::report::evidence_client::{Target, Upload, send, upload_turn_artifact};
    use crucible_contract::{ArtifactKind, content_digest};
    use std::time::Duration;

    /// A controller that answers each request in turn with the next of `statuses`; the server
    /// thread returns how many requests it served.
    fn controller(statuses: &[&'static str]) -> (String, std::thread::JoinHandle<usize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let statuses = statuses.to_vec();
        let server = std::thread::spawn(move || {
            for status in &statuses {
                let (mut s, _) = listener.accept().expect("accept");
                crate::testing::answer_one_request(&mut s, status);
            }
            statuses.len()
        });
        (base, server)
    }

    #[test]
    fn a_transient_failure_is_retried_until_the_controller_stores_it() {
        let (base, server) = controller(&[
            "503 Service Unavailable",
            "502 Bad Gateway",
            "429 Too Many Requests",
            "200 OK",
        ]);
        assert!(send(&turn(base), ArtifactKind::ScopePack, b"gz"));
        assert_eq!(server.join().expect("server"), 4);
    }

    #[test]
    fn a_refusal_is_not_retried() {
        let (base, server) = controller(&["401 Unauthorized"]);
        assert!(!send(&turn(base), ArtifactKind::ScopePack, b"gz"));
        assert_eq!(server.join().expect("server"), 1);
    }

    #[test]
    fn retries_stop_when_the_waits_run_out() {
        let (base, server) = controller(&["503 Service Unavailable"; 4]);
        assert!(!send(&turn(base), ArtifactKind::ScopePack, b"gz"));
        assert_eq!(server.join().expect("server"), 4);
    }

    #[test]
    fn an_undelivered_session_is_marked_and_a_delivered_one_clears_the_mark() {
        let state = tempfile::tempdir().expect("tempdir");
        let marker = state
            .path()
            .join(crucible_contract::RUN_SESSION_UNDELIVERED);
        crate::report::evidence_client::mark_session(state.path(), true).expect("no mark yet");
        assert!(!marker.exists());
        crate::report::evidence_client::mark_session(state.path(), false).expect("mark");
        assert!(marker.exists());
        crate::report::evidence_client::mark_session(state.path(), true).expect("clear");
        assert!(!marker.exists());
    }

    #[test]
    fn an_error_body_is_cut_to_its_first_line() {
        let page = "\n  <html>\n  <body>all pods are down</body>\n</html>";
        assert_eq!(crate::report::evidence_client::first_line(page), "<html>");
        assert_eq!(
            crate::report::evidence_client::first_line(&"x".repeat(500)).len(),
            200
        );
    }

    fn turn(base: String) -> Upload {
        Upload {
            base,
            target: Target::Turn("crucible-turn-a".to_string()),
            token: "crr_id_secret".to_string(),
            waits: &[Duration::ZERO; 3],
        }
    }

    #[test]
    fn a_turn_puts_its_artifact_under_its_pod_with_its_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().expect("accept");
            crate::testing::read_one_request(&mut s)
        });
        let gz = b"\x1f\x8bpack".to_vec();
        let art = upload_turn_artifact(&turn(base), ArtifactKind::ScopePack, &gz);
        let (line, headers, body) = server.join().expect("server");
        assert_eq!(
            line,
            "PUT /api/evidence/turns/crucible-turn-a/scope-pack HTTP/1.1"
        );
        assert!(
            headers.contains("authorization: bearer crr_id_secret"),
            "{headers}"
        );
        assert_eq!(body, gz);
        assert!(art.delivered);
        assert_eq!(art.digest, content_digest(&gz));
        assert_eq!(art.bytes, gz.len() as u64);
    }

    #[test]
    fn a_turn_whose_controller_is_gone_marks_the_artifact_undelivered() {
        let art = upload_turn_artifact(
            &turn("http://127.0.0.1:1".to_string()),
            ArtifactKind::ScopeTranscript,
            b"bytes",
        );
        assert!(!art.delivered);
        assert_eq!(art.digest, content_digest(b"bytes"));
    }

    #[test]
    fn a_turn_upload_needs_the_controller_its_name_and_a_token() {
        let _env = crucible::test_support::env_lock();
        let vars = [
            crucible_contract::ENV_CONTROLLER_URL,
            crucible_contract::ENV_POD_NAME,
            crucible_contract::ENV_POD_TOKEN,
        ];
        let set = |n: usize| {
            for (i, k) in vars.iter().enumerate() {
                if i < n {
                    unsafe { std::env::set_var(k, "x") };
                } else {
                    unsafe { std::env::remove_var(k) };
                }
            }
        };
        for n in 0..3 {
            set(n);
            assert!(Upload::turn_from_env().is_none(), "{n} of 3 set");
        }
        set(3);
        unsafe { std::env::set_var(crucible_contract::ENV_CONTROLLER_URL, "http://c:8080/") };
        let upload = Upload::turn_from_env().expect("all three set");
        assert_eq!(
            upload.url(ArtifactKind::ScopePack),
            "http://c:8080/api/evidence/turns/x/scope-pack"
        );
        set(0);
    }
}

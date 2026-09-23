//! The lazy run-artifact proxy behind `GET /api/runs/{run_id}/artifacts/{*path}`, plus the manifest
//! `GET /api/runs/{run_id}/artifacts` lists. A run's published artifacts (`session.jsonl`,
//! `RESULTS.md`, `summary.json`, `flow.json`, `flow.html`, `diffs/<file>`, and any other safe
//! relative path of up to 3 segments) live under the same
//! prefix as its `runs.session_uri` pointer (a `db://run-session/…` store pointer for
//! controller-persisted sessions, an `s3://…` URI, or a local path for adopted evidence);
//! this module resolves the object under a strict whitelist and streams it back, so the SPA can read
//! the same artifacts the SSG's per-run page rendered from without the controller mirroring them.
//!
//! S3 access is SHELLED to the engine (`crucible fetch --uri … --out …`), never done in-process: the
//! controller has no aws-sdk and never learns the bucket layout — the same async-boundary discipline
//! `ingest.rs` follows (the pod-watch downloads via the engine, then ingests the local file). The
//! engine owns the IRSA creds + GetObject policy. A local-path `session_uri` (dev) is served straight
//! off the filesystem under the identical whitelist.
//!
//! Responses carry a short `Cache-Control` and a content-derived `ETag`; a matching
//! `If-None-Match` short-circuits to `304`, and a backfilled/replaced file changes the tag.

use crate::metrics::Metrics;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::Stream;
use serde::Serialize;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_util::io::ReaderStream;
use utoipa::ToSchema;

/// `Cache-Control` for a published artifact. Records are immutable by contract, but backfills
/// happen; a short max-age + the content ETag means a swapped file reaches every browser within
/// minutes, at the price of a cheap conditional request (304) after each expiry.
const IMMUTABLE_CACHE: &str = "public, max-age=300";

/// A whitelisted run artifact. The only shapes served — everything else is a 400. A `diffs/<seg>`
/// carries exactly one safe path segment (no separators, no `.`/`..`), so it can never escape the
/// run's `diffs/` directory. `General` covers every other manifest-listable file: a relative path of
/// 1-3 safe segments, none starting with `.`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Artifact {
    Session,
    Results,
    Summary,
    FlowJson,
    FlowHtml,
    Diff(String),
    General(String),
}

impl Artifact {
    /// Parse the captured `{*path}` tail into a whitelisted artifact, or `None` (→ 400) for anything
    /// off the list: an absolute path, `..`/`.`, an empty segment, a dotfile segment, more than 3
    /// segments, or a `diffs/` entry that isn't a single safe segment (`diffs/` keeps its original
    /// one-segment rule and never falls through to the general arm).
    fn parse(raw: &str) -> Option<Artifact> {
        match raw {
            "session.jsonl" => Some(Artifact::Session),
            "RESULTS.md" => Some(Artifact::Results),
            "summary.json" => Some(Artifact::Summary),
            "flow.json" => Some(Artifact::FlowJson),
            "flow.html" => Some(Artifact::FlowHtml),
            other => {
                if other == "diffs" {
                    return None;
                }
                if let Some(seg) = other.strip_prefix("diffs/") {
                    return if crate::runs::task_evidence::safe_segment(seg) {
                        Some(Artifact::Diff(seg.to_string()))
                    } else {
                        None
                    };
                }
                let segs: Vec<&str> = other.split('/').collect();
                if (1..=3).contains(&segs.len())
                    && segs
                        .iter()
                        .all(|s| crate::runs::task_evidence::safe_segment(s) && !s.starts_with('.'))
                {
                    Some(Artifact::General(other.to_string()))
                } else {
                    None
                }
            }
        }
    }

    /// The artifact's path relative to the run prefix (an S3 key suffix or a filesystem sub-path).
    fn rel(&self) -> String {
        match self {
            Artifact::Session => "session.jsonl".to_string(),
            Artifact::Results => "RESULTS.md".to_string(),
            Artifact::Summary => "summary.json".to_string(),
            Artifact::FlowJson => "flow.json".to_string(),
            Artifact::FlowHtml => "flow.html".to_string(),
            Artifact::Diff(seg) => format!("diffs/{seg}"),
            Artifact::General(rel) => rel.clone(),
        }
    }

    fn content_type(&self) -> &'static str {
        match self {
            Artifact::Session => "application/x-ndjson",
            Artifact::Results => "text/markdown",
            Artifact::Summary | Artifact::FlowJson => "application/json",
            Artifact::FlowHtml => "text/html; charset=utf-8",
            Artifact::Diff(_) => "text/plain",
            Artifact::General(rel) => content_type_for(rel),
        }
    }

    /// CSP for the served bytes. `flow.html` is an engine-rendered page derived from agent session
    /// text; `sandbox allow-scripts` makes the browser treat it as opaque-origin (its inline
    /// interactivity runs, but it can't reach controller cookies or the API) whether it's opened
    /// directly or framed by the SPA. General html gets the same treatment; svg too, since a
    /// navigated `image/svg+xml` document also executes scripts.
    fn csp(&self) -> Option<&'static str> {
        match self {
            Artifact::FlowHtml => Some("sandbox allow-scripts"),
            Artifact::General(rel) if matches!(extension_of(rel).as_str(), "html" | "svg") => {
                Some("sandbox allow-scripts")
            }
            _ => None,
        }
    }
}

/// The final extension of a relative path, lowercased ("" when there is none).
fn extension_of(rel: &str) -> String {
    rel.rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Content type for a general artifact, by extension allowlist; anything unrecognized is served as
/// opaque bytes rather than guessed.
fn content_type_for(rel: &str) -> &'static str {
    match extension_of(rel).as_str() {
        "md" => "text/markdown",
        "json" => "application/json",
        "jsonl" => "application/x-ndjson",
        "txt" | "log" | "patch" | "diff" => "text/plain",
        "html" => "text/html; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "csv" => "text/csv",
        _ => "application/octet-stream",
    }
}

/// The run prefix a `session_uri` points under — everything up to and including the last `/` (the
/// pointer names the `session.jsonl` file; its siblings share the directory). `None` when the URI
/// carries no `/` (nothing to resolve siblings against).
pub(crate) fn prefix_of(session_uri: &str) -> Option<&str> {
    let idx = session_uri.rfind('/')?;
    Some(&session_uri[..=idx])
}

/// One manifest row: a servable path relative to the run prefix. `size_bytes` is known only for a
/// local prefix — the controller has no S3 client by design, so `s3://` entries carry no size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ArtifactEntry {
    pub path: String,
    pub size_bytes: Option<u64>,
}

/// The manifest `GET /api/runs/{run_id}/artifacts` returns: every artifact the `{*path}` proxy can
/// serve for the run, sorted by path.
#[derive(Debug, Serialize, ToSchema)]
pub struct ArtifactManifest {
    pub entries: Vec<ArtifactEntry>,
}

/// The names the engine publishes on keep for every backend — listable without any backend call.
const DERIVED_NAMES: [&str; 5] = [
    "session.jsonl",
    "RESULTS.md",
    "summary.json",
    "flow.json",
    "flow.html",
];

/// Build a run's artifact manifest. A local prefix is walked (so future artifacts like
/// `codegen-out/…` show up as the engine publishes them); an `s3://` prefix gets only the derived
/// list — the standard names plus `diffs/iter-<N>.patch` for each deep iteration the run recorded;
/// a `db://run-session/…` prefix holds exactly the session log.
pub(crate) async fn build_manifest(prefix: &str, diff_iters: &[i64]) -> ArtifactManifest {
    let mut entries = if prefix.starts_with(crate::runs::blob_store::DB_SESSION_URI_PREFIX) {
        vec![ArtifactEntry {
            path: "session.jsonl".to_string(),
            size_bytes: None,
        }]
    } else if prefix.starts_with("s3://") {
        derived_entries(diff_iters)
    } else {
        walk_local(std::path::Path::new(prefix)).await
    };
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    ArtifactManifest { entries }
}

fn derived_entries(diff_iters: &[i64]) -> Vec<ArtifactEntry> {
    DERIVED_NAMES
        .iter()
        .map(|n| n.to_string())
        .chain(diff_iters.iter().map(|i| format!("diffs/iter-{i}.patch")))
        .map(|path| ArtifactEntry {
            path,
            size_bytes: None,
        })
        .collect()
}

/// List every regular file under a local run prefix, at most 2 directory levels below the root
/// (rel paths of ≤ 3 segments — exactly the shape [`Artifact::parse`]'s general arm serves).
/// Dotfiles, unsafe names, and symlinks are skipped; an unreadable directory yields nothing rather
/// than an error (the manifest is best-effort).
async fn walk_local(root: &std::path::Path) -> Vec<ArtifactEntry> {
    const MAX_SEGMENTS: usize = 3;
    let mut out = Vec::new();
    let mut dirs: Vec<(PathBuf, String, usize)> = vec![(root.to_path_buf(), String::new(), 0)];
    while let Some((dir, rel, depth)) = dirs.pop() {
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(ent)) = rd.next_entry().await {
            let name = ent.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') || !crate::runs::task_evidence::safe_segment(name) {
                continue;
            }
            let child_rel = if rel.is_empty() {
                name.to_string()
            } else {
                format!("{rel}/{name}")
            };
            let Ok(ft) = ent.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                if depth + 1 < MAX_SEGMENTS {
                    dirs.push((ent.path(), child_rel, depth + 1));
                }
            } else if ft.is_file() {
                let size_bytes = ent.metadata().await.ok().map(|m| m.len());
                out.push(ArtifactEntry {
                    path: child_rel,
                    size_bytes,
                });
            }
        }
    }
    out
}

/// Serve one whitelisted artifact for a run. `session_uri` is the run's evidence pointer (`None` when
/// the run recorded none). Metrics label the outcome; the caller has already resolved (and 404'd) an
/// unknown run.
pub(crate) async fn serve_artifact(
    metrics: Option<&Metrics>,
    pool: &sqlx::PgPool,
    session_uri: Option<&str>,
    run_id: &str,
    raw_path: &str,
    if_none_match: Option<&str>,
) -> Response {
    let record = |outcome: &str| {
        if let Some(m) = metrics {
            m.record_artifact_request(outcome);
        }
    };

    let Some(artifact) = Artifact::parse(raw_path) else {
        record("rejected");
        return (
            StatusCode::BAD_REQUEST,
            format!("not a whitelisted artifact path: {raw_path}"),
        )
            .into_response();
    };

    let Some(uri) = session_uri else {
        record("no_evidence");
        return (
            StatusCode::NOT_FOUND,
            format!("run {run_id} has no session evidence"),
        )
            .into_response();
    };
    let Some(prefix) = prefix_of(uri) else {
        record("no_evidence");
        return (
            StatusCode::NOT_FOUND,
            format!("run {run_id} session_uri has no resolvable prefix"),
        )
            .into_response();
    };

    // Fetch: the artifact store for a db:// prefix, local path off the filesystem, s3:// via the
    // engine subprocess into a temp dir.
    let (path, guard) = match fetch_artifact(pool, prefix, &artifact.rel()).await {
        Ok(f) => f,
        Err(FetchError::NotFound) => {
            record("not_found");
            return (
                StatusCode::NOT_FOUND,
                format!("artifact not found: {run_id}/{}", artifact.rel()),
            )
                .into_response();
        }
        Err(FetchError::Fetch(msg)) => {
            record("fetch_error");
            tracing::warn!(run_id, artifact = %artifact.rel(), error = %msg, "artifact fetch failed");
            return (StatusCode::BAD_GATEWAY, "failed to fetch artifact").into_response();
        }
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            record("fetch_error");
            tracing::warn!(run_id, artifact = %artifact.rel(), error = %e, "opening fetched artifact failed");
            return (StatusCode::BAD_GATEWAY, "failed to fetch artifact").into_response();
        }
    };

    // Content-derived ETag: records are immutable by contract, but backfills (flow renders added
    // to old records) do happen, and a name-derived tag left every browser stale until cache
    // expiry. The hash costs one extra read of a file already on local disk.
    let mut file = file;
    let etag = match content_etag(&mut file).await {
        Ok(t) => t,
        Err(e) => {
            record("fetch_error");
            tracing::warn!(run_id, artifact = %artifact.rel(), error = %e, "hashing artifact failed");
            return (StatusCode::BAD_GATEWAY, "failed to fetch artifact").into_response();
        }
    };
    if if_none_match.is_some_and(|inm| inm == etag) {
        record("not_modified");
        return StatusCode::NOT_MODIFIED.into_response();
    }

    record("ok");
    let body = Body::from_stream(GuardedStream {
        inner: ReaderStream::new(file),
        _guard: guard,
    });
    let mut resp = body.into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(artifact.content_type()),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE),
    );
    if let Some(csp) = artifact.csp() {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(csp),
        );
    }
    if let Ok(v) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, v);
    }
    resp
}

/// Strong ETag from the artifact bytes, leaving the file handle rewound for streaming.
async fn content_etag(file: &mut tokio::fs::File) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    file.seek(std::io::SeekFrom::Start(0)).await?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(34);
    hex.push('"');
    for b in &digest[..16] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex.push('"');
    Ok(hex)
}

/// Why a fetch didn't yield a file: the object doesn't exist (→ 404) vs. a transport/subprocess
/// failure (→ 502).
pub(crate) enum FetchError {
    NotFound,
    Fetch(String),
}

/// Materialize one artifact under `prefix` as a local file: a `db://run-session/…` prefix reads
/// the stored session out of the artifact tables into a temp dir, local paths resolve in place,
/// `s3://` goes through the engine subprocess into a temp dir. The `TempDir` guard must outlive
/// every read of the returned path.
pub(crate) async fn fetch_artifact(
    pool: &sqlx::PgPool,
    prefix: &str,
    rel: &str,
) -> Result<(PathBuf, Option<tempfile::TempDir>), FetchError> {
    if let Some(run_id) = crate::runs::blob_store::run_id_of_session_uri(prefix) {
        fetch_db_session(pool, run_id, rel).await
    } else if prefix.starts_with("s3://") {
        fetch_s3(prefix, rel).await
    } else {
        fetch_local(prefix, rel).await
    }
}

/// Materialize a store-backed run session to a temp file (the flow render shells a subprocess over
/// a path, and the proxy streams a file). Only `session.jsonl` exists under a `db://` prefix.
async fn fetch_db_session(
    pool: &sqlx::PgPool,
    run_id: &str,
    rel: &str,
) -> Result<(PathBuf, Option<tempfile::TempDir>), FetchError> {
    if rel != "session.jsonl" {
        return Err(FetchError::NotFound);
    }
    let session = crate::runs::blob_store::get_run_session(pool, run_id)
        .await
        .map_err(|e| FetchError::Fetch(format!("reading stored session for {run_id}: {e:#}")))?
        .ok_or(FetchError::NotFound)?;
    let dir = tempfile::tempdir().map_err(|e| FetchError::Fetch(format!("temp dir: {e}")))?;
    let path = dir.path().join("session.jsonl");
    tokio::fs::write(&path, session)
        .await
        .map_err(|e| FetchError::Fetch(format!("materializing stored session: {e}")))?;
    Ok((path, Some(dir)))
}

/// Resolve a local-path artifact (dev `session_uri`). The whitelisted `rel` has no traversal
/// component, so the join stays under the run directory.
async fn fetch_local(
    prefix: &str,
    rel: &str,
) -> Result<(PathBuf, Option<tempfile::TempDir>), FetchError> {
    let path = PathBuf::from(prefix).join(rel);
    match tokio::fs::metadata(&path).await {
        Ok(_) => Ok((path, None)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FetchError::NotFound),
        Err(e) => Err(FetchError::Fetch(format!("stat {}: {e}", path.display()))),
    }
}

/// Download an `s3://` artifact into a temp dir by shelling `crucible fetch`. The temp dir rides
/// back with the path so it lives exactly as long as the caller needs the bytes.
async fn fetch_s3(
    prefix: &str,
    rel: &str,
) -> Result<(PathBuf, Option<tempfile::TempDir>), FetchError> {
    let full_uri = format!("{prefix}{rel}");
    let dir = tempfile::tempdir().map_err(|e| FetchError::Fetch(format!("temp dir: {e}")))?;
    let out = dir.path().join("artifact");
    let bin = crate::runs::engine::resolve_bin();
    crate::runs::workpod::admit_contract(
        crate::runs::contract::RequestKind::Fetch,
        &[crate::runs::contract::DispatchTarget::Binary(bin.clone())],
    )
    .await
    .map_err(|e| FetchError::Fetch(e.to_string()))?;

    let status = tokio::process::Command::new(&bin)
        .arg("fetch")
        .arg("--uri")
        .arg(&full_uri)
        .arg("--out")
        .arg(&out)
        .output()
        .await
        .map_err(|e| FetchError::Fetch(format!("spawning `{} fetch`: {e}", bin.display())))?;

    if !status.status.success() {
        // GetObject on a missing key exits nonzero; treat any failure as unavailable rather than
        // trying to distinguish 404 from a transport error off stderr text. A missing local temp
        // file below would otherwise mask it, so surface the fetch failure directly.
        let stderr = String::from_utf8_lossy(&status.stderr);
        return Err(FetchError::Fetch(format!(
            "`crucible fetch {full_uri}` exited {:?}: {}",
            status.status.code(),
            stderr.trim()
        )));
    }

    match tokio::fs::metadata(&out).await {
        Ok(_) => Ok((out, Some(dir))),
        Err(e) => Err(FetchError::Fetch(format!("fetched artifact missing: {e}"))),
    }
}

/// A byte stream that keeps a temp-dir guard alive for the stream's whole life, so an `s3://` fetch's
/// downloaded temp file isn't reaped mid-response. For a local artifact the guard is `None`.
struct GuardedStream {
    inner: ReaderStream<tokio::fs::File>,
    _guard: Option<tempfile::TempDir>,
}

impl Stream for GuardedStream {
    type Item = std::io::Result<axum::body::Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // `ReaderStream<File>` and `Option<TempDir>` are both `Unpin`, so pinning through is safe.
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn whitelist_accepts_exactly_the_named_set_and_safe_diffs() {
        assert_eq!(Artifact::parse("session.jsonl"), Some(Artifact::Session));
        assert_eq!(Artifact::parse("RESULTS.md"), Some(Artifact::Results));
        assert_eq!(Artifact::parse("summary.json"), Some(Artifact::Summary));
        assert_eq!(Artifact::parse("flow.json"), Some(Artifact::FlowJson));
        assert_eq!(Artifact::parse("flow.html"), Some(Artifact::FlowHtml));
        assert_eq!(
            Artifact::parse("diffs/iter-3.diff"),
            Some(Artifact::Diff("iter-3.diff".to_string()))
        );
    }

    #[test]
    fn whitelist_rejects_traversal_dotfiles_and_bad_diffs() {
        for bad in [
            "",
            ".",
            "..",
            "/session.jsonl",
            "session.jsonl/",
            "../session.jsonl",
            "../../etc/passwd",
            "summary.json/..",
            "diffs",
            "diffs/",
            "diffs/..",
            "diffs/.",
            "diffs//x",
            "diffs/../secret",
            "diffs/sub/dir.diff",
            "diffs/a/b",
            "diffs/x/..",
            "RESULTS.md/../summary.json",
            "\0",
            "diffs/a\0b",
            ".hidden",
            ".env",
            "codegen-out/.DS_Store",
            "a/.git/config",
            "a/b/c/d",
            "a//b",
            "a\\b",
            "a/b\\c",
            "codegen-out/..",
            "codegen-out/../../etc/passwd",
        ] {
            assert_eq!(Artifact::parse(bad), None, "must reject {bad:?}");
        }
    }

    #[test]
    fn general_arm_accepts_safe_relative_paths_with_typed_content() {
        for (path, want_ct) in [
            ("notes.txt", "text/plain"),
            ("run.log", "text/plain"),
            ("codegen-out/report.md", "text/markdown"),
            ("codegen-out/data.json", "application/json"),
            ("codegen-out/trace.jsonl", "application/x-ndjson"),
            (
                "codegen-out/deck.pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("codegen-out/table.csv", "text/csv"),
            ("codegen-out/plots/latency.png", "image/png"),
            ("codegen-out/plots/latency.svg", "image/svg+xml"),
            ("codegen-out/report.html", "text/html; charset=utf-8"),
            ("out.patch", "text/plain"),
            ("bench.diff", "text/plain"),
            ("mystery.bin", "application/octet-stream"),
            ("no-extension", "application/octet-stream"),
            ("FLOW.HTML", "text/html; charset=utf-8"),
        ] {
            let parsed = Artifact::parse(path);
            assert_eq!(
                parsed,
                Some(Artifact::General(path.to_string())),
                "must accept {path:?}"
            );
            assert_eq!(
                parsed.expect("parsed").content_type(),
                want_ct,
                "content-type for {path}"
            );
        }
    }

    #[test]
    fn general_html_and_svg_are_sandboxed_others_are_not() {
        for sandboxed in ["codegen-out/report.html", "plot.svg", "FLOW.HTML"] {
            let a = Artifact::parse(sandboxed).expect("parse");
            assert_eq!(a.csp(), Some("sandbox allow-scripts"), "{sandboxed}");
        }
        for plain in ["notes.txt", "codegen-out/plots/latency.png", "mystery.bin"] {
            let a = Artifact::parse(plain).expect("parse");
            assert_eq!(a.csp(), None, "{plain}");
        }
    }

    #[test]
    fn prefix_of_strips_the_filename() {
        assert_eq!(
            prefix_of("s3://bucket/runs/goal/rid/session.jsonl"),
            Some("s3://bucket/runs/goal/rid/")
        );
        assert_eq!(prefix_of("/data/state/session.jsonl"), Some("/data/state/"));
        assert_eq!(prefix_of("session.jsonl"), None);
    }

    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn serves_the_whitelisted_set_from_a_local_session_uri_dir(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("session.jsonl"), "{\"a\":1}\n").expect("w");
        std::fs::write(dir.path().join("RESULTS.md"), "# results").expect("w");
        std::fs::write(dir.path().join("summary.json"), "{\"ok\":true}").expect("w");
        std::fs::write(dir.path().join("flow.json"), "{\"iterations\":[]}").expect("w");
        std::fs::write(dir.path().join("flow.html"), "<!doctype html>flow").expect("w");
        std::fs::create_dir(dir.path().join("diffs")).expect("mk");
        std::fs::write(dir.path().join("diffs").join("one.diff"), "@@ diff").expect("w");
        // session_uri points at the session.jsonl file; siblings resolve off its prefix.
        let uri = dir
            .path()
            .join("session.jsonl")
            .to_string_lossy()
            .to_string();

        for (path, want_ct, want_body) in [
            ("session.jsonl", "application/x-ndjson", "{\"a\":1}\n"),
            ("RESULTS.md", "text/markdown", "# results"),
            ("summary.json", "application/json", "{\"ok\":true}"),
            ("flow.json", "application/json", "{\"iterations\":[]}"),
            (
                "flow.html",
                "text/html; charset=utf-8",
                "<!doctype html>flow",
            ),
            ("diffs/one.diff", "text/plain", "@@ diff"),
        ] {
            let resp = serve_artifact(None, &pool, Some(&uri), "run-1", path, None).await;
            assert_eq!(resp.status(), StatusCode::OK, "serving {path}");
            let ct = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            assert_eq!(ct, want_ct, "content-type for {path}");
            let cache = resp
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            assert_eq!(cache, IMMUTABLE_CACHE, "cache-control for {path}");
            assert_eq!(body_string(resp).await, want_body, "body for {path}");
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn flow_html_is_sandboxed_and_other_artifacts_are_not(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("flow.html"), "<!doctype html>").expect("w");
        std::fs::write(dir.path().join("summary.json"), "{}").expect("w");
        let uri = dir
            .path()
            .join("session.jsonl")
            .to_string_lossy()
            .to_string();

        let resp = serve_artifact(None, &pool, Some(&uri), "run-1", "flow.html", None).await;
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(csp, "sandbox allow-scripts");

        let resp = serve_artifact(None, &pool, Some(&uri), "run-1", "summary.json", None).await;
        assert!(
            resp.headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_none()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn rejects_traversal_before_touching_the_filesystem(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("secret"), "top secret").expect("w");
        let uri = dir
            .path()
            .join("session.jsonl")
            .to_string_lossy()
            .to_string();
        let resp = serve_artifact(None, &pool, Some(&uri), "run-1", "diffs/../secret", None).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn missing_local_artifact_is_404_not_500(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        let uri = dir
            .path()
            .join("session.jsonl")
            .to_string_lossy()
            .to_string();
        let resp = serve_artifact(None, &pool, Some(&uri), "run-1", "summary.json", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn no_session_uri_is_404(pool: sqlx::PgPool) {
        let resp = serve_artifact(None, &pool, None, "run-1", "session.jsonl", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn serves_a_general_nested_artifact_from_a_local_prefix(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("codegen-out").join("plots")).expect("mk");
        std::fs::write(
            dir.path()
                .join("codegen-out")
                .join("plots")
                .join("latency.svg"),
            "<svg/>",
        )
        .expect("w");
        let uri = dir
            .path()
            .join("session.jsonl")
            .to_string_lossy()
            .to_string();

        let resp = serve_artifact(
            None,
            &pool,
            Some(&uri),
            "run-1",
            "codegen-out/plots/latency.svg",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(ct, "image/svg+xml");
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(csp, "sandbox allow-scripts");
        assert_eq!(body_string(resp).await, "<svg/>");
    }

    #[tokio::test]
    async fn manifest_for_s3_prefix_is_the_derived_list_with_null_sizes() {
        let m = build_manifest("s3://bucket/runs/goal/rid/", &[1, 3]).await;
        let paths: Vec<&str> = m.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "RESULTS.md",
                "diffs/iter-1.patch",
                "diffs/iter-3.patch",
                "flow.html",
                "flow.json",
                "session.jsonl",
                "summary.json",
            ]
        );
        assert!(m.entries.iter().all(|e| e.size_bytes.is_none()));
    }

    #[tokio::test]
    async fn manifest_for_local_prefix_walks_files_with_sizes_and_depth_cap() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("session.jsonl"), "{}\n").expect("w");
        std::fs::write(dir.path().join("RESULTS.md"), "# hi").expect("w");
        std::fs::write(dir.path().join(".hidden"), "no").expect("w");
        std::fs::create_dir(dir.path().join("diffs")).expect("mk");
        std::fs::write(dir.path().join("diffs").join("iter-1.patch"), "@@").expect("w");
        std::fs::create_dir_all(dir.path().join("codegen-out").join("plots")).expect("mk");
        std::fs::write(
            dir.path()
                .join("codegen-out")
                .join("plots")
                .join("latency.png"),
            [0u8; 4],
        )
        .expect("w");
        // Over depth (4 segments): must not be listed.
        std::fs::create_dir_all(dir.path().join("a").join("b").join("c")).expect("mk");
        std::fs::write(
            dir.path().join("a").join("b").join("c").join("deep.txt"),
            "x",
        )
        .expect("w");
        // Dot-directory: skipped entirely, contents never listed.
        std::fs::create_dir(dir.path().join(".git")).expect("mk");
        std::fs::write(dir.path().join(".git").join("config"), "x").expect("w");

        let prefix = format!("{}/", dir.path().to_string_lossy());
        let m = build_manifest(&prefix, &[1]).await;
        let paths: Vec<&str> = m.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "RESULTS.md",
                "codegen-out/plots/latency.png",
                "diffs/iter-1.patch",
                "session.jsonl",
            ]
        );
        for e in &m.entries {
            assert!(e.size_bytes.is_some(), "size for {}", e.path);
        }
        let png = m
            .entries
            .iter()
            .find(|e| e.path == "codegen-out/plots/latency.png")
            .expect("png entry");
        assert_eq!(png.size_bytes, Some(4));
        // Every listed path must be servable by the proxy's whitelist.
        for e in &m.entries {
            assert!(Artifact::parse(&e.path).is_some(), "servable {}", e.path);
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn serves_a_store_backed_session_and_404s_its_siblings(pool: sqlx::PgPool) {
        let session = "{\"v\":1,\"kind\":\"shutdown\",\"outcome\":\"finished\"}\n";
        crate::runs::blob_store::put_run_session(&pool, "run-db", session.as_bytes())
            .await
            .expect("store");
        let uri = crate::runs::blob_store::run_session_uri("run-db");

        let resp = serve_artifact(None, &pool, Some(&uri), "run-db", "session.jsonl", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(ct, "application/x-ndjson");
        assert_eq!(body_string(resp).await, session);

        // Only the session exists under a db:// prefix.
        let resp = serve_artifact(None, &pool, Some(&uri), "run-db", "summary.json", None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // A run whose session was never stored is a 404, not a 502.
        let missing = crate::runs::blob_store::run_session_uri("run-none");
        let resp = serve_artifact(
            None,
            &pool,
            Some(&missing),
            "run-none",
            "session.jsonl",
            None,
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // The manifest for a db:// prefix lists exactly the session.
        let m = build_manifest(&crate::runs::blob_store::run_session_uri("run-db"), &[1]).await;
        let paths: Vec<&str> = m.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["session.jsonl"]);
    }

    #[tokio::test]
    async fn manifest_for_missing_local_dir_is_empty() {
        let m = build_manifest("/nonexistent/run/prefix/", &[1]).await;
        assert!(m.entries.is_empty());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn matching_if_none_match_is_304_and_changed_content_busts_it(pool: sqlx::PgPool) {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("summary.json"), "{}").expect("w");
        let uri = dir
            .path()
            .join("session.jsonl")
            .to_string_lossy()
            .to_string();
        let first = serve_artifact(None, &pool, Some(&uri), "run-1", "summary.json", None).await;
        let etag = first
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("etag")
            .to_string();

        let resp = serve_artifact(
            None,
            &pool,
            Some(&uri),
            "run-1",
            "summary.json",
            Some(&etag),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

        // A backfilled/replaced file must produce a different tag, so the stale one misses.
        std::fs::write(dir.path().join("summary.json"), "{\"v\":2}").expect("w");
        let resp = serve_artifact(
            None,
            &pool,
            Some(&uri),
            "run-1",
            "summary.json",
            Some(&etag),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let new_etag = resp
            .headers()
            .get(header::ETAG)
            .and_then(|v| v.to_str().ok())
            .expect("etag")
            .to_string();
        assert_ne!(new_etag, etag);
    }
}
